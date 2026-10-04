//! 语音听写（P10-M1：核心链路）。
//!
//! 分工（对齐 xime-speech 的设计注释）：纯推理在 [`xime_speech`]（sherpa-onnx
//! 流式 zipformer，端点检测内建）；本模块负责宿主侧三件事——
//!
//! 1. **模型管理**：首次使用自动从 ModelScope 下载 tar.bz2 → 解压 → 校验四
//!    文件，落 `~/.local/share/xime/models/<id>/`；
//! 2. **音频采集**：PulseAudio 简单 API（16kHz 单声道 s16le；KDE 的
//!    PipeWire 经 pipewire-pulse 兼容此 API），`dynamic` 特性运行时 dlopen，
//!    构建不依赖 libpulse-dev；
//! 3. **会话线程**：单个 worker 独占 [`StreamingRecognizer`]（非 Send 共享
//!    语义），命令进 / 事件出的信箱模式——daemon 主循环只消费事件做
//!    上屏与候选栏反馈，识别/下载/解压都不碰主线程（UI 冻结纪律）。
//!
//! 交互（对齐 XimeYao 设置页说明的设计意图）：点候选栏 🎙️ 开始听写，
//! 停顿时自动上屏（端点检测断句），再点 🎙️ 结束。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use tracing::{debug, error, info};
use xime_speech::{AsrModelProfile, AsrModelRegistry, SpeechConfig, StreamingRecognizer};

// ── PulseAudio 简单 API 的内嵌 dlopen 绑定 ──────────────────────────
// 只用三个函数（new/read/free），运行时加载 libpulse-simple.so.0——KDE 的
// PipeWire 经 pipewire-pulse 兼容此 API；构建零系统依赖，缺失时给出可读错误。

/// pa_sample_spec（simple.h）：format(u32) + rate(u32) + channels(u8)，C 对齐。
#[repr(C)]
struct PaSampleSpec {
    format: u32,
    rate: u32,
    channels: u8,
}

/// PA_SAMPLE_S16NE（本机字节序 s16）。
const PA_SAMPLE_S16NE: u32 = 3;
/// PA_STREAM_RECORD。
const PA_STREAM_RECORD: u32 = 2;

/// libpulse-simple 运行时绑定（占用即独占录音流，Drop 释放）。
struct PulseCapture {
    _lib: libloading::Library,
    handle: *mut std::ffi::c_void,
    read_fn: unsafe extern "C" fn(*mut std::ffi::c_void, *mut u8, usize, *mut i32) -> i32,
    error: i32,
}

impl PulseCapture {
    /// 打开默认录音源（16kHz mono s16ne）。
    fn new() -> Result<Self, String> {
        type PaSimpleNew = unsafe extern "C" fn(
            *const std::ffi::c_char,
            *const std::ffi::c_char,
            u32,
            *const std::ffi::c_char,
            *const std::ffi::c_char,
            *const PaSampleSpec,
            *const std::ffi::c_void,
            *const std::ffi::c_void,
            *mut i32,
        ) -> *mut std::ffi::c_void;
        type PaSimpleFree = unsafe extern "C" fn(*mut std::ffi::c_void);
        type PaSimpleRead =
            unsafe extern "C" fn(*mut std::ffi::c_void, *mut u8, usize, *mut i32) -> i32;

        unsafe {
            let lib = libloading::Library::new("libpulse-simple.so.0").map_err(|e| {
                format!("未找到 libpulse-simple（需要 pipewire-pulse 或 pulseaudio）：{e}")
            })?;
            let new_fn: libloading::Symbol<PaSimpleNew> = lib
                .get(b"pa_simple_new")
                .map_err(|e| format!("libpulse-simple 缺少 pa_simple_new：{e}"))?;
            let read_fn: libloading::Symbol<PaSimpleRead> = lib
                .get(b"pa_simple_read")
                .map_err(|e| format!("libpulse-simple 缺少 pa_simple_read：{e}"))?;
            let free_fn: libloading::Symbol<PaSimpleFree> =
                lib.get(b"pa_simple_free").map_err(|e| format!("{e}"))?;

            let spec = PaSampleSpec {
                format: PA_SAMPLE_S16NE,
                rate: SAMPLE_RATE,
                channels: CHANNELS as u8,
            };
            let cstr = |s: &str| std::ffi::CString::new(s).unwrap_or_default();
            let handle = new_fn(
                std::ptr::null(), // server = 默认
                cstr("xime-daemon").as_ptr(),
                PA_STREAM_RECORD,
                std::ptr::null(), // 设备 = 默认
                cstr("xime-dictation").as_ptr(),
                &spec,
                std::ptr::null(), // channel map
                std::ptr::null(), // buffer attr
                &mut 0,
            );
            if handle.is_null() {
                return Err("麦克风打开失败：请检查输入设备与 pipewire-pulse 服务".into());
            }
            // Symbol 借用 lib；把函数指针拷出来后 lib 一起存进结构体。
            let read_fn: PaSimpleRead = *read_fn;
            let _ = free_fn;
            Ok(Self {
                _lib: lib,
                handle,
                read_fn,
                error: 0,
            })
        }
    }

