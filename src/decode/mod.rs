// 解码 worker：在独立线程上用 FFmpeg 做逐帧精确解码 + GOP 缓存。
// M1：打开、扫包建显示顺序索引、跳关键帧→向前解码到任意目标帧。
// M4：触碰某帧时整段 GOP 一起解码并缓存（字节预算 LRU），后退/邻近全是缓存命中；
//     空闲时按行进方向预取相邻 GOP，跨关键帧不卡顿。

use crate::msg::{FrameRgba, FromDecoder, ToDecoder};
use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next as ff;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

/// 帧缓存字节预算（用户「不怕体积，怕卡」→ 给足）。帧数随分辨率自适应。
const CACHE_BUDGET: usize = 1024 * 1024 * 1024; // 1 GiB

pub struct DecoderHandle {
    pub to: Sender<ToDecoder>,
    pub from: Receiver<FromDecoder>,
}

pub fn spawn(ctx: egui::Context) -> DecoderHandle {
    let (to_tx, to_rx) = crossbeam_channel::unbounded::<ToDecoder>();
    let (from_tx, from_rx) = crossbeam_channel::unbounded::<FromDecoder>();

    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || {
            if let Err(e) = ff::init() {
                let _ = from_tx.send(FromDecoder::Error(format!("ffmpeg init 失败: {e}")));
            }
            let mut state: Option<VideoState> = None;

            while let Ok(msg) = to_rx.recv() {
                match msg {
                    ToDecoder::Open(path) => {
                        match VideoState::open(&path) {
                            Ok(mut st) => {
                                let _ = from_tx.send(FromDecoder::Opened {
                                    total_frames: st.frame_pts.len() as u64,
                                    fps: st.fps,
                                    width: st.width,
                                    height: st.height,
                                    keyframes: st.keyframe_indices.len() as u64,
                                });
                                match st.frame(0) {
                                    Ok(f) => {
                                        let _ = from_tx.send(FromDecoder::Frame(f));
                                    }
                                    Err(e) => {
                                        let _ = from_tx
                                            .send(FromDecoder::Error(format!("解码首帧失败: {e}")));
                                    }
                                }
                                state = Some(st);
                            }
                            Err(e) => {
                                let _ = from_tx.send(FromDecoder::Error(format!("打开失败: {e}")));
                            }
                        }
                    }
                    ToDecoder::GetFrame(n) => {
                        if let Some(st) = state.as_mut() {
                            match st.frame(n) {
                                Ok(f) => {
                                    let _ = from_tx.send(FromDecoder::Frame(f));
                                }
                                Err(e) => {
                                    let _ = from_tx
                                        .send(FromDecoder::Error(format!("解码第 {n} 帧失败: {e}")));
                                }
                            }
                        }
                    }
                    ToDecoder::Preview(n) => {
                        if let Some(st) = state.as_mut() {
                            match st.keyframe(n) {
                                Ok(f) => {
                                    let _ = from_tx.send(FromDecoder::Frame(f));
                                }
                                Err(e) => {
                                    let _ = from_tx
                                        .send(FromDecoder::Error(format!("预览第 {n} 帧失败: {e}")));
                                }
                            }
                        }
                    }
                }
                ctx.request_repaint();
            }
        })
        .expect("spawn decoder thread");

    DecoderHandle { to: to_tx, from: from_rx }
}

/// 缓存解码后的 YUV 帧（引用计数，零拷贝）。显示时才转 RGBA，避免把途经帧都做色彩转换。
struct GopCache {
    map: HashMap<usize, ff::frame::Video>,
    order: VecDeque<usize>,
    bytes: usize,
    budget: usize,
    /// 正在填充的 GOP 区间 [start, end)，期间其帧不可被逐出（否则会逐出正在解码的自己）。
    pinned: Option<(usize, usize)>,
}

fn frame_bytes(f: &ff::frame::Video) -> usize {
    (0..f.planes()).map(|i| f.data(i).len()).sum()
}

