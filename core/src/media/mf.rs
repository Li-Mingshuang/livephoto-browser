//! Media Foundation 视频解码：抽帧与转码。
//!
//! Live Photo 的视频本体是 HEVC（`hvc1`）1920×1440、时长 1.8~2.7 秒。
//! 用 MF 而不是打包 FFmpeg 的收益：
//!   - 零打包体积（走系统 `Microsoft.HEVCVideoExtension`）；
//!   - 能吃到 GPU 硬解（DXVA），这是 HEIC 解码（纯软件 ≈460ms/张）完全比不了的。
//!
//! 注意：MF 需要 STA 套间初始化（与 HEIC 解码同因），见 `media::com_init_sta`。

use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{GUID, Interface, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFVideoFormat_RGB32,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_I8;

use super::wic::{fit_within, Bitmap, PixelLayout};

/// 首个视频流的索引常量（`MF_SOURCE_READER_FIRST_VIDEO_STREAM` = -4）。
const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// 源视频的基本信息。
#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub w: u32,
    pub h: u32,
    /// 原生输出的 4CC（如 `hvc1` / `avc1`）。
    pub codec: String,
}

/// 读取首个视频流的原生尺寸与编码格式（不解码）。
pub fn probe(path: &Path) -> Result<VideoInfo> {
    let wide = wide_path(path);
    unsafe {
        let reader = create_reader(path, &wide, false)?;
        let native: IMFMediaType = reader
            .GetNativeMediaType(FIRST_VIDEO, 0)
            .with_context(|| format!("取不到视频流信息：{}", path.display()))?;
        let fs = native.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let subtype = native.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        Ok(VideoInfo {
            w: (fs >> 32) as u32,
            h: (fs & 0xffff_ffff) as u32,
            codec: fourcc(&subtype),
        })
    }
}

/// 抽一帧。
///
/// - `time_ms = None`：取第一帧（真实产品里对应"打开即出画"的最坏情况）
/// - `time_ms = Some(t)`：先 seek 到 t 再取最近的帧
/// - `max_dim`：让 MF 的视频处理器（Video Processor MFT）直接输出缩放后的尺寸，
///   避免解全尺寸再缩放
pub fn extract_frame(path: &Path, time_ms: Option<u64>, max_dim: Option<u32>) -> Result<Bitmap> {
    let wide = wide_path(path);
    unsafe {
        let reader = create_reader(path, &wide, true)?;

        // 原生尺寸，用于算等比缩放后的目标尺寸
        let native: IMFMediaType = reader.GetNativeMediaType(FIRST_VIDEO, 0)?;
        let fs = native.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let (nw, nh) = ((fs >> 32) as u32, (fs & 0xffff_ffff) as u32);
        let (tw, th) = match max_dim {
            Some(m) if nw > 0 && nh > 0 => fit_within(nw, nh, m),
            _ => (nw, nh),
        };

        // 让解码器/视频处理器输出 RGB32（内存序为 BGRA，低位是 B）
        let out_type: IMFMediaType = MFCreateMediaType()?;
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        if tw > 0 && th > 0 {
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, ((tw as u64) << 32) | th as u64)?;
        }
        reader.SetCurrentMediaType(FIRST_VIDEO, None, &out_type)?;

        if let Some(ms) = time_ms {
            seek(&reader, ms)?;
        }

        let sample = read_next_sample(&reader)?;
        sample_to_bitmap(&sample, tw, th)
    }
}

/// 连续解码整段，返回（帧数，总耗时由调用方计时）。
/// 用于量"整段解码吞吐"。
pub fn decode_all_frames(path: &Path, max_dim: Option<u32>) -> Result<usize> {
    let wide = wide_path(path);
    unsafe {
        let reader = create_reader(path, &wide, true)?;
        let native: IMFMediaType = reader.GetNativeMediaType(FIRST_VIDEO, 0)?;
        let fs = native.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let (nw, nh) = ((fs >> 32) as u32, (fs & 0xffff_ffff) as u32);
        let (tw, th) = match max_dim {
            Some(m) if nw > 0 && nh > 0 => fit_within(nw, nh, m),
            _ => (nw, nh),
        };

        let out_type: IMFMediaType = MFCreateMediaType()?;
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        if tw > 0 && th > 0 {
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, ((tw as u64) << 32) | th as u64)?;
        }
        reader.SetCurrentMediaType(FIRST_VIDEO, None, &out_type)?;

        let mut count = 0usize;
        loop {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(
                FIRST_VIDEO,
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut sample),
            )?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                anyhow::bail!("读取采样时出错：{}", path.display());
            }
            if sample.is_some() {
                count += 1;
            }
            if count > 10_000 {
                break; // 防御：避免异常文件把这里变成死循环
            }
        }
        Ok(count)
    }
}

