// egui App —— 按视觉稿重做的界面：
//   自绘标题栏（沉浸模式可整条隐藏 → 无关闭按钮）、底部悬浮控制条、设置弹层、
//   深色+琥珀扁平主题、自绘窗口边缘缩放。
//   逐帧解码（M1）、正常播放定时器已接入；全局快捷键/按住连播/缓存待后续里程碑。

use crate::config::Config;
use crate::decode::{self, DecoderHandle};
use crate::input::{self, Action, Binding, HookMsg};
use crate::msg::{FromDecoder, ToDecoder};
use crate::proxy::{self, ProxyMsg};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 视觉稿配色（HEX）。带透明度的颜色不能做 const，用函数返回。
mod col {
    use eframe::egui::Color32;
    pub const WIN_BG: Color32 = Color32::from_rgb(0x0E, 0x0E, 0x10);
    pub const TITLEBAR: Color32 = Color32::from_rgb(0x16, 0x16, 0x18);
    pub const PANEL: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x22);
    pub const RAISED: Color32 = Color32::from_rgb(0x26, 0x26, 0x2C);
    pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x30);
    pub const BORDER2: Color32 = Color32::from_rgb(0x34, 0x34, 0x3C);
    pub const TEXT_HI: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF4);
    pub const TEXT: Color32 = Color32::from_rgb(0xC9, 0xC9, 0xCE);
    pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8A, 0x8A, 0x92);
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x5A, 0x5A, 0x62);
    pub const ACCENT: Color32 = Color32::from_rgb(0xE8, 0x9A, 0x3C);
    pub const ACCENT_HI: Color32 = Color32::from_rgb(0xF3, 0xAD, 0x55);
    pub const VIDEO_BLACK: Color32 = Color32::from_rgb(0x04, 0x04, 0x05);
    pub fn bar() -> Color32 {
        Color32::from_rgba_unmultiplied(22, 22, 24, 209)
    }
    pub fn accent_soft() -> Color32 {
        Color32::from_rgba_unmultiplied(0xE8, 0x9A, 0x3C, 41)
    }
}

/// Segoe Fluent Icons / MDL2 码点（用名为 "icons" 的字体族渲染，避免 emoji 缺字）。
mod ico {
    pub const APP: char = '\u{E714}'; // Video
    pub const OPEN: char = '\u{E8E5}'; // OpenFile
    pub const PREV: char = '\u{E892}'; // Previous |◀
    pub const PLAY: char = '\u{E768}';
    pub const PAUSE: char = '\u{E769}';
    pub const NEXT: char = '\u{E893}'; // Next ▶|
    pub const PIN: char = '\u{E718}';
    pub const ADD: char = '\u{E710}'; // 新建窗口 +
    pub const SETTINGS: char = '\u{E713}';
    pub const MIN: char = '\u{E921}'; // ChromeMinimize
    pub const MAX: char = '\u{E922}'; // ChromeMaximize
    pub const RESTORE: char = '\u{E923}'; // ChromeRestore
    pub const CLOSE: char = '\u{E8BB}'; // ChromeClose
}

/// 用 "icons" 字体族构造一段图标文本。
fn icon_rt(ch: char, size: f32, color: egui::Color32) -> egui::RichText {
    egui::RichText::new(ch.to_string())
        .family(egui::FontFamily::Name("icons".into()))
        .size(size)
        .color(color)
}

/// 自绘标题栏高度（与 Panel::top 的 exact_size 一致）。
const TITLE_BAR_H: f32 = 38.0;

pub struct FramePlayerApp {
    decoder: DecoderHandle,
    hook_rx: crossbeam_channel::Receiver<HookMsg>,
    tex: Option<egui::TextureHandle>,

    /// 当前快捷键绑定（UI 显示/编辑用的本地副本，与 input::KEYMAP 同步）
    bindings: Vec<Binding>,
    /// 正在为哪个动作捕获新按键（None=未在捕获）
    capturing: Option<Action>,

    file: Option<PathBuf>,
    current_frame: u64,
    total_frames: u64,
    fps: f64,
    vid_w: u32,
    vid_h: u32,
    status: String,
    /// 是否有一帧请求在途（用于把连播节奏限制在解码速度内，避免堆积）
    pending: bool,
    /// 在途请求是否为「拖动预览(关键帧)」
    pending_preview: bool,
    /// 积压的最新请求 (目标帧, 是否预览)，上一帧完成后再发出（合并，丢弃中间请求）
    pending_req: Option<(u64, bool)>,
    /// 当前显示的是否为预览(关键帧)画面（用于松手时强制请求精确帧）
    current_is_preview: bool,

    play_interval_s: f64,
    hold_interval_s: f64,
    playing: bool,
    last_advance: f64,
    /// 全局键按住连播方向：Some(-1)=上一帧, Some(1)=下一帧
    holding: Option<i64>,
    /// 正在按住的那个键的 vk（用于失效保护轮询物理键状态）
    holding_vk: Option<u32>,
    hold_last: f64,

    always_on_top: bool,
    immersive: bool,
    show_settings: bool,

    /// 用户打开的原始源文件（代理基于它生成）
    source_file: Option<PathBuf>,
    /// 当前解码的是否为「全 I 帧代理」（跳转即时）
    using_proxy: bool,
    /// 代理转码进度通道（生成中为 Some）
    proxy_rx: Option<crossbeam_channel::Receiver<ProxyMsg>>,
    proxy_progress: Option<f32>,
    /// 取消正在进行的代理转码（打开新文件时置位）
    proxy_cancel: Option<Arc<AtomicBool>>,
    /// 切换到代理后要恢复到的帧
    restore_frame: Option<u64>,
    /// 打开后待执行「按视频比例自适应窗口」
    fit_pending: bool,

    /// 上一帧观测到的尺寸（判断用户在拖哪一维做比例锁定缩放）
    prev_size: Option<egui::Vec2>,
    /// 进入沉浸的时刻（用于提示渐隐）
    immersive_since: f64,

    /// 代理缓存上限（MiB），超出按最旧淘汰；0 = 不限
    proxy_cache_max_mb: u64,
    /// 本窗口的 Win32 HWND（去圆角 + 判断是否前台窗口）
    hwnd: Option<isize>,
    /// 上一帧本窗口是否为前台（检测 false→true 边沿 → 写 active.pid）
    prev_focused: bool,
    /// 上次读取 active.pid 更新 IS_ACTIVE 的时刻（节流 ~150ms）
    last_active_poll: f64,
    /// 在此 UI 时间前持续重绘（双击采样窗口、缩放后等），避免反应式空闲丢事件
    repaint_until: f64,
}

