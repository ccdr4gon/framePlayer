// 代理(proxy)生成：把源视频转码成「全 I 帧」副本，每帧都是关键帧，
// 之后跳任意帧只解 1 帧 → 即时且帧准。一次性后台任务，带进度。

use crossbeam_channel::{Receiver, Sender};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 让转码进程以「低于正常」优先级运行，给键盘钩子等让出 CPU。
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

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

/// 按「规范化源路径 + 大小 + 修改时间」给该源生成稳定的代理文件名。
pub fn proxy_path_for(source: &Path) -> Option<PathBuf> {
    let dir = cache_dir()?;
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
    Some(dir.join(format!("proxy-{:016x}.mp4", h.finish())))
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
pub fn evict_cache(max_mb: u64) {
    if max_mb == 0 {
        return;
    }
    let Some(dir) = cache_dir() else { return };
    let max_bytes = max_mb.saturating_mul(1024 * 1024);

    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
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
) -> Receiver<ProxyMsg> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("proxy".into())
        .spawn(move || run(&tx, &ffmpeg, &source, &dest, total_frames, &ctx, &cancel))
        .expect("spawn proxy thread");
    rx
}

fn aborted(cancel: &AtomicBool, child: &mut Child, tmp: &Path) -> bool {
    if cancel.load(Ordering::Relaxed) {
        let _ = child.kill();
        let _ = std::fs::remove_file(tmp);
        true
    } else {
        false
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
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = dest.with_extension("part.mp4");

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
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS)
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

    // 解析 -progress 的 frame=N 行换算百分比
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if aborted(cancel, &mut child, &tmp) {
                return;
            }
            if let Some(v) = line.strip_prefix("frame=") {
                if let Ok(f) = v.trim().parse::<u64>() {
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
    }
    if aborted(cancel, &mut child, &tmp) {
        return;
    }

    let status = child.wait();
    let stderr = child
        .stderr
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            use std::io::Read;
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();

    match status {
        Ok(s) if s.success() => match std::fs::rename(&tmp, dest) {
            Ok(()) => {
                let _ = tx.send(ProxyMsg::Done(dest.to_path_buf()));
            }
            Err(e) => {
                let _ = tx.send(ProxyMsg::Failed(format!("重命名代理失败: {e}")));
            }
        },
        Ok(s) => {
            let _ = std::fs::remove_file(&tmp);
            let tail: String = stderr.lines().rev().take(2).collect::<Vec<_>>().join(" | ");
            let _ = tx.send(ProxyMsg::Failed(format!("ffmpeg 退出码 {s}: {tail}")));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            let _ = tx.send(ProxyMsg::Failed(format!("等待 ffmpeg 失败: {e}")));
        }
    }
    ctx.request_repaint();
}