/// 创建 Source Reader。
///
/// `enable_video_processing` 必须为 `true` 才能请求 RGB32 输出：
/// 目标是 RGB32 而源是 NV12 时必须由视频处理器 MFT 做色彩转换与缩放，
/// 否则 `SetCurrentMediaType` 会以 `MF_E_INVALIDMEDIATYPE (0xC00D36B4)` 失败
/// （实测踩过：只在缩放路径开这个开关，导致原生尺寸抽帧全部失败）。
unsafe fn create_reader(
    path: &Path,
    wide: &[u16],
    enable_video_processing: bool,
) -> Result<IMFSourceReader> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.context("MFCreateAttributes 返回空")?;
    if enable_video_processing {
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
    }
    let reader = MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), &attrs)
        .with_context(|| format!("MF 打不开：{}", path.display()))?;
    Ok(reader)
}

unsafe fn seek(reader: &IMFSourceReader, time_ms: u64) -> Result<()> {
    let mut var: PROPVARIANT = std::mem::zeroed();
    {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = (time_ms as i64) * 10_000; // 100ns 单位
    }
    reader.SetCurrentPosition(&GUID::zeroed(), &var)?;
    Ok(())
}

unsafe fn read_next_sample(reader: &IMFSourceReader) -> Result<IMFSample> {
    for _ in 0..600 {
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        reader.ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), None, Some(&mut sample))?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            anyhow::bail!("读到流末尾也没拿到帧");
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            anyhow::bail!("读取采样时出错");
        }
        if let Some(s) = sample {
            return Ok(s);
        }
    }
    anyhow::bail!("连续 600 次都没有拿到可用采样")
}

/// 把 RGB32 采样转成紧凑的 Rgb24 位图（同时处理 MF 的 BGRA 顺序与可能的上翻 stride）。
///
/// 必须成对调用 Lock/Unlock：漏掉解锁会让缓冲区保持锁定，
/// 后续 MF 调用可能失败甚至卡住。
unsafe fn sample_to_bitmap(sample: &IMFSample, tw: u32, th: u32) -> Result<Bitmap> {
    let buffer = sample.ConvertToContiguousBuffer()?;
    let w = tw as usize;
    let h = th as usize;
    if w == 0 || h == 0 {
        anyhow::bail!("目标尺寸为 0，无法转换采样");
    }
    let mut data = vec![0u8; w * h * 3];

    if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut pitch = 0i32;
        buf2d.Lock2D(&mut p, &mut pitch)?;
        if p.is_null() {
            let _ = buf2d.Unlock2D();
            anyhow::bail!("Lock2D 返回空指针");
        }
        copy_rows(p as isize, pitch as isize, &mut data, w, h);
        buf2d.Unlock2D()?;
    } else {
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        buffer.Lock(&mut p, Some(&mut max_len), Some(&mut cur_len))?;
        if p.is_null() {
            let _ = buffer.Unlock();
            anyhow::bail!("Lock 返回空指针");
        }
        copy_rows(p as isize, (tw * 4) as isize, &mut data, w, h);
        buffer.Unlock()?;
    }

    Ok(Bitmap {
        w: tw,
        h: th,
        layout: PixelLayout::Rgb24,
        data,
    })
}

/// 从 RGB32（内存序 B,G,R,X）拷成紧凑 Rgb24。
/// `pitch` 为负表示自底向上存储。
unsafe fn copy_rows(base: isize, pitch: isize, data: &mut [u8], w: usize, h: usize) {
    for y in 0..h {
        let row = if pitch >= 0 {
            y as isize
        } else {
            (h - 1 - y) as isize
        };
        let row_ptr = (base + row * pitch) as *const u8;
        let dst = &mut data[y * w * 3..(y + 1) * w * 3];
        for x in 0..w {
            let p = row_ptr.add(x * 4);
            dst[x * 3] = *p.add(2); // R
            dst[x * 3 + 1] = *p.add(1); // G
            dst[x * 3 + 2] = *p; // B
        }
    }
}