impl FramePlayerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        setup_theme(&cc.egui_ctx);
        let hwnd = window_hwnd(cc);
        if let Some(h) = hwnd {
            square_window_corners(h); // 关掉 Win11 DWM 圆角，沉浸/正常模式都是尖角
        }
        let decoder = decode::spawn(cc.egui_ctx.clone());
        let hook_rx = input::spawn_hook(cc.egui_ctx.clone());

        // 载入配置（速度 + 自定义快捷键），并同步到全局钩子的键位表。
        let config = Config::load();
        input::set_bindings(config.bindings.clone());

        // 启动即把自己写成「激活窗口」：单窗口/刚启动也能立即响应全局热键。
        if let Some(p) = proxy::active_pid_path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&p, std::process::id().to_string());
        }

        let mut app = Self {
            decoder,
            hook_rx,
            tex: None,
            bindings: config.bindings,
            capturing: None,
            file: None,
            current_frame: 0,
            total_frames: 0,
            fps: 0.0,
            vid_w: 0,
            vid_h: 0,
            status: String::new(),
            pending: false,
            pending_preview: false,
            pending_req: None,
            current_is_preview: false,
            play_interval_s: config.play_interval_s,
            hold_interval_s: config.hold_interval_s,
            playing: false,
            last_advance: 0.0,
            holding: None,
            holding_vk: None,
            hold_last: 0.0,
            always_on_top: false,
            immersive: false,
            show_settings: false,
            source_file: None,
            using_proxy: false,
            proxy_rx: None,
            proxy_progress: None,
            proxy_cancel: None,
            restore_frame: None,
            fit_pending: false,
            prev_size: None,
            immersive_since: 0.0,
            proxy_cache_max_mb: config.proxy_cache_max_mb,
            hwnd,
            prev_focused: false,
            last_active_poll: 0.0,
            repaint_until: 0.0,
        };
        // 命令行打开：走 open_path 以便自动用/生成代理
        if let Some(arg) = std::env::args().skip(1).find(|a| !a.starts_with("--")) {
            let p = PathBuf::from(&arg);
            if p.is_file() {
                app.open_path(p);
            }
        }
        // 启动时按上限压一次代理缓存（清理历史/孤立的代理）
        proxy::evict_cache(app.proxy_cache_max_mb);
        app
    }

    /// 本窗口是否为系统前台窗口。用 GetForegroundWindow 比 egui 的 i.focused 更可靠
    /// （无边框窗口下 i.focused 可能不准）。
    fn is_foreground(&self) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        match self.hwnd {
            Some(h) => unsafe { GetForegroundWindow().0 as isize == h },
            None => false,
        }
    }

    /// 多窗口全局热键归属：本窗口在前台时，它就是「最近激活」者——始终激活，并(节流)把
    /// 自己写进 active.pid，这样别的窗口启动/获焦改写过该文件后也能立刻夺回。非前台时则
    /// 节流读取 active.pid 判断本窗口是否仍是最近激活者。钩子回调只读 IS_ACTIVE，绝不碰文件。
    fn update_active_window(&mut self, ctx: &egui::Context) {
        let fg = self.is_foreground();
        let now = ctx.input(|i| i.time);
        // 仅在前台状态变化或每 ~150ms 才动一次：set_active 会唤醒热键线程重新对账，
        // 节流后既能及时夺回热键、又能定期重试注册（多窗口切换时另一窗口刚放手）。
        let edge = fg != self.prev_focused;
        if edge || now - self.last_active_poll >= 0.150 {
            self.last_active_poll = now;
            if fg {
                input::set_active(true);
                if let Some(p) = proxy::active_pid_path() {
                    let _ = std::fs::write(&p, std::process::id().to_string());
                }
            } else if let Some(p) = proxy::active_pid_path() {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    let mine = s.trim().parse::<u32>().ok() == Some(std::process::id());
                    input::set_active(mine);
                }
            }
            if edge {
                log::debug!("foreground={fg}");
            }
        }
        self.prev_focused = fg;
        ctx.request_repaint_after(std::time::Duration::from_millis(160));
    }

    fn set_always_on_top(&mut self, ctx: &egui::Context, on: bool) {
        self.always_on_top = on;
        let level = if on {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
    }

    fn open_file_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Video", &["mp4", "mov", "mkv", "m4v", "webm"])
            .pick_file()
        {
            self.open_path(path);
        }
    }

    /// 打开指定路径的视频（文件对话框 / 命令行 / 拖拽 共用）。
    /// 若该源已存在全 I 帧代理，则自动改用代理（跳转即时）。
    fn open_path(&mut self, source: PathBuf) {
        self.cancel_proxy(); // 打开新文件：中止上一个文件的转码
        self.source_file = Some(source.clone());
        self.file = Some(source.clone());
        self.tex = None;
        self.restore_frame = None;
        self.fit_pending = true; // 打开后按视频比例自适应窗口
        let (to_open, using) = match proxy::existing_proxy(&source) {
            Some(p) => (p, true),
            None => (source.clone(), false),
        };
        self.using_proxy = using;
        self.status = if using {
            "已用加速代理(跳转即时)".into()
        } else {
            format!("正在打开 {}…", source.display())
        };
        let _ = self.decoder.to.send(ToDecoder::Open(to_open));
    }

    /// 为当前源生成全 I 帧代理（后台转码，进度见设置面板）。
    fn start_proxy(&mut self, ctx: &egui::Context) {
        if self.using_proxy || self.proxy_rx.is_some() || self.total_frames == 0 {
            return;
        }
        let Some(src) = self.source_file.clone() else {
            log::warn!("start_proxy: no source_file");
            return;
        };
        let Some(ffmpeg) = proxy::ffmpeg_exe() else {
            self.status = "未找到 ffmpeg.exe，无法生成代理".into();
            return;
        };
        let Some(dest) = proxy::proxy_path_for(&src) else {
            log::warn!("start_proxy: proxy_path_for(None) src={src:?}");
            self.status = "无法确定代理路径".into();
            return;
        };
        log::info!("start_proxy: 开始转码 {src:?} -> {dest:?}");
        let cancel = Arc::new(AtomicBool::new(false));
        self.proxy_cancel = Some(cancel.clone());
        self.proxy_progress = Some(0.0);
        self.proxy_rx = Some(proxy::spawn_transcode(
            ffmpeg,
            src,
            dest,
            self.total_frames,
            ctx.clone(),
            cancel,
        ));
    }

    /// 中止正在进行的代理转码。
    fn cancel_proxy(&mut self) {
        if let Some(c) = self.proxy_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        self.proxy_rx = None;
        self.proxy_progress = None;
    }

    /// 处理代理转码进度/完成。
    fn drain_proxy(&mut self) {
        let msgs: Vec<ProxyMsg> = self
            .proxy_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for msg in msgs {
            match msg {
                ProxyMsg::Progress(p) => self.proxy_progress = Some(p),
                ProxyMsg::Done(path) => {
                    self.proxy_rx = None;
                    self.proxy_progress = None;
                    self.proxy_cancel = None;
                    self.using_proxy = true;
                    self.status = "加速完成，已切换到代理".into();
                    self.restore_frame = Some(self.current_frame);
                    self.tex = None;
                    proxy::evict_cache(self.proxy_cache_max_mb); // 新代理写入后压缩缓存
                    let _ = self.decoder.to.send(ToDecoder::Open(path));
                }
                ProxyMsg::Failed(e) => {
                    self.proxy_rx = None;
                    self.proxy_progress = None;
                    self.proxy_cancel = None;
                    self.status = format!("加速失败: {e}");
                    log::error!("proxy: {e}");
                }
            }
        }
    }

    fn step(&mut self, delta: i64) {
        if self.total_frames == 0 {
            return;
        }
        let last = self.total_frames as i64 - 1;
        let target = (self.current_frame as i64 + delta).clamp(0, last) as u64;
        self.request(target, false);
    }

    /// 统一的取帧入口（精确 or 拖动预览）。在途即只记最新请求并合并，解码线程
    /// 同一时刻只处理一个，绝不堆积。preview=true 时只解关键帧做粗预览。
    fn request(&mut self, target: u64, preview: bool) {
        if target >= self.total_frames {
            return;
        }
        if self.pending {
            self.pending_req = Some((target, preview));
            return;
        }
        // 已精确停在该帧则不重复请求；但若当前是预览画面，松手的精确请求必须发出
        if !preview && target == self.current_frame && !self.current_is_preview {
            return;
        }
        self.pending = true;
        self.pending_preview = preview;
        self.pending_req = None;
        let msg = if preview {
            ToDecoder::Preview(target)
        } else {
            ToDecoder::GetFrame(target)
        };
        let _ = self.decoder.to.send(msg);
    }

    /// 处理来自全局钩子的已识别动作。
    fn handle_action(&mut self, ctx: &egui::Context, action: Action, pressed: bool) {
        log::debug!("handle_action {action:?} pressed={pressed} active={}", input::is_active());
        let now = ctx.input(|i| i.time);
        match (action, pressed) {
            (Action::TogglePlay, true) => self.toggle_play(ctx),
            (Action::ToggleAlwaysOnTop, true) => {
                let on = !self.always_on_top;
                self.set_always_on_top(ctx, on);
            }
            (Action::ToggleImmersive, true) => self.set_immersive(ctx, !self.immersive),
            (Action::PrevFrame, true) => {
                self.playing = false;
                self.holding = Some(-1);
                self.holding_vk = self.binding_vk(Action::PrevFrame);
                self.hold_last = now;
                self.step(-1);
            }
            (Action::NextFrame, true) => {
                self.playing = false;
                self.holding = Some(1);
                self.holding_vk = self.binding_vk(Action::NextFrame);
                self.hold_last = now;
                self.step(1);
            }
            (Action::PrevFrame, false) => {
                if self.holding == Some(-1) {
                    self.holding = None;
                    self.holding_vk = None;
                }
            }
            (Action::NextFrame, false) => {
                if self.holding == Some(1) {
                    self.holding = None;
                    self.holding_vk = None;
                }
            }
            _ => {}
        }
    }

    fn binding_vk(&self, action: Action) -> Option<u32> {
        self.bindings.iter().find(|b| b.action == action).map(|b| b.vk)
    }

    /// 捕获模式抓到一个组合键 → 写入正在重绑的动作。
    fn handle_capture(&mut self, vk: u32, ctrl: bool, alt: bool, shift: bool) {
        log::info!(
            "handle_capture vk={vk:#x} ctrl={ctrl} alt={alt} shift={shift} capturing={:?}",
            self.capturing
        );
        let Some(action) = self.capturing.take() else {
            return;
        };
        if vk == 0x1B {
            return; // Esc = 取消
        }
        for b in &mut self.bindings {
            if b.action == action {
                b.vk = vk;
                b.ctrl = ctrl;
                b.alt = alt;
                b.shift = shift;
            }
        }
        input::set_bindings(self.bindings.clone());
        self.save_config();
    }

    fn save_config(&self) {
        Config {
            play_interval_s: self.play_interval_s,
            hold_interval_s: self.hold_interval_s,
            proxy_cache_max_mb: self.proxy_cache_max_mb,
            bindings: self.bindings.clone(),
        }
        .save();
    }

    fn drain_decoder(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.decoder.from.try_recv() {
            match msg {
                FromDecoder::Opened {
                    total_frames,
                    fps,
                    width,
                    height,
                    keyframes,
                } => {
                    self.total_frames = total_frames;
                    self.fps = fps;
                    self.vid_w = width;
                    self.vid_h = height;
                    self.current_frame = 0;
                    self.playing = false;
                    self.pending = false;
                    self.pending_req = None;
                    self.current_is_preview = false;
                    self.holding = None;
                    if !self.using_proxy {
                        self.status.clear();
                    }
                    // 切换到代理后回到原先所在帧
                    if let Some(f) = self.restore_frame.take() {
                        self.request(f.min(total_frames.saturating_sub(1)), false);
                    }
                    // 按视频比例自适应窗口（消除黑边）
                    if std::mem::take(&mut self.fit_pending) {
                        self.fit_window_to_video(ctx);
                    }
                    // 打开非代理的长 GOP 视频 → 自动后台生成加速代理（短 GOP/全 I 帧不浪费）
                    let avg_gop = if keyframes > 0 { total_frames / keyframes } else { 0 };
                    if !self.using_proxy && self.proxy_rx.is_none() && avg_gop > 12 {
                        self.start_proxy(ctx);
                    }
                }
                FromDecoder::Frame(frame) => {
                    self.current_frame = frame.index;
                    self.current_is_preview = self.pending_preview;
                    self.pending = false;
                    self.upload_frame(ctx, &frame);
                }
                FromDecoder::Error(e) => {
                    self.pending = false;
                    self.status = format!("错误：{e}");
                    log::error!("{e}");
                }
            }
        }

        // 在途请求完成后，若还有积压的最新请求，立刻发出（拖动合并的关键一步）
        if !self.pending {
            if let Some((t, p)) = self.pending_req.take() {
                self.request(t, p);
            }
        }
    }

    fn upload_frame(&mut self, ctx: &egui::Context, frame: &Arc<crate::msg::FrameRgba>) {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.pixels,
        );
        match &mut self.tex {
            Some(t) => t.set(image, egui::TextureOptions::NEAREST),
            None => {
                self.tex = Some(ctx.load_texture("frame", image, egui::TextureOptions::NEAREST))
            }
        }
    }

    fn toggle_play(&mut self, ctx: &egui::Context) {
        if self.total_frames == 0 {
            return;
        }
        self.playing = !self.playing;
        self.last_advance = ctx.input(|i| i.time);
    }

    /// 打开视频后把窗口调成视频的长宽比（视频区铺满、无黑边）。
    /// 以原生分辨率为基准、缩到屏幕工作区内；窗口高度 = 视频区高度 + 标题栏。
    fn fit_window_to_video(&self, ctx: &egui::Context) {
        if self.vid_w == 0 || self.vid_h == 0 {
            return;
        }
        if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            return; // 最大化时不动
        }
        let (max_w, max_h) = match ctx.input(|i| i.viewport().monitor_size) {
            Some(m) => (m.x * 0.92, m.y * 0.92 - TITLE_BAR_H),
            None => (1600.0, 900.0),
        };
        let vw = self.vid_w as f32;
        let vh = self.vid_h as f32;
        let scale = (max_w / vw).min(max_h / vh).min(1.0);
        let inner = egui::vec2((vw * scale).round(), (vh * scale + TITLE_BAR_H).round());
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(inner));
        ctx.request_repaint();
    }

    fn set_immersive(&mut self, ctx: &egui::Context, on: bool) {
        if on == self.immersive {
            return;
        }
        self.immersive = on;
        if on {
            self.show_settings = false;
            self.prev_size = None;
            self.immersive_since = ctx.input(|i| i.time);
        } else {
            // 退出沉浸：保持沉浸中调整后的大小，不再恢复进入前的尺寸
            self.prev_size = None;
        }
    }

    /// 沉浸模式：保持「窗口长宽比 == 视频长宽比」，画面铺满无黑边。
    /// 刚进入时缩小较大的一维（去黑边、不溢出）；之后按用户正在拖动的一维做比例锁定缩放，
    /// 因此拖任意边/角都能改变大小。
    fn enforce_immersive_aspect(&mut self, ctx: &egui::Context) {
        if self.vid_w == 0 || self.vid_h == 0 {
            return;
        }
        // 最大化时长宽比由屏幕决定，必然有黑边 —— 先取消最大化
        if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            self.prev_size = None;
            return;
        }
        let size = ctx.content_rect().size();
        let (w, h) = (size.x, size.y);
        if w <= 1.0 || h <= 1.0 {
            return;
        }
        let va = self.vid_w as f32 / self.vid_h as f32;
        let (nw, nh) = match self.prev_size {
            None => {
                if w / h > va {
                    (h * va, h)
                } else {
                    (w, w / va)
                }
            }
            Some(prev) => {
                if (w - prev.x).abs() >= (h - prev.y).abs() {
                    (w, w / va) // 用户在改宽 → 高跟随
                } else {
                    (h * va, h) // 用户在改高 → 宽跟随
                }
            }
        };
        if (nw - w).abs() > 1.0 || (nh - h).abs() > 1.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(nw, nh)));
            ctx.request_repaint();
            self.prev_size = Some(egui::vec2(nw, nh));
        } else {
            self.prev_size = Some(size);
        }
    }

    // ——————————————————————— 标题栏 ———————————————————————
    fn title_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Panel::top("titlebar")
            .exact_size(TITLE_BAR_H)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::NONE
                    .fill(col::TITLEBAR)
                    .inner_margin(egui::Margin {
                        left: 13,
                        right: 6,
                        top: 0,
                        bottom: 0,
                    }),
            )
            .show(ui, |ui| {
                let bar = ui.max_rect();
                let drag = ui.interact(bar, egui::Id::new("title_drag"), egui::Sense::click_and_drag());
                drag.surrender_focus(); // 标题栏拖拽区不持有键盘焦点
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
                }

                let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ui.horizontal_centered(|ui| {
                    // 左侧：新建窗口(+) · 置顶(pin) · 图标/标题
                    if icon_button(ui, ico::ADD, "新建独立窗口", col::TEXT_DIM).clicked() {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).spawn();
                        }
                    }
                    let pin_color = if self.always_on_top {
                        col::ACCENT
                    } else {
                        col::TEXT_DIM
                    };
                    let pin_hover = if self.always_on_top {
                        "窗口置顶：开"
                    } else {
                        "窗口置顶：关"
                    };
                    if icon_button(ui, ico::PIN, pin_hover, pin_color).clicked() {
                        let on = !self.always_on_top;
                        self.set_always_on_top(ctx, on);
                    }
                    ui.add_space(2.0);
                    ui.label(icon_rt(ico::APP, 14.0, col::ACCENT));
                    ui.label(
                        egui::RichText::new("framePlayer")
                            .color(col::TEXT)
                            .size(12.5),
                    );
                    if let Some(name) = self.file.as_ref().and_then(|f| f.file_name()) {
                        ui.label(
                            egui::RichText::new(format!("—  {}", name.to_string_lossy()))
                                .color(col::TEXT_FAINT)
                                .size(12.0),
                        );
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if window_button(ui, ico::CLOSE, "关闭").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        let max_icon = if maxed { ico::RESTORE } else { ico::MAX };
                        if window_button(ui, max_icon, "最大化 / 还原").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maxed));
                        }
                        if window_button(ui, ico::MIN, "最小化").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                    });
                });
            });
    }

    // ——————————————————————— 底部悬浮控制条 ———————————————————————
    fn control_bar(&mut self, ctx: &egui::Context) {
        egui::Area::new("ctrlbar".into())
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
            .movable(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(col::bar())
                    .stroke(egui::Stroke::new(1.0, col::BORDER))
                    .corner_radius(egui::CornerRadius::same(9))
                    .inner_margin(egui::Margin::symmetric(9, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if icon_button(ui, ico::OPEN, "打开文件", col::TEXT).clicked() {
                                self.open_file_dialog();
                            }
                            separator(ui);

                            if icon_button(ui, ico::PREV, "上一帧（←）· 按住连续后退", col::TEXT)
                                .clicked()
                            {
                                self.step(-1);
                            }
                            let play_icon = if self.playing { ico::PAUSE } else { ico::PLAY };
                            if icon_button(ui, play_icon, "播放 / 暂停（空格）", col::ACCENT).clicked() {
                                self.toggle_play(ctx);
                            }
                            if icon_button(ui, ico::NEXT, "下一帧（→）· 按住连续前进", col::TEXT)
                                .clicked()
                            {
                                self.step(1);
                            }

                            // 帧号 / 总帧数
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(format!("{}", self.current_frame))
                                    .monospace()
                                    .color(col::TEXT_HI)
                                    .size(13.0),
                            );
                            ui.label(egui::RichText::new("/").monospace().color(col::TEXT_FAINT));
                            ui.label(
                                egui::RichText::new(format!("{}", self.total_frames.saturating_sub(1)))
                                    .monospace()
                                    .color(col::TEXT_DIM)
                                    .size(13.0),
                            );

                            // 按帧进度条
                            if self.total_frames > 0 {
                                let last = self.total_frames - 1;
                                let mut pos = self.current_frame.min(last);
                                ui.spacing_mut().slider_width = 190.0;
                                let resp =
                                    ui.add(egui::Slider::new(&mut pos, 0..=last).show_value(false));
                                resp.surrender_focus(); // 不抢键盘焦点，避免拖完进度条后 Tab 失效
                                if resp.dragged() {
                                    self.request(pos, true); // 拖动中：关键帧快速预览
                                } else if resp.changed() || resp.drag_stopped() {
                                    // 点击 / 松手：先给关键帧即时反馈，再精确（合并：精确排在预览之后）
                                    self.request(pos, true);
                                    self.request(pos, false);
                                }
                            }

                            // 视频时间（进度条右侧）：当前 / 总时长
                            if self.fps > 0.0 {
                                let cur = self.current_frame as f64 / self.fps;
                                let tot = self.total_frames as f64 / self.fps;
                                ui.add_space(2.0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} / {}",
                                        fmt_time(cur),
                                        fmt_time(tot)
                                    ))
                                    .monospace()
                                    .color(col::TEXT_DIM)
                                    .size(12.5),
                                );
                            }

                            separator(ui);

                            let gear_color = if self.show_settings {
                                col::ACCENT
                            } else {
                                col::TEXT_DIM
                            };
                            if icon_button(ui, ico::SETTINGS, "设置", gear_color).clicked() {
                                self.show_settings = !self.show_settings;
                            }
                        });
                    });
            });
    }

    // ——————————————————————— 设置弹层 ———————————————————————
    fn settings_popup(&mut self, ctx: &egui::Context) {
        egui::Area::new("settings".into())
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -66.0))
            .movable(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(col::PANEL)
                    .stroke(egui::Stroke::new(1.0, col::BORDER2))
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(0))
                    .show(ui, |ui| {
                        ui.set_width(330.0);

                        // 头
                        ui.horizontal(|ui| {
                            ui.add_space(14.0);
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new("设置").color(col::TEXT_HI).size(13.5).strong(),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add_space(6.0);
                                if window_button(ui, ico::CLOSE, "关闭").clicked() {
                                    self.show_settings = false;
                                }
                            });
                        });
                        ui.add_space(2.0);
                        section_sep(ui);

                        // 区：播放速度
                        ui.add_space(8.0);
                        section_title(ui, "播放速度 · 每帧之间的秒数");
                        if speed_row(ui, "正常播放", "连续播放时每帧停留", &mut self.play_interval_s)
                            .changed()
                        {
                            self.save_config();
                        }
                        ui.add_space(6.0);
                        if speed_row(
                            ui,
                            "按住连播",
                            "按住切帧键时每帧停留",
                            &mut self.hold_interval_s,
                        )
                        .changed()
                        {
                            self.save_config();
                        }
                        ui.add_space(10.0);
                        section_sep(ui);

                        // 区：全局快捷键（点击键位可重新绑定）
                        ui.add_space(8.0);
                        section_title(ui, "全局快捷键 · 点击键位可重新绑定");
                        let bindings = self.bindings.clone();
                        let capturing = self.capturing;
                        let mut toggle: Option<Action> = None;
                        for action in Action::ALL {
                            let is_cap = capturing == Some(action);
                            let label = if is_cap {
                                "按下新组合键…  Esc 取消".to_string()
                            } else {
                                bindings
                                    .iter()
                                    .find(|b| b.action == action)
                                    .map(input::chord_label)
                                    .unwrap_or_else(|| "—".into())
                            };
                            let clicked = ui
                                .horizontal(|ui| {
                                    ui.add_space(16.0);
                                    ui.label(
                                        egui::RichText::new(action.label())
                                            .color(col::TEXT)
                                            .size(12.5),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.add_space(16.0);
                                            let color =
                                                if is_cap { col::ACCENT } else { col::TEXT };
                                            let mut btn = egui::Button::new(
                                                egui::RichText::new(&label)
                                                    .monospace()
                                                    .size(12.0)
                                                    .color(color),
                                            );
                                            if is_cap {
                                                btn = btn.stroke(egui::Stroke::new(1.0, col::ACCENT));
                                            }
                                            ui.add(btn).clicked()
                                        },
                                    )
                                    .inner
                                })
                                .inner;
                            if clicked {
                                toggle = Some(action);
                            }
                            ui.add_space(7.0);
                        }
                        if let Some(action) = toggle {
                            if self.capturing == Some(action) {
                                self.capturing = None;
                                input::cancel_capture();
                            } else {
                                self.capturing = Some(action);
                                input::begin_capture();
                                // 释放键盘焦点：刚点过的按钮会持有焦点，捕获时按空格/回车会
                                // 再次「激活」它，导致捕获被反复取消——看起来就像设不上快捷键。
                                let focused = ui.ctx().memory(|m| m.focused());
                                if let Some(id) = focused {
                                    ui.ctx().memory_mut(|m| m.surrender_focus(id));
                                }
                            }
                        }
                        ui.add_space(3.0);
                        section_sep(ui);

                        // 区：视频信息
                        ui.add_space(8.0);
                        section_title(ui, "视频信息");
                        let res = if self.vid_w > 0 {
                            format!("{} × {}", self.vid_w, self.vid_h)
                        } else {
                            "—".into()
                        };
                        info_row(ui, "分辨率", &res);
                        info_row(ui, "帧率", &format!("{:.3} fps", self.fps));
                        info_row(ui, "总帧数", &format!("{}", self.total_frames));
                        let dur = if self.fps > 0.0 {
                            let secs = self.total_frames as f64 / self.fps;
                            format!("{:02}:{:05.2}", (secs as u64) / 60, secs % 60.0)
                        } else {
                            "—".into()
                        };
                        info_row(ui, "时长", &dur);
                        ui.add_space(10.0);
                        section_sep(ui);

                        // 区：加速跳转（全 I 帧代理）
                        ui.add_space(8.0);
                        section_title(ui, "加速跳转 · 生成全 I 帧代理");
                        let using = self.using_proxy;
                        let progress = self.proxy_progress;
                        let total = self.total_frames;
                        let mut start = false;
                        ui.horizontal(|ui| {
                            ui.add_space(16.0);
                            if using {
                                ui.label(
                                    egui::RichText::new("✓ 已使用加速代理，跳转即时")
                                        .color(col::ACCENT)
                                        .size(12.5),
                                );
                            } else if let Some(p) = progress {
                                ui.label(
                                    egui::RichText::new(format!("加速中… {:.0}%", p * 100.0))
                                        .color(col::TEXT)
                                        .size(12.5),
                                );
                            } else if total == 0 {
                                ui.label(
                                    egui::RichText::new("先打开一个视频")
                                        .color(col::TEXT_FAINT)
                                        .size(12.5),
                                );
                            } else if ui
                                .add(egui::Button::new(
                                    egui::RichText::new("生成加速代理").size(12.5).color(col::TEXT_HI),
                                ))
                                .on_hover_text(
                                    "把当前视频转成每帧都是关键帧的副本，之后跳任意帧即时。一次性后台任务，占额外磁盘。",
                                )
                                .clicked()
                            {
                                start = true;
                            }
                        });
                        if let Some(p) = progress {
                            ui.horizontal(|ui| {
                                ui.add_space(16.0);
                                ui.add(
                                    egui::ProgressBar::new(p)
                                        .desired_width(290.0)
                                        .show_percentage(),
                                );
                            });
                        }
                        if start {
                            self.start_proxy(ctx);
                        }
                        ui.add_space(8.0);
                        // 代理缓存上限（界面 GiB，存 MiB）
                        let mut gib = self.proxy_cache_max_mb as f64 / 1024.0;
                        if cache_row(ui, &mut gib).changed() {
                            self.proxy_cache_max_mb = (gib * 1024.0).round() as u64;
                            self.save_config();
                            proxy::evict_cache(self.proxy_cache_max_mb); // 调小即时生效
                        }
                        ui.add_space(10.0);
                    });
            });
    }
}

