// 代理(proxy)生成：把源视频转码成「全 I 帧」副本，每帧都是关键帧，
// 之后跳任意帧只解 1 帧 → 即时且帧准。一次性后台任务，带进度。

use crossbeam_channel::{Receiver, Sender};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// 让转码进程以「低于正常」优先级运行，给键盘钩子等让出 CPU。
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
/// 不为子进程创建控制台窗口：GUI 程序(无控制台)spawn 控制台版 ffmpeg.exe 时，
/// Windows 默认会弹一个 terminal 窗口；加此标志即可不弹窗。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub enum ProxyMsg {
    Progress(f32), // 0.0..=1.0
    Done(PathBuf),
    Failed(String),
}

/// 打包后 ffmpeg.exe 在可执行文件旁；开发期回退到 third_party。
pub fn ffmpeg_exe() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("ffmpeg.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("third_party/ffmpeg-7.1.1-full_build-shared/bin/ffmpeg.exe");
    dev.is_file().then_some(dev)
}

fn cache_dir() -> Option<PathBuf> {
    let d = directories::ProjectDirs::from("", "", "FramePlayer")?;
    Some(d.cache_dir().join("proxies"))
}

/// 跨进程「最近激活的播放器窗口 PID」标记文件（多窗口全局热键归属用）。
/// 放在缓存根目录（与 proxies/ 同级）。只由 UI 线程读写，钩子回调绝不碰它。
pub fn active_pid_path() -> Option<PathBuf> {
    let d = directories::ProjectDirs::from("", "", "FramePlayer")?;
    Some(d.cache_dir().join("active.pid"))
}

/// 把源路径规范化成「同一物理文件 => 同一字符串」的稳定 key：
/// 解析成绝对路径、去掉 Windows 的 \\?\ verbatim 前缀、统一小写
///（Windows 路径大小写不敏感，盘符大小写/正反斜杠/相对路径差异都会改变哈希，
/// 否则同一文件用不同写法打开会各生成一份代理、反复转码）。
fn canonical_key(source: &Path) -> String {
    let canon = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    let mut s = canon.to_string_lossy().into_owned();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        s = stripped.to_string();
    }
    s.to_lowercase()
}

/// 按「规范化源路径 + 大小 + 修改时间」给该源生成稳定的哈希（代理/续播记录共用）。
fn file_hash(source: &Path) -> Option<u64> {
    let meta = std::fs::metadata(source).ok()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canonical_key(source).hash(&mut h);
    meta.len().hash(&mut h);
    modified.hash(&mut h);
    Some(h.finish())
}

/// 该源对应的全 I 帧代理文件名。
pub fn proxy_path_for(source: &Path) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("proxy-{:016x}.mp4", file_hash(source)?)))
}

fn resume_dir() -> Option<PathBuf> {
    Some(cache_dir()?.parent()?.join("resume"))
}

/// 读取该源「上次播放到第几帧」。无记录返回 None。
pub fn load_resume(source: &Path) -> Option<u64> {
    let p = resume_dir()?.join(format!("{:016x}.txt", file_hash(source)?));
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// 记录该源当前播放到第几帧（每个文件一个小文件，多窗口下最后写入者获胜）。
pub fn save_resume(source: &Path, frame: u64) {
    let Some(h) = file_hash(source) else { return };
    let Some(dir) = resume_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{h:016x}.txt")), frame.to_string());
}

/// 已存在且非空的代理视为可用。
pub fn existing_proxy(source: &Path) -> Option<PathBuf> {
    let p = proxy_path_for(source)?;
    match std::fs::metadata(&p) {
        Ok(m) if m.len() > 0 => Some(p),
        _ => None,
    }
}

/// 把代理缓存目录压到 `max_mb` MiB 以内：按修改时间从旧到新删，直到达标。
/// 启动时、每次转码成功后、以及在设置里调小上限时各调用一次。0 = 不限制。
/// 只在 UI 线程调用——绝不能在键盘钩子回调里碰文件系统。
pub fn evict_cache(max_mb: u64, keep: Option<&Path>) {
    let Some(dir) = cache_dir() else { return };
    evict_cache_in(&dir, max_mb, keep);
}

fn evict_cache_in(dir: &Path, max_mb: u64, keep: Option<&Path>) {
    if max_mb == 0 {
        return;
    }
    let max_bytes = max_mb.saturating_mul(1024 * 1024);

    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("mp4") {
            continue;
        }
        // 跳过正在写入的 .part.mp4（双扩展名，stem 仍以 .part 结尾）
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.ends_with(".part"))
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        total = total.saturating_add(size);
        entries.push((path, size, mtime));
    }
    if total <= max_bytes {
        return;
    }
    entries.sort_by_key(|(_, _, m)| *m); // 最旧的排前面
    for (path, size, _) in entries {
        if total <= max_bytes {
            break;
        }
        // 正在使用或准备打开的代理允许暂时超出上限，不能在交给解码线程前删掉。
        if keep == Some(path.as_path()) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                total = total.saturating_sub(size);
                log::info!("代理缓存淘汰: {path:?}");
            }
            Err(e) => log::warn!("代理缓存删除失败 {path:?}: {e}"),
        }
    }
}

