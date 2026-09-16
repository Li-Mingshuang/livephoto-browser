//! 解码层。
//!
//! M0 阶段只覆盖 HEIC/JPEG 的 WIC 解码（其余在后续里程碑补齐）。

pub mod mf;
pub mod wic;

use anyhow::Result;

/// 在当前线程初始化 COM（MTA）。
///
/// 已经是别的套间模型（例如 Tauri/WebView2 在主线程初始化的 STA）时不算失败，
/// 只是不能再改模式，因此 `RPC_E_CHANGED_MODE` 被显式放过。
#[cfg(windows)]
pub fn com_init() -> Result<()> {
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            hr.ok()?;
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn com_init() -> Result<()> {
    Ok(())
}

/// 在当前线程初始化 COM（STA / 单线程套间）。
///
/// **这是 HEIC 解码的硬性前提。** 实测（`m0-results/`）：Windows 的 HEIF 扩展
/// 提供的 WIC 解码器在 MTA 套间下调用 `CreateDecoderFromFilename` 会**永久阻塞**
/// （进程 0 CPU），在 STA 下正常。因为该解码器内部会回调调用方套间，
/// 而只有 STA 的阻塞等待（CoWaitForMultipleHandles）才会顺带泵消息。
///
/// 因此每个解码工作线程都必须是独立的 STA，并且各自持有自己的 COM 对象
/// （STA 对象不能跨线程直接使用）。详见 `wic::thread_factory`。
#[cfg(windows)]
pub fn com_init_sta() -> Result<()> {
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            hr.ok()?;
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn com_init_sta() -> Result<()> {
    Ok(())
}

/// 进程级初始化 Media Foundation。整个进程调用一次即可。
///
/// 除了 HEVC 视频解码本身需要它，实测对 HEIC 解码也有明显收益：
/// 未调用时 12MP HEIC 解码 ≈654ms，调用后 ≈316ms
/// （HEIF 内部的 HEVC 图层得以走 MF 硬解路径）。
#[cfg(windows)]
pub fn mf_startup() -> Result<()> {
    use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| anyhow::anyhow!("MFStartup 失败: {e}"))?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn mf_startup() -> Result<()> {
    Ok(())
}

/// 释放 Media Foundation（进程退出前调用）。
#[cfg(windows)]
pub fn mf_shutdown() {
    use windows::Win32::Media::MediaFoundation::MFShutdown;
    unsafe {
        let _ = MFShutdown();
    }
}

#[cfg(not(windows))]
pub fn mf_shutdown() {}

/// 回收当前线程的 COM 套间：先 `CoUninitialize` 再重新 `com_init_sta`。
///
/// 必须在同一个线程上成对调用（COM 的要求）。用途见 `wic::reset_thread_factory`。
#[cfg(windows)]
pub fn com_reset_sta() -> Result<()> {
    use windows::Win32::System::Com::CoUninitialize;
    unsafe {
        CoUninitialize();
    }
    com_init_sta()
}

#[cfg(not(windows))]
pub fn com_reset_sta() -> Result<()> {
    Ok(())
}

/// **解码工作线程的入口初始化**：本线程 STA + 进程级 `MFStartup`。
///
/// 这是所有解码线程必须调用的第一步。两条禁令（都踩过坑）：
///
/// - **不要在 Tauri/tao 主线程上调 `com_init()`（MTA）**：tao 创建窗口时会调
///   `OleInitialize`（要求 STA），套间模式已被改成 MTA 就会直接 panic：
///   `OleInitialize failed! Result was: RPC_E_CHANGED_MODE`。
/// - 也不要指望主线程是 STA 就能共用：STA 的 COM 对象不能跨线程使用，
///   每个工作线程必须是自己的 STA 并各自持有 WIC 工厂（`wic::with_factory`）。
pub fn worker_init() -> Result<()> {
    com_init_sta()?;
    static MF_ONCE: std::sync::Once = std::sync::Once::new();
    let mut result = Ok(());
    MF_ONCE.call_once(|| {
        result = mf_startup();
    });
    result
}
