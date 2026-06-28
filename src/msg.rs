// 线程间消息类型（UI ↔ 解码 worker）。

use std::path::PathBuf;
use std::sync::Arc;

/// 一帧解码后的 RGBA8 数据（紧凑打包，stride == width*4）。
pub struct FrameRgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// 显示顺序的帧序号
    pub index: u64,
}

/// UI → 解码 worker
pub enum ToDecoder {
    Open(PathBuf),
    /// 请求精确的第 n 帧
    GetFrame(u64),
    /// 拖动预览：只解码 n 所在 GOP 的关键帧（快），但以帧号 n 标注
    Preview(u64),
}

/// 解码 worker → UI
pub enum FromDecoder {
    Opened {
        total_frames: u64,
        fps: f64,
        width: u32,
        height: u32,
        /// 关键帧数量（用于判断是否值得生成代理）
        keyframes: u64,
    },
    Frame(Arc<FrameRgba>),
    Error(String),
}