/// 后台转码为全 I 帧 H.264。`total_frames` 用于算百分比。`cancel` 置位即中止。
pub fn spawn_transcode(
    ffmpeg: PathBuf,
    source: PathBuf,
    dest: PathBuf,
    total_frames: u64,
    ctx: egui::Context,
    cancel: Arc<AtomicBool>,
) -> (Receiver<ProxyMsg>, std::thread::JoinHandle<()>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let worker = std::thread::Builder::new()
        .name("proxy".into())
        .spawn(move || run(&tx, &ffmpeg, &source, &dest, total_frames, &ctx, &cancel))
        .expect("spawn proxy thread");
    (rx, worker)
}

static TRANSCODE_ID: AtomicU64 = AtomicU64::new(0);

fn temporary_path(dest: &Path) -> PathBuf {
    let id = TRANSCODE_ID.fetch_add(1, Ordering::Relaxed);
    dest.with_extension(format!("{}-{id}.part.mp4", std::process::id()))
}

/// 始终等待子进程退出后再清理，Windows 上仍在写入的文件无法删除。
fn wait_for_transcode(child: &mut Child, cancel: &AtomicBool) -> std::io::Result<Option<ExitStatus>> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            child.wait()?;
            return Ok(None);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
    }
}

fn run(
    tx: &Sender<ProxyMsg>,
    ffmpeg: &Path,
    source: &Path,
    dest: &Path,
    total_frames: u64,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 每个进程、每次任务各用一份临时文件，避免多窗口转码互相覆盖/删除。
    let tmp = temporary_path(dest);

    // 只用一半核心，给键盘钩子/UI 留出 CPU，避免转码期间全局快捷键被饿死。
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let threads = (cores / 2).max(2);

    let mut child = match Command::new(ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(source)
        .args([
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "18",
            "-x264-params",
            "keyint=1:scenecut=0",
            "-fps_mode",
            "passthrough",
            "-threads",
        ])
        .arg(threads.to_string())
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(&tmp)
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(ProxyMsg::Failed(format!("启动 ffmpeg 失败: {e}")));
            ctx.request_repaint();
            return;
        }
    };

    let (status, stderr_tail) = monitor_transcode(&mut child, tx, total_frames, ctx, cancel);
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&tmp);
        return;
    }

    match status {
        Ok(None) => {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        Ok(Some(s)) if s.success() => match std::fs::rename(&tmp, dest) {
            Ok(()) => {
                let _ = tx.send(ProxyMsg::Done(dest.to_path_buf()));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                // 另一个窗口可能已经发布了同一份代理；只会看到完整的最终文件。
                if std::fs::metadata(dest).is_ok_and(|m| m.is_file() && m.len() > 0) {
                    let _ = tx.send(ProxyMsg::Done(dest.to_path_buf()));
                } else {
                    let _ = tx.send(ProxyMsg::Failed(format!("重命名代理失败: {e}")));
                }
            }
        },
        Ok(Some(s)) => {
            let _ = std::fs::remove_file(&tmp);
            let _ = tx.send(ProxyMsg::Failed(format!("ffmpeg 退出码 {s}: {stderr_tail}")));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            let _ = tx.send(ProxyMsg::Failed(format!("等待 ffmpeg 失败: {e}")));
        }
    }
    ctx.request_repaint();
}