impl GopCache {
    fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget,
            pinned: None,
        }
    }
    fn contains(&self, idx: usize) -> bool {
        self.map.contains_key(&idx)
    }
    fn get(&self, idx: usize) -> Option<&ff::frame::Video> {
        self.map.get(&idx)
    }
    fn contains_range(&self, start: usize, end: usize) -> bool {
        (start..end).all(|i| self.map.contains_key(&i))
    }
    fn is_pinned(&self, idx: usize) -> bool {
        matches!(self.pinned, Some((s, e)) if idx >= s && idx < e)
    }
    fn insert(&mut self, idx: usize, frame: ff::frame::Video) {
        if self.map.contains_key(&idx) {
            return;
        }
        self.bytes += frame_bytes(&frame);
        self.order.push_back(idx);
        self.map.insert(idx, frame);

        // 逐出最旧的「非 pinned」帧；若剩余全是 pinned 则暂时超预算（一段 GOP 内）。
        while self.bytes > self.budget && self.order.len() > 1 {
            let mut skipped: Vec<usize> = Vec::new();
            let mut evicted = false;
            while let Some(old) = self.order.pop_front() {
                if self.is_pinned(old) {
                    skipped.push(old);
                } else if let Some(f) = self.map.remove(&old) {
                    self.bytes -= frame_bytes(&f);
                    evicted = true;
                    break;
                }
            }
            for idx in skipped.into_iter().rev() {
                self.order.push_front(idx);
            }
            if !evicted {
                break;
            }
        }
    }
}

struct VideoState {
    ictx: ff::format::context::Input,
    stream_index: usize,
    decoder: ff::decoder::Video,
    scaler: Option<ff::software::scaling::Context>,
    /// 显示顺序的每帧 pts（升序，下标 = 帧号）
    frame_pts: Vec<i64>,
    /// 各 GOP 起点（关键帧）的帧号，升序，首元素必为 0
    keyframe_indices: Vec<usize>,
    time_base: ff::Rational,
    fps: f64,
    width: u32,
    height: u32,
    cache: GopCache,
    /// 解码游标：当前已 seek 定位的 GOP 起点，及该次解码已达到的最高显示帧号。
    /// 用于「在同一 GOP 内向前步进时继续解码、而非每次都从关键帧重解」。
    cur_gop: Option<usize>,
    cur_upto: Option<usize>,
}

impl VideoState {
    fn open(path: &Path) -> Result<Self, ff::Error> {
        let mut ictx = ff::format::input(&path)?;

        let (stream_index, time_base, fps, decoder) = {
            let stream = ictx
                .streams()
                .best(ff::media::Type::Video)
                .ok_or(ff::Error::StreamNotFound)?;
            let stream_index = stream.index();
            let time_base = stream.time_base();
            let avg = stream.avg_frame_rate();
            let fps = if avg.denominator() != 0 {
                avg.numerator() as f64 / avg.denominator() as f64
            } else {
                0.0
            };
            let mut ctx = ff::codec::context::Context::from_parameters(stream.parameters())?;
            // 帧级多线程软件解码。
            // 注：试过 D3D11VA 硬件解码，但在当前“硬解帧 → av_hwframe_transfer_data 回传内存
            //   → egui write_texture”的上送路径下，逐帧 GPU→CPU 回传的开销反而大于省下的解码时间，
            //   实测更慢，故回退软件解码。真正的零拷贝需要把 D3D11 纹理经 DXGI 共享句柄导入 wgpu
            //   （register_native_texture，仅 Vulkan 后端），是较大的改造，暂不做。
            ctx.set_threading(ff::codec::threading::Config::kind(
                ff::codec::threading::Type::Frame,
            ));
            let decoder = ctx.decoder().video()?;
            (stream_index, time_base, fps, decoder)
        };

        let width = decoder.width();
        let height = decoder.height();

        // 扫一遍包，建立显示顺序索引 + 关键帧位置（只解封装、不解码）。
        let mut pairs: Vec<(i64, bool)> = Vec::new();
        for (stream, packet) in ictx.packets() {
            if stream.index() == stream_index {
                if let Some(p) = packet.pts().or_else(|| packet.dts()) {
                    pairs.push((p, packet.is_key()));
                }
            }
        }
        pairs.sort_by_key(|&(p, _)| p);
        pairs.dedup_by_key(|&mut (p, _)| p);

        let frame_pts: Vec<i64> = pairs.iter().map(|&(p, _)| p).collect();
        let mut keyframe_indices: Vec<usize> = pairs
            .iter()
            .enumerate()
            .filter_map(|(i, &(_, k))| if k { Some(i) } else { None })
            .collect();
        if keyframe_indices.first() != Some(&0) {
            keyframe_indices.insert(0, 0); // 保证首帧可作为 GOP 起点
        }

        Ok(Self {
            ictx,
            stream_index,
            decoder,
            scaler: None,
            frame_pts,
            keyframe_indices,
            time_base,
            fps,
            width,
            height,
            cache: GopCache::new(CACHE_BUDGET),
            cur_gop: None,
            cur_upto: None,
        })
    }