impl eframe::App for FramePlayerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_decoder(&ctx);
        self.drain_proxy();
        // 多窗口全局热键归属：维护本进程「是否激活」状态（含轻量心跳重绘）
        self.update_active_window(&ctx);
        // 反应式空闲时，在指定时间窗内持续重绘，保证双击第二击能被采样到
        if ctx.input(|i| i.time) < self.repaint_until {
            ctx.request_repaint();
        }

        // 文件拖拽：把拖入窗口的第一个有路径的文件直接打开
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone())) {
            self.open_path(path);
        }

        // 全局热键事件（RegisterHotKey → 热键线程 → 通道）
        while let Ok(msg) = self.hook_rx.try_recv() {
            match msg {
                HookMsg::Action { action, pressed } => self.handle_action(&ctx, action, pressed),
            }
        }
        // 重绑：设置面板里点了某个键位后，轮询用户按下的新组合键
        if self.capturing.is_some() {
            ctx.request_repaint(); // 捕获时保持重绘，及时采样按键
            if let Some((vk, ctrl, alt, shift)) = input::poll_captured() {
                self.handle_capture(vk, ctrl, alt, shift);
            }
        }
        // 失效保护：物理键已松开但 keyup 事件丢失 → 主动停止连播（否则会永久空转重绘）
        if let (Some(_), Some(vk)) = (self.holding, self.holding_vk) {
            let down = input::key_physically_down(vk);
            if !down {
                log::debug!("hold failsafe: vk={vk:#x} physically up -> stop scrub");
                self.holding = None;
                self.holding_vk = None;
            }
        }
        // 按住全局键连续切帧：节奏受 hold_interval 与解码速度共同限制，松手即停
        if let Some(dir) = self.holding {
            let now = ctx.input(|i| i.time);
            if !self.pending && now - self.hold_last >= self.hold_interval_s {
                // 用固定节拍推进 hold_last（+= interval），而不是 = now：后者每拍都会把
                // 「超出的那点时间」算进下一拍，累积成系统性偏慢（实测约慢 50%）。
                self.hold_last += self.hold_interval_s;
                if now - self.hold_last >= self.hold_interval_s {
                    self.hold_last = now; // 落后超过一拍（卡顿/解码慢）→ 重对齐，避免追帧爆发
                }
                self.step(dir);
            }
            // 已到首/末帧则停止重排，避免边界处空转
            let at_boundary = (dir < 0 && self.current_frame == 0)
                || (dir > 0 && self.current_frame + 1 >= self.total_frames);
            if !at_boundary {
                // 连播期间满帧重绘：让步进贴近设定节奏，不被粗定时器粒度拖慢
                ctx.request_repaint();
            }
        }

        resize_handles(ui, &ctx);

        // 沉浸模式下持续保持「窗口长宽比 == 视频长宽比」，彻底消除黑边
        if self.immersive {
            self.enforce_immersive_aspect(&ctx);
        }

        // 本地快捷键：仅当没有控件持有键盘焦点、不在捕获、且【没有按任何修饰键】时触发。
        // - focused().is_none()：避免和设置里的数值输入框抢键（按钮已主动让焦点）。
        // - modifiers.is_none()：consume_key 用的是 matches_logically，Modifiers::NONE 会
        //   匹配到带 Ctrl/Alt 的事件；焦点在本窗口时 Ctrl+Alt+Space 会被全局钩子与这里
        //   各处理一次（双切=无效）。限定裸键即可避免与全局组合键互相抵消。
        if self.capturing.is_none()
            && ctx.memory(|m| m.focused().is_none())
            && ctx.input(|i| i.modifiers.is_none())
        {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
                // egui 在 begin_pass 已把这次 Tab 记作「焦点前移」，可能顺手让某按钮获焦；
                // 主动放弃焦点，保证下一次 Tab 仍走到这里。
                if let Some(id) = ctx.memory(|m| m.focused()) {
                    ctx.memory_mut(|m| m.surrender_focus(id));
                }
                self.set_immersive(&ctx, !self.immersive);
            }
            if self.immersive
                && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                self.set_immersive(&ctx, false);
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight)) {
                self.step(1);
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft)) {
                self.step(-1);
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space)) {
                self.toggle_play(&ctx);
            }
        }

        // 正常播放定时器（M3 会用更精确的 Controller 定时器替换）
        if self.playing && self.total_frames > 0 {
            let now = ctx.input(|i| i.time);
            if now - self.last_advance >= self.play_interval_s {
                self.last_advance = now;
                if self.current_frame + 1 >= self.total_frames {
                    self.playing = false;
                } else {
                    self.step(1);
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                self.play_interval_s.clamp(0.001, 10.0),
            ));
        }

        if !self.immersive {
            self.title_bar(ui, &ctx);
        }

        let mut dbl_fit = false;
        let mut video_warm = false;
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = ui.max_rect();
            // 双击画面：按视频比例自适应窗口、去黑边（沉浸模式下双击是退出，由别处处理）
            if !self.immersive {
                let resp =
                    ui.interact(rect, egui::Id::new("video_fit"), egui::Sense::click());
                resp.surrender_focus(); // 画面交互区不持有键盘焦点
                if resp.double_clicked() {
                    dbl_fit = true;
                }
                // 反应式重绘下两次单击之间若没有新事件，egui 不跑帧→第二击采样不到→
                // 双击丢失（刚拉伸完窗口尤为明显）。一旦在画面上按下/单击，开一小段持续
                // 重绘窗口，保证双击间隔内一定有帧。
                if resp.clicked() || resp.is_pointer_button_down_on() {
                    video_warm = true;
                }
            }
            let painter = ui.painter();
            painter.rect_filled(rect, 0.0, col::VIDEO_BLACK);
            if let Some(tex) = &self.tex {
                let tsize = tex.size_vec2();
                if tsize.x > 0.0 && tsize.y > 0.0 {
                    let scale = (rect.width() / tsize.x).min(rect.height() / tsize.y);
                    let dest = egui::Rect::from_center_size(rect.center(), tsize * scale);
                    painter.image(
                        tex.id(),
                        dest,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
            } else {
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "未打开视频\n\n打开 MP4 后用 ← / → 逐帧 · 空格 播放 · Tab 沉浸 · 📌 置顶 · 双击去黑边",
                    egui::FontId::proportional(17.0),
                    col::TEXT_FAINT,
                );
            }
        });
        if video_warm {
            self.repaint_until = ctx.input(|i| i.time) + 0.5;
        }
        if dbl_fit {
            self.fit_window_to_video(&ctx);
        }

        if !self.immersive {
            self.control_bar(&ctx);
            if self.show_settings {
                self.settings_popup(&ctx);
            }
        } else {
            // 沉浸模式：按住画面拖动即可移动窗口（向内收 15px，避开四周缩放区）
            let drag_rect = ctx.content_rect().shrink(15.0);
            let resp = ui.interact(
                drag_rect,
                egui::Id::new("immersive_move"),
                egui::Sense::click_and_drag(),
            );
            if resp.drag_started_by(egui::PointerButton::Primary) {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if resp.double_clicked() {
                self.set_immersive(&ctx, false); // 双击退出沉浸
            }

            // 沉浸模式角标提示：停留 1s 后 1s 渐隐消失
            let elapsed = (ctx.input(|i| i.time) - self.immersive_since) as f32;
            const HOLD: f32 = 1.0;
            const FADE: f32 = 1.0;
            let alpha = if elapsed < HOLD {
                1.0
            } else if elapsed < HOLD + FADE {
                1.0 - (elapsed - HOLD) / FADE
            } else {
                0.0
            };
            if alpha > 0.0 {
                let a = (alpha * 138.0) as u8; // TEXT_DIM 的 0x8A≈138 作为满不透明上限
                let color = egui::Color32::from_rgba_unmultiplied(0x8A, 0x8A, 0x92, a.max(1));
                egui::Area::new("immersive_hint".into())
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
                    .movable(false)
                    .interactable(false)
                    .show(&ctx, |ui| {
                        ui.label(
                            egui::RichText::new("沉浸模式 · 按 Tab / Esc 退出")
                                .color(color)
                                .size(12.0),
                        );
                    });
                ctx.request_repaint();
            }
        }

        // 拖拽悬停提示：有文件正拖到窗口上方时，居中显示「松开以打开视频」
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            egui::Area::new("drop_hint".into())
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .interactable(false)
                .show(&ctx, |ui| {
                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 190))
                        .stroke(egui::Stroke::new(1.5, col::ACCENT))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(egui::Margin::symmetric(28, 20))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("松开以打开视频")
                                    .color(col::TEXT_HI)
                                    .size(18.0),
                            );
                        });
                });
        }
    }
}