fn monitor_transcode(
    child: &mut Child,
    tx: &Sender<ProxyMsg>,
    total_frames: u64,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> (std::io::Result<Option<ExitStatus>>, String) {
    // 两路输出必须同时读取：stderr 管道写满时 FFmpeg 会阻塞，也就不会再输出进度。
    // 取消检测独立于输出，避免卡在读取下一行、迟迟无法停止转码。
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(v) = line.strip_prefix("frame=")
                        && let Ok(f) = v.trim().parse::<u64>()
                    {
                        let p = if total_frames > 0 {
                            (f as f32 / total_frames as f32).clamp(0.0, 0.999)
                        } else {
                            0.0
                        };
                        let _ = tx.send(ProxyMsg::Progress(p));
                        ctx.request_repaint();
                    }
                }
            }
        });
        let errors = scope.spawn(|| {
            let mut tail = std::collections::VecDeque::new();
            if let Some(stderr) = stderr {
                // FFmpeg 的诊断在 Windows 上不一定是 UTF-8；替换无效字节后继续排空管道。
                for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
                    if tail.len() == 2 {
                        tail.pop_front();
                    }
                    tail.push_back(String::from_utf8_lossy(&line).trim().to_owned());
                }
            }
            tail.into_iter().collect::<Vec<_>>().join(" | ")
        });
        let status = wait_for_transcode(child, cancel);
        (status, errors.join().unwrap_or_default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestDir, ffmpeg, make_video};
    use std::io::Write;
    use std::time::{Duration, Instant};

    #[test]
    fn cache_eviction_keeps_the_active_proxy_and_partial_files() {
        let dir = TestDir::new();
        let old = dir.0.join("old.mp4");
        let active = dir.0.join("active.mp4");
        let partial = temporary_path(&active);
        for path in [&old, &active, &partial] {
            std::fs::File::create(path)
                .unwrap()
                .set_len(2 * 1024 * 1024)
                .unwrap();
        }
        evict_cache_in(&dir.0, 1, Some(&active));
        assert!(!old.exists());
        assert!(active.exists());
        assert!(partial.exists());
        evict_cache_in(&dir.0, 1, None);
        assert!(!active.exists());
        assert!(partial.exists());
    }

    #[test]
    fn simultaneous_transcodes_publish_a_complete_proxy() {
        let dir = TestDir::new();
        let source = dir.0.join("source.mp4");
        let dest = dir.0.join("proxy.mp4");
        make_video(&source, "libx264");
        let ffmpeg = ffmpeg();
        let ctx = egui::Context::default();
        let cancel = AtomicBool::new(false);
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| run(&tx, &ffmpeg, &source, &dest, 48, &ctx, &cancel));
            }
        });
        let mut completed = 0;
        for msg in rx.try_iter() {
            match msg {
                ProxyMsg::Done(path) => {
                    assert_eq!(path, dest);
                    completed += 1;
                }
                ProxyMsg::Failed(e) => panic!("{e}"),
                ProxyMsg::Progress(_) => {}
            }
        }
        assert_eq!(completed, 2);
        assert_eq!(
            std::fs::read_dir(&dir.0).unwrap().count(),
            2,
            "不应残留临时文件"
        );
        let report = crate::decode::selftest(&dest).unwrap();
        assert!(
            report.contains("total=48") && report.contains("keyframes=48"),
            "{report}"
        );
    }

    // 用测试程序自身模拟 FFmpeg 输出；仅由下面的测试作为子进程启动。
    #[test]
    #[ignore = "由转码进程管理测试显式启动"]
    fn subprocess_output_fixture() {
        match std::env::var("FRAME_PLAYER_TEST_CHILD_MODE")
            .unwrap()
            .as_str()
        {
            "flood" => {
                // 超过 Windows 管道容量，并包含非 UTF-8 诊断。
                std::io::stderr()
                    .write_all(&vec![b'x'; 256 * 1024])
                    .unwrap();
                std::io::stderr()
                    .write_all(b"\n\xff\nlast error\n")
                    .unwrap();
                std::io::stdout().write_all(b"frame=48\n").unwrap();
            }
            "silent" => std::thread::sleep(Duration::from_secs(10)),
            _ => panic!("unknown fixture mode"),
        }
    }

    #[test]
    fn transcode_drains_stderr_and_can_cancel_without_progress() {
        for mode in ["flood", "silent"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "proxy::tests::subprocess_output_fixture",
                    "--nocapture",
                ])
                .env("FRAME_PLAYER_TEST_CHILD_MODE", mode)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .unwrap();
            let cancel = AtomicBool::new(false);
            let ctx = egui::Context::default();
            let (tx, rx) = crossbeam_channel::unbounded();
            let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
            let start = Instant::now();
            let (status, tail) = std::thread::scope(|scope| {
                scope.spawn(|| {
                    let timeout = if mode == "silent" {
                        Duration::from_millis(200)
                    } else {
                        Duration::from_secs(5)
                    };
                    if done_rx.recv_timeout(timeout).is_err() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                });
                let result = monitor_transcode(&mut child, &tx, 48, &ctx, &cancel);
                let _ = done_tx.send(());
                result
            });
            if mode == "flood" {
                assert!(status.unwrap().is_some_and(|s| s.success()), "{tail}");
                assert!(tail.contains("last error"));
                assert!(
                    rx.try_iter()
                        .any(|msg| matches!(msg, ProxyMsg::Progress(p) if p > 0.9))
                );
            } else {
                assert!(status.unwrap().is_none());
                assert!(start.elapsed() < Duration::from_secs(3));
            }
            assert!(child.try_wait().unwrap().is_some());
        }
    }
}
