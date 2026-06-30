// 系统级全局快捷键。改用 Win32 RegisterHotKey（可靠：不受 WH_KEYBOARD_LL 的
// LowLevelHooksTimeout 静默移除影响），在专用线程的消息循环里收 WM_HOTKEY。
// 多窗口：只有「最近激活」的窗口进程注册热键（set_active 驱动），别的进程注销，
// 因此热键只对激活窗口生效，且不会被多进程重复注册（RegisterHotKey 系统唯一）。
// 重绑捕获：设置面板聚焦时用 GetAsyncKeyState 轮询用户按下的组合键（无需钩子）。
// 按住连播：WM_HOTKEY(带 MOD_NOREPEAT)只触发一次起始，松手由 UI 端 GetAsyncKeyState 失效保护检测。

use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP, WM_HOTKEY,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Serialize, Deserialize, Default)]
pub enum Action {
    #[default]
    TogglePlay,
    PrevFrame,
    NextFrame,
    ToggleAlwaysOnTop,
    ToggleImmersive,
}

impl Action {
    /// 固定的展示顺序与中文名。
    pub const ALL: [Action; 5] = [
        Action::TogglePlay,
        Action::PrevFrame,
        Action::NextFrame,
        Action::ToggleAlwaysOnTop,
        Action::ToggleImmersive,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Action::TogglePlay => "播放 / 暂停",
            Action::PrevFrame => "上一帧",
            Action::NextFrame => "下一帧",
            Action::ToggleAlwaysOnTop => "窗口置顶",
            Action::ToggleImmersive => "沉浸模式",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Binding {
    pub action: Action,
    pub vk: u32,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// 保留字段（RegisterHotKey 天然会拦截命中的热键，不再需要单独控制）。
    pub suppress: bool,
}

fn same_chord(a: &Binding, b: &Binding) -> bool {
    a.vk == b.vk && a.ctrl == b.ctrl && a.alt == b.alt && a.shift == b.shift
}

#[derive(Clone, Copy, Debug)]
pub enum HookMsg {
    /// 已识别的动作：pressed=true 表示按下（RegisterHotKey 不提供松开，松手由 UI 端检测）
    Action { action: Action, pressed: bool },
}

static EVENT_TX: OnceLock<Sender<HookMsg>> = OnceLock::new();
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// 当前键位表（重绑/载入配置后更新；热键线程据此注册）。
static BINDINGS: OnceLock<Mutex<Vec<Binding>>> = OnceLock::new();
/// 本进程是否应持有全局热键注册（= 是否为最近激活窗口）。
static WANT_ACTIVE: AtomicBool = AtomicBool::new(true);
/// 是否在「捕获新组合键」中（此时注销热键，避免按到现有热键）。
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// 热键线程 id（用于 PostThreadMessage 唤醒它重新对账）。
static HOTKEY_TID: AtomicU32 = AtomicU32::new(0);

/// 自定义线程消息：唤醒热键线程重新对账注册。
const WM_RECONCILE: u32 = WM_APP + 1;

pub fn set_active(active: bool) {
    WANT_ACTIVE.store(active, Ordering::Relaxed);
    wake_reconcile();
}

pub fn is_active() -> bool {
    WANT_ACTIVE.load(Ordering::Relaxed)
}

/// 覆盖键位表（重新绑定 / 载入配置后调用），并触发重新注册。
pub fn set_bindings(b: Vec<Binding>) {
    if let Some(m) = BINDINGS.get() {
        if let Ok(mut g) = m.lock() {
            *g = b;
        }
    }
    wake_reconcile();
}

/// 进入捕获模式：暂时注销热键，由 UI 端轮询用户按下的下一个组合键。
pub fn begin_capture() {
    log::info!("begin_capture: 等待用户按下新组合键");
    CAPTURING.store(true, Ordering::Relaxed);
    wake_reconcile();
}
pub fn cancel_capture() {
    log::info!("cancel_capture");
    CAPTURING.store(false, Ordering::Relaxed);
    wake_reconcile();
}

fn wake_reconcile() {
    let tid = HOTKEY_TID.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_RECONCILE, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn default_bindings() -> Vec<Binding> {
    let m = |action, vk| Binding {
        action,
        vk,
        ctrl: true,
        alt: true,
        shift: false,
        suppress: true,
    };
    vec![
        m(Action::TogglePlay, 0x20),        // Ctrl+Alt+Space
        m(Action::PrevFrame, 0x25),         // Ctrl+Alt+Left
        m(Action::NextFrame, 0x27),         // Ctrl+Alt+Right
        m(Action::ToggleAlwaysOnTop, 0x54), // Ctrl+Alt+T
        m(Action::ToggleImmersive, 0x46),   // Ctrl+Alt+F
    ]
}

/// 启动热键线程，返回接收 HookMsg 的通道。
pub fn spawn_hook(ctx: egui::Context) -> Receiver<HookMsg> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let _ = EVENT_TX.set(tx);
    let _ = CTX.set(ctx);
    let _ = BINDINGS.set(Mutex::new(default_bindings()));

    std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(|| unsafe { hotkey_thread() })
        .expect("spawn hotkeys thread");

    rx
}

unsafe fn hotkey_thread() {
    // 先用 PeekMessage 建立线程消息队列，确保 PostThreadMessage 能送达。
    let mut msg = MSG::default();
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    HOTKEY_TID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    log::info!("全局热键线程已启动（RegisterHotKey）");

    let mut registered: Vec<(i32, Binding)> = Vec::new();
    let mut logged_fail: Vec<Binding> = Vec::new(); // 已记过日志的失败键，避免重试刷屏
    reconcile(&mut registered, &mut logged_fail);

    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        match msg.message {
            WM_HOTKEY => {
                let id = msg.wParam.0;
                let action = BINDINGS
                    .get()
                    .and_then(|m| m.lock().ok())
                    .and_then(|g| g.get(id).map(|b| b.action));
                if let Some(action) = action {
                    send(HookMsg::Action {
                        action,
                        pressed: true,
                    });
                }
            }
            WM_RECONCILE => reconcile(&mut registered, &mut logged_fail),
            _ => {}
        }
    }

    for (id, _) in registered.drain(..) {
        let _ = unsafe { UnregisterHotKey(None, id) };
    }
}

/// 把「期望注册的热键」与「已注册的」对账：注销多余/变动的，注册缺失/失败重试的。
/// 期望集合 = 激活且非捕获时的当前键位表；否则为空。`logged_fail` 记录已打过日志的
/// 失败键，使被占用的热键每次静默重试、只在首次失败时记一条 warn（不刷屏）。
fn reconcile(registered: &mut Vec<(i32, Binding)>, logged_fail: &mut Vec<Binding>) {
    let want = WANT_ACTIVE.load(Ordering::Relaxed) && !CAPTURING.load(Ordering::Relaxed);
    let desired: Vec<Binding> = if want {
        BINDINGS
            .get()
            .and_then(|m| m.lock().ok())
            .map(|g| g.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // 注销不再需要或键位已变的（id == 索引）
    registered.retain(|(id, b)| {
        let keep = desired
            .get(*id as usize)
            .map(|nb| same_chord(nb, b))
            .unwrap_or(false);
        if !keep {
            let _ = unsafe { UnregisterHotKey(None, *id) };
        }
        keep
    });

    // 注册缺失的（含上次因被占用而失败、本次重试）
    for (i, b) in desired.iter().enumerate() {
        let id = i as i32;
        if registered.iter().any(|(rid, rb)| *rid == id && same_chord(rb, b)) {
            continue;
        }
        let mut m = MOD_NOREPEAT.0;
        if b.ctrl {
            m |= MOD_CONTROL.0;
        }
        if b.alt {
            m |= MOD_ALT.0;
        }
        if b.shift {
            m |= MOD_SHIFT.0;
        }
        match unsafe { RegisterHotKey(None, id, HOT_KEY_MODIFIERS(m), b.vk) } {
            Ok(()) => {
                registered.push((id, *b));
                logged_fail.retain(|f| !same_chord(f, b)); // 注册成功 → 允许将来再次失败时记日志
            }
            Err(e) => {
                if !logged_fail.iter().any(|f| same_chord(f, b)) {
                    log::warn!(
                        "RegisterHotKey 失败 {} [{}]：{e}（可能被其它程序/系统占用，会静默重试）",
                        chord_label(b),
                        b.action.label()
                    );
                    logged_fail.push(*b);
                }
            }
        }
    }
    // 清掉已不再期望的失败记录，使其下次被加回并失败时仍会记一次日志
    logged_fail.retain(|f| desired.iter().any(|d| same_chord(d, f)));
}

fn is_modifier(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5 | 0x5B | 0x5C
    )
}

/// 捕获模式下轮询：找到当前按下的非修饰键 → 返回 (vk, ctrl, alt, shift) 并结束捕获。
/// 由 UI 线程在 capturing 时每帧调用。未捕获或无按键则返回 None。
pub fn poll_captured() -> Option<(u32, bool, bool, bool)> {
    if !CAPTURING.load(Ordering::Relaxed) {
        return None;
    }
    for vk in 0x08u32..=0xFEu32 {
        // 跳过修饰键与鼠标键
        if is_modifier(vk) || matches!(vk, 0x01..=0x06) {
            continue;
        }
        if key_physically_down(vk) {
            let ctrl = key_physically_down(0x11);
            let alt = key_physically_down(0x12);
            let shift = key_physically_down(0x10);
            CAPTURING.store(false, Ordering::Relaxed);
            wake_reconcile(); // 捕获结束，按新键位重新注册
            return Some((vk, ctrl, alt, shift));
        }
    }
    None
}

/// 物理按键是否当前按下（按住失效保护 + 捕获轮询用）。
pub fn key_physically_down(vk: u32) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

fn send(msg: HookMsg) {
    if let Some(tx) = EVENT_TX.get() {
        let _ = tx.try_send(msg);
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// 把组合键渲染成可读文本，如 "Ctrl+Alt+Space"。
pub fn chord_label(b: &Binding) -> String {
    let mut s = String::new();
    if b.ctrl {
        s.push_str("Ctrl+");
    }
    if b.alt {
        s.push_str("Alt+");
    }
    if b.shift {
        s.push_str("Shift+");
    }
    s.push_str(&vk_name(b.vk));
    s
}

pub fn vk_name(vk: u32) -> String {
    match vk {
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x25 => "←".into(),
        0x26 => "↑".into(),
        0x27 => "→".into(),
        0x28 => "↓".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x30..=0x39 => ((b'0' + (vk - 0x30) as u8) as char).to_string(),
        0x41..=0x5A => ((b'A' + (vk - 0x41) as u8) as char).to_string(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x70..=0x7B => format!("F{}", vk - 0x70 + 1),
        other => format!("0x{other:02X}"),
    }
}