// ——————————————————————— 小部件辅助 ———————————————————————

fn icon_button(ui: &mut egui::Ui, ch: char, hover: &str, color: egui::Color32) -> egui::Response {
    let resp = ui.add(
        egui::Button::new(icon_rt(ch, 16.0, color))
            .frame(false)
            .min_size(egui::vec2(30.0, 30.0)),
    );
    // 不抢键盘焦点：否则点过按钮后裸键(Tab/空格/←→)会被「焦点」判据屏蔽，
    // 且全局组合键泄漏到 egui 时会再次「激活」该按钮。
    resp.surrender_focus();
    resp.on_hover_text(hover)
}

fn window_button(ui: &mut egui::Ui, ch: char, hover: &str) -> egui::Response {
    let resp = ui.add(
        egui::Button::new(icon_rt(ch, 11.0, col::TEXT_DIM))
            .frame(false)
            .min_size(egui::vec2(32.0, 26.0)),
    );
    resp.surrender_focus();
    resp.on_hover_text(hover)
}

/// 秒数格式化为 mm:ss.mmm（超过一小时显示 h:mm:ss.mmm）。
fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0);
    let h = (s / 3600.0) as u64;
    let m = ((s % 3600.0) / 60.0) as u64;
    let sec = s % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{sec:06.3}")
    } else {
        format!("{m:02}:{sec:06.3}")
    }
}