    /// 读取一块 s16 样本（字节缓冲，调用方转 i16）。
    fn read(&mut self, buf: &mut [u8]) -> Result<(), String> {
        unsafe {
            if (self.read_fn)(self.handle, buf.as_mut_ptr(), buf.len(), &mut self.error) < 0 {
                return Err("录音读取失败（麦克风被占用或已断开？）".into());
            }
        }
        Ok(())
    }
}

impl Drop for PulseCapture {
    fn drop(&mut self) {
        type PaSimpleFree = unsafe extern "C" fn(*mut std::ffi::c_void);
        unsafe {
            if let Ok(free_fn) = self._lib.get::<PaSimpleFree>(b"pa_simple_free") {
                free_fn(self.handle);
            }
        }
    }
}

/// 发给 worker 的命令。
#[derive(Debug)]
pub enum SpeechCommand {
    /// 🎙️ 点击：Idle → 开始（无模型先下载）；Listening → 结束上屏。
    Toggle,
    /// daemon 退出：停采集、结束线程。
    Shutdown,
}

/// worker 发给主循环的事件（主循环负责上屏 / 候选栏反馈 / 桌面通知）。
#[derive(Debug, Clone)]
pub enum SpeechEvent {
    /// 状态迁移（驱动候选栏样式与托盘反馈）。
    State(SpeechState),
    /// 听写中的中间文本（候选栏实时显示）。
    Partial(String),
    /// 一句识别完成（端点断句或停止收尾），主循环 commit_string 上屏。
    Committed(String),
    /// 失败（下载/装载/采集），文案已面向用户。
    Error(String),
}

/// 听写状态机。
#[derive(Debug, Clone, PartialEq)]
pub enum SpeechState {
    Idle,
    /// 模型下载进度 0.0~1.0。
    Downloading(f32),
    /// 模型装载中（首次 ~秒级）。
    Loading,
    Listening,
}

/// 采集参数：16kHz 单声道，每块 1024 样本 ≈ 64ms。
const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u16 = 1;
const BLOCK_SAMPLES: usize = 1024;

/// 模型数据根（对齐 XimeChe 数据目录约定：rime-data 同在 ~/.local/share/xime）。
fn models_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    Path::new(&home).join(".local/share/xime/models")
}

#[cfg(test)]
fn models_root_for_test() -> PathBuf {
    std::env::temp_dir().join(format!("xime-speech-test-{}", std::process::id()))
}

/// 模型目录是否就绪：profile 要求的四个文件全部存在且非空。
fn is_model_ready(profile: &AsrModelProfile, dir: &Path) -> bool {
    [
        profile.encoder_file.as_str(),
        profile.decoder_file.as_str(),
        profile.joiner_file.as_str(),
        profile.tokens_file.as_str(),
    ]
    .iter()
    .all(|name| {
        dir.join(name).is_file() && std::fs::metadata(dir.join(name)).is_ok_and(|m| m.len() > 0)
    })
}

/// 下载 tar.bz2（流式，进度经 `on_progress` 上报 0.0~1.0）。
fn download_archive(
    profile: &AsrModelProfile,
    dest: &Path,
    on_progress: &dyn Fn(f32),
) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    info!(
        "Downloading ASR model '{}' from {}",
        profile.id, profile.download_url
    );
    // ModelScope 会 403 掉无 UA 的客户端，带一个正常 UA。
    let client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) xime-input-method")
        .build()?;
    let mut resp = client.get(&profile.download_url).send()?;
    if !resp.status().is_success() {
        anyhow::bail!("模型下载失败：HTTP {}", resp.status());
    }
    let total = resp.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(dest)?;
    let mut downloaded: u64 = 0;
    let mut last_reported = 0.0f32;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
        downloaded += n as u64;
        if total > 0 {
            let progress = downloaded as f32 / total as f32;
            // 每 5% 上报一次，避免事件风暴。
            if progress - last_reported >= 0.05 {
                last_reported = progress;
                on_progress(progress);
            }
        }
    }
    file.flush()?;
    on_progress(1.0);
    info!("ASR model downloaded: {downloaded} bytes");
    Ok(())
}

