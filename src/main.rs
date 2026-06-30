// Release 构建不弹控制台窗口（GUI 子系统）；debug 仍保留控制台便于看日志/自检。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Frame Player — 逐帧 MP4 播放器
// M0 skeleton: eframe window + control shell + 置顶(钉子) + Tab 沉浸模式。
// 解码 / 全局快捷键将在后续里程碑接入。

mod app;
mod config;
mod decode;
mod input;
mod msg;
mod proxy;

use eframe::egui;

fn init_logging() {
    // 写到 %TEMP%\frameplayer-<pid>.log（每进程一个文件），release 无控制台也能看日志。
    let path = std::env::temp_dir().join(format!("frameplayer-{}.log", std::process::id()));
    // 默认只记 info 级以上；排障时设 RUST_LOG=frame_player=debug 获取更详细日志。
    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("frame_player=info,warn"),
    );
    if let Ok(file) = std::fs::File::create(&path) {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
    let _ = builder.try_init();
    log::info!(
        "=== frameplayer pid={} log -> {} ===",
        std::process::id(),
        path.display()
    );
}

fn main() -> eframe::Result<()> {
    init_logging();

    // 无界面自检： frame_player --selftest <path>
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--selftest") {
        let path = args.get(pos + 1).cloned().unwrap_or_default();
        match decode::selftest(std::path::Path::new(&path)) {
            Ok(s) => {
                println!("{s}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("SELFTEST FAILED: {e}");
                std::process::exit(1);
            }
        }
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([960.0, 600.0])
        .with_min_inner_size([480.0, 320.0])
        .with_decorations(false) // 自绘标题栏；沉浸模式可彻底隐藏（连关闭按钮一起）
        .with_title("Frame Player");
    // 运行时窗口图标（任务栏/Alt-Tab）；固定到任务栏的图标由 build.rs 嵌入的资源提供。
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Frame Player",
        native_options,
        Box::new(|cc| Ok(Box::new(app::FramePlayerApp::new(cc)) as Box<dyn eframe::App>)),
    )
}