fn separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 20.0), egui::Sense::hover());
    let x = rect.center().x;
    ui.painter().line_segment(
        [egui::pos2(x, rect.top() + 2.0), egui::pos2(x, rect.bottom() - 2.0)],
        egui::Stroke::new(1.0, col::BORDER2),
    );
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(text)
                .color(col::TEXT_FAINT)
                .size(10.5)
                .strong(),
        );
    });
    ui.add_space(8.0);
}

fn section_sep(ui: &mut egui::Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 0.0, col::BORDER);
}

fn speed_row(ui: &mut egui::Ui, label: &str, sub: &str, value: &mut f64) -> egui::Response {
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label).color(col::TEXT).size(12.5));
            ui.label(egui::RichText::new(sub).color(col::TEXT_FAINT).size(11.0));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(16.0);
            ui.add(
                egui::DragValue::new(value)
                    .speed(0.001)
                    .range(0.001..=10.0)
                    .fixed_decimals(3)
                    .suffix(" s"),
            )
        })
        .inner
    })
    .inner
}

/// 代理缓存上限行：界面用 GiB 编辑，外部换算成 MiB 存储。
fn cache_row(ui: &mut egui::Ui, gib: &mut f64) -> egui::Response {
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new("缓存上限").color(col::TEXT).size(12.5));
            ui.label(
                egui::RichText::new("超出按最旧淘汰 · 0 = 不限")
                    .color(col::TEXT_FAINT)
                    .size(11.0),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(16.0);
            ui.add(
                egui::DragValue::new(gib)
                    .speed(0.25)
                    .range(0.0..=512.0)
                    .fixed_decimals(2)
                    .suffix(" GiB"),
            )
        })
        .inner
    })
    .inner
}