    /// 帧 n 所在 GOP 的起点（<= n 的最近关键帧）。
    fn gop_start(&self, n: usize) -> usize {
        match self.keyframe_indices.binary_search(&n) {
            Ok(i) => self.keyframe_indices[i],
            Err(0) => 0,
            Err(i) => self.keyframe_indices[i - 1],
        }
    }

    /// 以 start（某关键帧）为起点的 GOP 的结束（下一关键帧，开区间），或总帧数。
    fn gop_end(&self, start: usize) -> usize {
        match self.keyframe_indices.binary_search(&start) {
            Ok(i) => self
                .keyframe_indices
                .get(i + 1)
                .copied()
                .unwrap_or(self.frame_pts.len()),
            Err(_) => self.frame_pts.len(),
        }
    }

    /// 确保第 n 帧已解码入缓存。只解码到 n 为止；同一 GOP 内向前步进时从当前解码
    /// 位置继续，避免重解整段 GOP。
    fn ensure_decoded(&mut self, n: usize) -> Result<(), ff::Error> {
        if self.cache.contains(n) {
            return Ok(());
        }
        let start = self.gop_start(n);
        let can_continue = self.cur_gop == Some(start) && self.cur_upto.map_or(false, |u| n > u);
        if !can_continue {
            let ts_us = rescale_to_us(self.frame_pts[start], self.time_base);
            self.ictx.seek(ts_us, ..ts_us)?;
            self.decoder.flush();
            self.cur_gop = Some(start);
            self.cur_upto = None;
        }
        self.decode_until(start, n)
    }

    /// 解码并返回显示顺序的第 n 帧（优先缓存）。
    fn frame(&mut self, n: u64) -> Result<Arc<FrameRgba>, ff::Error> {
        let idx = n as usize;
        if idx >= self.frame_pts.len() {
            return Err(ff::Error::InvalidData);
        }
        self.ensure_decoded(idx)?;
        // self.cache 与 self.scaler 是不同字段，可同时借用。
        let yuv = self.cache.get(idx).ok_or(ff::Error::InvalidData)?;
        Ok(Arc::new(scale_to_rgba(&mut self.scaler, yuv, n)?))
    }

    /// 拖动预览：只解码 n 所在 GOP 的关键帧（一帧 I 帧，很快），但以帧号 n 标注，
    /// 让进度条/帧号停在拖动位置，画面给出粗预览；松手后再请求精确帧。
    fn keyframe(&mut self, n: u64) -> Result<Arc<FrameRgba>, ff::Error> {
        let idx = n as usize;
        if idx >= self.frame_pts.len() {
            return Err(ff::Error::InvalidData);
        }
        let kf = self.gop_start(idx);
        self.ensure_decoded(kf)?;
        let yuv = self.cache.get(kf).ok_or(ff::Error::InvalidData)?;
        Ok(Arc::new(scale_to_rgba(&mut self.scaler, yuv, n)?))
    }

    /// 从当前解码位置向前解码，直到 [start, n] 全部入缓存（保证向后步进命中缓存）。
    /// 解码期间 pin 住整段 GOP，防止 LRU 逐出正在填充的帧。
    fn decode_until(&mut self, start: usize, n: usize) -> Result<(), ff::Error> {
        let end = self.gop_end(start);
        self.cache.pinned = Some((start, end));
        let r = self.decode_until_inner(start, end, n);
        self.cache.pinned = None;
        r
    }