/// 解压 tar.bz2 并把 profile 要求的四个文件归位到 `model_dir`（发布包内
/// 通常带一层日期目录，递归扫描按文件名找、按角色拷贝到目标根）。
fn extract_archive(
    archive: &Path,
    profile: &AsrModelProfile,
    model_dir: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(model_dir)?;
    let bz = bzip2::read::BzDecoder::new(std::fs::File::open(archive)?);
    let mut tar = tar::Archive::new(bz);
    let work = model_dir.join("_extract");
    std::fs::create_dir_all(&work)?;
    tar.unpack(&work)?;

    for file_name in [
        &profile.encoder_file,
        &profile.decoder_file,
        &profile.joiner_file,
        &profile.tokens_file,
    ] {
        let target = model_dir.join(file_name);
        let found = find_file_recursive(&work, file_name)?;
        let Some(found) = found else {
            anyhow::bail!("压缩包里找不到 {}", file_name);
        };
        std::fs::copy(&found, &target)?;
    }
    std::fs::remove_dir_all(&work)?;
    Ok(())
}

/// 递归找文件名精确匹配的第一个文件。
fn find_file_recursive(root: &Path, name: &str) -> anyhow::Result<Option<PathBuf>> {
    let mut out = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == name) {
                out = Some(path);
                break;
            }
        }
        if out.is_some() {
            break;
        }
    }
    Ok(out)
}

/// 下载 + 解压 + 校验，返回就绪的模型目录。
fn ensure_model(profile: &AsrModelProfile, on_progress: &dyn Fn(f32)) -> anyhow::Result<PathBuf> {
    let dir = models_root().join(&profile.id);
    if is_model_ready(profile, &dir) {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)?;
    let archive = dir.join("_download.tar.bz2");
    let result = (|| -> anyhow::Result<()> {
        download_archive(profile, &archive, on_progress)?;
        extract_archive(&archive, profile, &dir)?;
        std::fs::remove_file(&archive).ok();
        if !is_model_ready(profile, &dir) {
            anyhow::bail!("解压后模型文件不齐");
        }
        Ok(())
    })();
    if let Err(e) = result {
        std::fs::remove_dir_all(&dir).ok();
        return Err(e);
    }
    info!("ASR model '{}' ready at {}", profile.id, dir.display());
    Ok(dir)
}

/// 会话桥：daemon 主循环持有（发命令 + 轮询事件）。
///
/// 全部方法 `&self`（daemon 的各事件处理器都是不可变借用），内部可变性：
/// 命令通道与状态缓存用 Mutex，事件接收端 `try_recv` 本身就是 `&self`。
pub struct SpeechBridge {
    cmd_tx: std::sync::Mutex<Option<Sender<SpeechCommand>>>,
    event_rx: Receiver<SpeechEvent>,
    /// 主循环侧缓存的（状态，最新 partial）。
    view: std::sync::Mutex<(SpeechState, String)>,
}

impl SpeechBridge {
    /// 启动 worker 线程（进程生命周期内常驻）。
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SpeechCommand>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<SpeechEvent>();
        std::thread::Builder::new()
            .name("xime-speech".into())
            .spawn(move || worker_loop(cmd_rx, event_tx))
            .expect("spawn speech worker");
        Self {
            cmd_tx: std::sync::Mutex::new(Some(cmd_tx)),
            event_rx,
            view: std::sync::Mutex::new((SpeechState::Idle, String::new())),
        }
    }

    /// 🎙️ 切换（不阻塞，结果经事件回）。
    pub fn toggle(&self) {
        let mut slot = self.cmd_tx.lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_ref() {
            Some(tx) => {
                if tx.send(SpeechCommand::Toggle).is_err() {
                    error!("speech worker gone");
                    *slot = None;
                }
            }
            None => error!("speech worker already stopped"),
        }
    }

    /// 主循环每轮拉取事件（副作用：上屏/候选栏/通知都在调用方做）。
    pub fn drain_events(&self, mut on_event: impl FnMut(SpeechEvent)) {
        let mut view = self.view.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => {
                    match &event {
                        SpeechEvent::State(state) => {
                            view.0 = state.clone();
                            if *state != SpeechState::Listening {
                                view.1.clear();
                            }
                        }
                        SpeechEvent::Partial(text) => view.1 = text.clone(),
                        _ => {}
                    }
                    on_event(event);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    *self.cmd_tx.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    break;
                }
            }
        }
    }

    pub fn state(&self) -> SpeechState {
        self.view
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .clone()
    }

    #[allow(dead_code)]
    pub fn partial(&self) -> String {
        self.view
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .1
            .clone()
    }

    /// daemon 退出时停线程（退出路径上没有恢复手段，错误忽略）。
    pub fn shutdown(&self) {
        let mut slot = self.cmd_tx.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = slot.take() {
            let _ = tx.send(SpeechCommand::Shutdown);
        }
    }
}