fn info_row(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(egui::RichText::new(k).color(col::TEXT_DIM).size(12.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(16.0);
            ui.label(egui::RichText::new(v).monospace().color(col::TEXT).size(12.5));
        });
    });
    ui.add_space(5.0);
}

/// 窗口四边 + 四角缩放（自绘装饰丢失了系统缩放边）。
fn resize_handles(ui: &mut egui::Ui, ctx: &egui::Context) {
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    use egui::{pos2, CursorIcon as CI, Id, Rect, ResizeDirection as RD, Sense, ViewportCommand};
    let r = ctx.content_rect();
    let t = 6.0;
    let c = 14.0;
    let handles = [
        ("rz_n", Rect::from_min_max(pos2(r.left() + c, r.top()), pos2(r.right() - c, r.top() + t)), RD::North, CI::ResizeVertical),
        ("rz_s", Rect::from_min_max(pos2(r.left() + c, r.bottom() - t), pos2(r.right() - c, r.bottom())), RD::South, CI::ResizeVertical),
        ("rz_w", Rect::from_min_max(pos2(r.left(), r.top() + c), pos2(r.left() + t, r.bottom() - c)), RD::West, CI::ResizeHorizontal),
        ("rz_e", Rect::from_min_max(pos2(r.right() - t, r.top() + c), pos2(r.right(), r.bottom() - c)), RD::East, CI::ResizeHorizontal),
        ("rz_nw", Rect::from_min_max(pos2(r.left(), r.top()), pos2(r.left() + c, r.top() + c)), RD::NorthWest, CI::ResizeNwSe),
        ("rz_ne", Rect::from_min_max(pos2(r.right() - c, r.top()), pos2(r.right(), r.top() + c)), RD::NorthEast, CI::ResizeNeSw),
        ("rz_sw", Rect::from_min_max(pos2(r.left(), r.bottom() - c), pos2(r.left() + c, r.bottom())), RD::SouthWest, CI::ResizeNeSw),
        ("rz_se", Rect::from_min_max(pos2(r.right() - c, r.bottom() - c), pos2(r.right(), r.bottom())), RD::SouthEast, CI::ResizeNwSe),
    ];
    for (id, rect, dir, cursor) in handles {
        let resp = ui.interact(rect, Id::new(id), Sense::drag());
        // 不抢键盘焦点：否则 Tab 焦点遍历会停在第一个缩放手柄上，
        // 使 focused().is_none() 永远为假，本地裸键(Tab/空格/←→)随之失效。
        resp.surrender_focus();
        if resp.hovered() || resp.dragged() {
            ctx.set_cursor_icon(cursor);
        }
        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }
}

