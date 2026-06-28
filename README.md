# framePlayer

一个轻量的 Windows **逐帧** MP4 播放器（Rust 编写）。可以**真正逐帧**（不是只跳关键帧）地前后播放，支持 H.264 / H.265 / AV1，并提供**系统级全局快捷键**——即使焦点不在播放器上也能控制。

> 目标：**不卡**。按住切帧连续滚动、松手立刻停止；点任意帧即时跳转。

## 功能

- **逐帧前进 / 后退**：每一帧都能停，不止关键帧。
- **可调播放速度**：以「每帧之间的秒数」设置（支持小数，如 0.04s）。
- **全局快捷键**（`RegisterHotKey`，焦点在别的程序也生效）：
  - `Ctrl+Alt+Space` 播放 / 暂停
  - `Ctrl+Alt+← / →` 上一帧 / 下一帧（**按住连续滚动，松手立即停止**，连播速度可调）
  - `Ctrl+Alt+T` 窗口置顶 · `Ctrl+Alt+F` 沉浸模式
  - 全部可在设置里重新绑定
- **本地快捷键**（焦点在播放器时）：`Space` 播放暂停 · `← / →` 切帧 · `Tab` 沉浸模式 · `Esc` 退出沉浸 · **双击画面去黑边**
- **加速代理（全 I 帧）**：打开长 GOP 视频后自动后台转码出一份「每帧都是关键帧」的副本，之后跳任意帧 ~15ms 即时；带缓存与可配置上限，同一文件不重复转码。
- **多窗口**：标题栏 `+` 新建独立窗口；全局快捷键只作用于最近激活的那个窗口。
- 拖拽打开 · 打开即按视频比例自适应窗口去黑边 · 自绘无边框界面（深色 + 琥珀）。

## 下载

到 [Releases](https://github.com/ccdr4gon/framePlayer/releases) 下载 `framePlayer-vX.Y.Z-win64.zip`，解压后运行 `frame_player.exe` 即可（FFmpeg 运行库已随包附带，无需另装）。仅 Windows x64。

> 提示：`Ctrl+Alt+Space` 可能与其它软件（如 Claude 桌面端）的全局快捷键冲突——若无反应，在设置里换一个组合键即可。

## 从源码构建

需要：
- Rust（stable，MSVC toolchain）+ Visual Studio Build Tools（VC++）
- **FFmpeg 7.1.x「shared」开发包**（推荐 gyan.dev 的 `ffmpeg-7.1.1-full_build-shared`）。**必须是 7.1.x**——8.x 会让 `ffmpeg-next` 的安全层报非穷尽 match 编译错误。
- LLVM（`bindgen` 需要 libclang）

步骤：
1. 复制 `.cargo/config.toml.example` 为 `.cargo/config.toml`，填入你的 `FFMPEG_DIR` 与 `LIBCLANG_PATH`。
2. `./build.ps1 build --release`（该脚本会激活 vcvars64 并把 FFmpeg 的 DLL + ffmpeg.exe 复制到 `target/release` 旁）。
3. 运行 `target/release/frame_player.exe`。

## 技术栈

`eframe`/`egui` 0.35（wgpu）UI · `ffmpeg-next` + FFmpeg 7.1.1 解码（GOP 缓存 + 解码游标，多线程软解，逐帧准确）· `RegisterHotKey` 全局热键 · `crossbeam-channel` 线程通信。

## 许可 / FFmpeg

本仓库随发布包附带 FFmpeg 共享运行库（gyan.dev 的 full build，**GPL**）。FFmpeg 源码与许可见 <https://ffmpeg.org> 与 <https://www.gyan.dev/ffmpeg/builds/>。