fn fourcc(guid: &GUID) -> String {
    // 注意：GUID::to_u128() 返回的是「规范序」（Data1 在高位），
    // 4CC 编码在 Data1 里，直接用 guid.data1 即可，
    // 用 to_u128() & 0xffffffff 取到的是 Data4 的尾巴（实测踩过）。
    let b = guid.data1.to_le_bytes();
    if b.iter().all(|c| c.is_ascii_graphic()) {
        String::from_utf8_lossy(&b).to_string()
    } else {
        format!("{guid:?}")
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    let mut w: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str()).collect();
    w.push(0);
    w
}

// ---------------------------------------------------------------------------
// M1 前置实验：常驻读取器 + 输出格式对比
//
// 动机（M0 实测）：
//   - 抽单帧 ≈200ms，而缩到 256px 只快 18% → 这部分主要是**每个 SourceReader
//     的一次性建链开销**，不是像素工作量，所以必须复用读取器。
//   - 整段解码只有 24.7fps@1440p，远低于 Iris Xe 硬解应有的水平，
//     怀疑 RGB32 的色彩转换 + CPU 回读拖了后腿，因此需要与 NV12 直出对比。
// ---------------------------------------------------------------------------

/// 输出格式：RGB32 便于 CPU 直接使用，NV12 是解码器的原生格式（播放时应走这条）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutFormat {
    Rgb32,
    Nv12,
}

impl OutFormat {
    fn guid(self) -> GUID {
        match self {
            OutFormat::Rgb32 => MFVideoFormat_RGB32,
            OutFormat::Nv12 => windows::Win32::Media::MediaFoundation::MFVideoFormat_NV12,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            OutFormat::Rgb32 => "RGB32",
            OutFormat::Nv12 => "NV12",
        }
    }
}

/// 常驻的视频读取器：把一次性建链开销摊薄到多次抽帧上。
pub struct ClipReader {
    reader: IMFSourceReader,
    w: u32,
    h: u32,
    fmt: OutFormat,
    eos: std::cell::Cell<bool>,
}

impl ClipReader {
    pub fn open(path: &Path, fmt: OutFormat, max_dim: Option<u32>) -> Result<Self> {
        let wide = wide_path(path);
        unsafe {
            let reader = create_reader(path, &wide, true)?;
            let native: IMFMediaType = reader.GetNativeMediaType(FIRST_VIDEO, 0)?;
            let fs = native.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            let (nw, nh) = ((fs >> 32) as u32, (fs & 0xffff_ffff) as u32);
            let (tw, th) = match max_dim {
                Some(m) if nw > 0 && nh > 0 => fit_within(nw, nh, m),
                _ => (nw, nh),
            };

            let out_type: IMFMediaType = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &fmt.guid())?;
            if tw > 0 && th > 0 {
                out_type.SetUINT64(&MF_MT_FRAME_SIZE, ((tw as u64) << 32) | th as u64)?;
            }
            reader.SetCurrentMediaType(FIRST_VIDEO, None, &out_type)?;

            Ok(Self {
                reader,
                w: tw,
                h: th,
                fmt,
                eos: std::cell::Cell::new(false),
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    pub fn format(&self) -> OutFormat {
        self.fmt
    }

    /// 本帧在 NV12 下应有的字节数（用于校验缓冲区大小）。
    pub fn frame_bytes(&self) -> usize {
        match self.fmt {
            OutFormat::Rgb32 => (self.w * self.h * 4) as usize,
            OutFormat::Nv12 => (self.w * self.h * 3 / 2) as usize,
        }
    }

    pub fn seek(&self, ms: u64) -> Result<()> {
        self.eos.set(false);
        unsafe { seek(&self.reader, ms) }
    }

    /// 读下一帧。到流末尾返回 `Ok(None)`。
    pub fn read_frame(&self) -> Result<Option<IMFSample>> {
        if self.eos.get() {
            return Ok(None);
        }
        unsafe {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            self.reader
                .ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), None, Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                self.eos.set(true);
                return Ok(None);
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                anyhow::bail!("读取采样时出错");
            }
            Ok(sample)
        }
    }
}

/// 用常驻读取器整段解码，返回帧数（由调用方计时）。
pub fn decode_all_with(reader: &ClipReader) -> Result<usize> {
    let mut n = 0usize;
    while reader.read_frame()?.is_some() {
        n += 1;
        if n > 100_000 {
            break;
        }
    }
    Ok(n)
}
