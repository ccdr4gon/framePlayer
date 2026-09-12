use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct TestDir(pub PathBuf);

impl TestDir {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts");
        if let Ok(root) = root.canonicalize()
            && self.0.starts_with(&root)
            && self.0 != root
        {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

pub fn ffmpeg() -> PathBuf {
    crate::proxy::ffmpeg_exe()
        .or_else(|| option_env!("FFMPEG_DIR").map(|dir| PathBuf::from(dir).join("bin/ffmpeg.exe")))
        .expect("测试需要 FFmpeg 开发包中的 ffmpeg.exe")
}

pub fn ffmpeg_output(args: &[&str]) -> Output {
    let output = Command::new(ffmpeg())
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

pub fn make_video(path: &Path, codec: &str) {
    ffmpeg_output(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=96x64:rate=24:duration=2",
        "-c:v",
        codec,
        "-g",
        "12",
        "-threads",
        "2",
        "-y",
        path.to_str().unwrap(),
    ]);
}