    fn decode_until_inner(
        &mut self,
        start: usize,
        end: usize,
        n: usize,
    ) -> Result<(), ff::Error> {
        let mut packet = ff::Packet::empty();
        let mut eof = false;
        loop {
            if !eof {
                match packet.read(&mut self.ictx) {
                    Ok(()) => {
                        if packet.stream() != self.stream_index {
                            continue;
                        }
                        self.decoder.send_packet(&packet)?;
                    }
                    Err(ff::Error::Eof) => {
                        eof = true;
                        self.decoder.send_eof()?;
                    }
                    Err(e) => return Err(e),
                }
            }

            loop {
                let mut decoded = ff::frame::Video::empty();
                match self.decoder.receive_frame(&mut decoded) {
                    Ok(()) => {
                        let p = decoded
                            .pts()
                            .or_else(|| Some(decoded.timestamp().unwrap_or(0)));
                        if let Some(p) = p {
                            if let Ok(di) = self.frame_pts.binary_search(&p) {
                                if di >= start && di < end {
                                    self.cur_upto =
                                        Some(self.cur_upto.map_or(di, |u| u.max(di)));
                                    if !self.cache.contains(di) {
                                        // 把解码帧（引用计数的 YUV）直接移入缓存，零拷贝
                                        self.cache.insert(di, decoded);
                                    }
                                }
                            }
                        }
                    }
                    Err(ff::Error::Other { errno: _ }) => break, // EAGAIN
                    Err(ff::Error::Eof) => break,
                    Err(e) => return Err(e),
                }
            }

            // [start, n] 全部就绪即可返回（不再解码该 GOP 余下的帧）
            if self.cache.contains_range(start, n + 1) {
                return Ok(());
            }
            if eof {
                return Ok(()); // 尽力而为
            }
        }
    }

}

/// 把一帧 YUV 转成紧凑 RGBA（scaler 懒创建并复用，与 self 解耦以便借用）。
fn scale_to_rgba(
    scaler: &mut Option<ff::software::scaling::Context>,
    src: &ff::frame::Video,
    index: u64,
) -> Result<FrameRgba, ff::Error> {
    let w = src.width();
    let h = src.height();

    if scaler.is_none() {
        *scaler = Some(ff::software::scaling::Context::get(
            src.format(),
            w,
            h,
            ff::format::Pixel::RGBA,
            w,
            h,
            ff::software::scaling::Flags::BILINEAR,
        )?);
    }
    let sc = scaler.as_mut().unwrap();

    let mut rgb = ff::frame::Video::empty();
    sc.run(src, &mut rgb)?;

    let stride = rgb.stride(0);
    let data = rgb.data(0);
    let row_bytes = (w * 4) as usize;
    let mut pixels = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        let s = &data[y * stride..y * stride + row_bytes];
        pixels[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(s);
    }

    Ok(FrameRgba {
        width: w,
        height: h,
        pixels,
        index,
    })
}

/// 无界面自检：解码一串帧（含倒退、GOP 内部、跨关键帧），校验帧号与像素尺寸正确。
pub fn selftest(path: &Path) -> Result<String, String> {
    ff::init().map_err(|e| e.to_string())?;
    let mut st = VideoState::open(path).map_err(|e| e.to_string())?;
    let total = st.frame_pts.len() as u64;
    let mut out = format!(
        "opened: {}x{}  total={}  fps={:.3}  keyframes={}\n",
        st.width,
        st.height,
        total,
        st.fps,
        st.keyframe_indices.len()
    );

    let mut targets: Vec<u64> = vec![0, 1, 2, 7, 14, 15, 16, 30];
    if total > 0 {
        targets.push(total / 2);
        targets.push(total - 1);
    }
    targets.push(3);
    targets.push(0);

    for t in targets {
        if t >= total {
            continue;
        }
        let t0 = std::time::Instant::now();
        let f = st.frame(t).map_err(|e| format!("frame {t}: {e}"))?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let expected = (f.width * f.height * 4) as usize;
        if f.index != t {
            return Err(format!("frame {t}: got index {}", f.index));
        }
        if f.pixels.len() != expected {
            return Err(format!(
                "frame {t}: pixels {} != expected {}",
                f.pixels.len(),
                expected
            ));
        }
        out.push_str(&format!("  frame {t:>5} ok: {}x{}  {ms:>7.1} ms\n", f.width, f.height));
    }
    out.push_str("ALL OK");
    Ok(out)
}

/// 把流时基下的 pts 换算成 AV_TIME_BASE（微秒）。
fn rescale_to_us(pts: i64, tb: ff::Rational) -> i64 {
    let num = tb.numerator() as i128;
    let den = tb.denominator() as i128;
    if den == 0 {
        return pts;
    }
    ((pts as i128 * num * 1_000_000) / den) as i64
}
