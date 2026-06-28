// 构建脚本：把 FFmpeg 的运行期 DLL 拷到可执行文件旁边，使 `cargo run` 能直接运行。
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");

    let Ok(ffmpeg_dir) = env::var("FFMPEG_DIR") else {
        return;
    };
    let bin = PathBuf::from(&ffmpeg_dir).join("bin");

    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out  → target/<profile> 是上 3 层
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let Some(target_dir) = out_dir.ancestors().nth(3) else {
        return;
    };

    if let Ok(entries) = fs::read_dir(&bin) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let is_dll = path.extension().and_then(|s| s.to_str()) == Some("dll");
            // 同时带上 ffmpeg.exe（生成全 I 帧代理时用）
            if is_dll || name.eq_ignore_ascii_case("ffmpeg.exe") {
                let _ = fs::copy(&path, target_dir.join(name));
            }
        }
    }
}