/// 取本窗口的 Win32 HWND（isize 形式），用于去圆角与判断是否前台窗口。
fn window_hwnd(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = cc.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// 关闭 Windows 11 DWM 的窗口圆角（DWMWCP_DONOTROUND）。无边框窗口默认仍被系统圆角，
/// 沉浸模式画面铺满时圆角会裁掉画面四角，故统一改成尖角。Win10 不支持该属性，失败忽略。
fn square_window_corners(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    };
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    let pref = DWMWCP_DONOTROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const core::ffi::c_void,
            std::mem::size_of_val(&pref) as u32,
        );
    }
}

/// 深色 + 琥珀 扁平主题。
fn setup_theme(ctx: &egui::Context) {
    use egui::{Color32, CornerRadius, Stroke};
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(col::TEXT);
    v.panel_fill = col::WIN_BG;
    v.window_fill = col::PANEL;
    v.window_stroke = Stroke::new(1.0, col::BORDER2);
    v.extreme_bg_color = col::RAISED;
    v.faint_bg_color = col::RAISED;
    v.selection.bg_fill = col::accent_soft();
    v.selection.stroke = Stroke::new(1.0, col::ACCENT);
    v.hyperlink_color = col::ACCENT;

    let r6 = CornerRadius::same(6);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, col::BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, col::TEXT);

    v.widgets.inactive.bg_fill = col::RAISED;
    v.widgets.inactive.weak_bg_fill = col::RAISED;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, col::BORDER2);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, col::TEXT);
    v.widgets.inactive.corner_radius = r6;

    v.widgets.hovered.bg_fill = col::BORDER2;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2C, 0x2C, 0x32);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, col::BORDER2);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, col::TEXT_HI);
    v.widgets.hovered.corner_radius = r6;

    v.widgets.active.bg_fill = col::accent_soft();
    v.widgets.active.weak_bg_fill = col::accent_soft();
    v.widgets.active.bg_stroke = Stroke::new(1.0, col::ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, col::ACCENT_HI);
    v.widgets.active.corner_radius = r6;

    ctx.set_visuals(v);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 6.0);
        style.spacing.button_padding = egui::vec2(7.0, 5.0);
    });
}

/// 加载 Windows 自带中日韩字体（egui 默认字体不含 CJK 字形）。
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // 中文字体（egui 默认字体不含 CJK 字形）
    const CJK: &[&str] = &[
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/Deng.ttf",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ];
    for path in CJK {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            break;
        }
    }

    // 图标字体：Segoe Fluent Icons（Win11）/ Segoe MDL2 Assets（Win10），命名族 "icons"
    const ICONS: &[&str] = &["C:/Windows/Fonts/SegoeIcons.ttf", "C:/Windows/Fonts/segmdl2.ttf"];
    for path in ICONS {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("icons".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts
                .families
                .insert(egui::FontFamily::Name("icons".into()), vec!["icons".to_owned()]);
            break;
        }
    }

    ctx.set_fonts(fonts);
}