impl Drop for SpeechBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// worker 主循环：Idle 时阻塞等命令；一次 Toggle 跑完整场听写会话。
fn worker_loop(cmd_rx: Receiver<SpeechCommand>, event_tx: Sender<SpeechEvent>) {
    let profile = AsrModelRegistry::default_profile();
    loop {
        // 等下一条命令（空闲时不占 CPU）。
        let Ok(cmd) = cmd_rx.recv() else {
            return; // 主循环退出
        };
        match cmd {
            SpeechCommand::Shutdown => return,
            SpeechCommand::Toggle => {
                let _ = event_tx.send(SpeechEvent::State(SpeechState::Loading));
                match run_listening_session(&profile, &cmd_rx, &event_tx) {
                    Ok(()) => info!("Speech session ended normally"),
                    Err(e) => {
                        error!("Speech session failed: {e:#}");
                        let _ = event_tx.send(SpeechEvent::Error(format!("{e:#}")));
                    }
                }
                let _ = event_tx.send(SpeechEvent::State(SpeechState::Idle));
            }
        }
    }
}

/// 一次听写会话：确保模型 → 装载 → 采集识别 → Stop 收尾。
/// 返回 Ok 表示正常结束（含用户主动停止）；Err 为可告知用户的失败。
fn run_listening_session(
    profile: &AsrModelProfile,
    cmd_rx: &Receiver<SpeechCommand>,
    event_tx: &Sender<SpeechEvent>,
) -> anyhow::Result<()> {
    // 1. 模型就绪（下载进度上报；已在则直接用）。
    let dir = {
        let dir = models_root().join(&profile.id);
        if is_model_ready(profile, &dir) {
            dir
        } else {
            let _ = event_tx.send(SpeechEvent::State(SpeechState::Downloading(0.0)));
            ensure_model(profile, &|p: f32| {
                let _ = event_tx.send(SpeechEvent::State(SpeechState::Downloading(p)));
            })?
        }
    };

    // 2. 装载（首次秒级）。
    let mut recognizer = StreamingRecognizer::open(profile, &dir, &SpeechConfig::default())
        .map_err(|e| anyhow::anyhow!("语音引擎装载失败：{e:?}"))?;

    // 3. 采集（PulseAudio simple record：16k mono s16ne，dlopen 运行时绑定）。
    let mut capture = PulseCapture::new().map_err(|e| anyhow::anyhow!(e))?;

    let _ = event_tx.send(SpeechEvent::State(SpeechState::Listening));
    let mut raw = vec![0u8; BLOCK_SAMPLES * 2];
    loop {
        // 停止命令优先检查（read 是阻塞点，块间隔 ~64ms 检查一次）。
        match cmd_rx.try_recv() {
            Ok(SpeechCommand::Toggle | SpeechCommand::Shutdown) => break,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }

        capture.read(&mut raw).map_err(|e| anyhow::anyhow!(e))?;
        // s16ne = 本机字节序 i16。
        let block: Vec<i16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_ne_bytes(*pair))
            .collect();
        recognizer.accept_pcm16(SAMPLE_RATE as i32, &block);

        let _ = event_tx.send(SpeechEvent::Partial(recognizer.partial_text()));

        // 停顿自动上屏：端点检测命中 → 当句提交、继续听下一句。
        if recognizer.is_endpoint() {
            let text = recognizer.partial_text();
            if !text.trim().is_empty() {
                let _ = event_tx.send(SpeechEvent::Committed(text));
            }
            recognizer.reset();
            let _ = event_tx.send(SpeechEvent::Partial(String::new()));
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    // 4. 收尾：finalize 冲出未断句的尾巴。
    drop(capture);
    let tail = recognizer.finalize();
    if !tail.trim().is_empty() {
        let _ = event_tx.send(SpeechEvent::Committed(tail));
    }
    debug!("Speech capture stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_files_check_shape() {
        let profile = AsrModelRegistry::default_profile();
        // 四个角色文件名非空（is_model_ready 的契约）。
        assert!(!profile.encoder_file.is_empty());
        assert!(!profile.tokens_file.is_empty());
    }

    #[test]
    fn is_model_ready_rejects_incomplete_dir() {
        let profile = AsrModelRegistry::default_profile();
        let dir = models_root_for_test().join("incomplete");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_model_ready(&profile, &dir), "空目录不就绪");
        std::fs::write(dir.join(&profile.encoder_file), b"x").unwrap();
        assert!(!is_model_ready(&profile, &dir), "只差一个文件也不就绪");
        std::fs::remove_dir_all(models_root_for_test()).ok();
    }

    #[test]
    fn bridge_starts_idle_and_drains() {
        let bridge = SpeechBridge::spawn();
        assert_eq!(bridge.state(), SpeechState::Idle);
        bridge.drain_events(|_| {}); // 无事件不 panic
        bridge.shutdown();
    }
}
