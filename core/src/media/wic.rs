//! 基于 Windows Imaging Component (WIC) 的图片解码。
//!
//! ## 为什么是 WIC
//! - HEIC 由系统 HEIF 扩展提供（`Microsoft.HEIFImageExtension`），零打包体积；
//! - 缩放走 WIC 自带的高质量插值，省一次全尺寸内存拷贝；
//! - 与系统色彩管理、EXIF 方向处理保持一致。
//!
//! ## 必须遵守的两条硬约束（实测得出，不是猜的）
//!
//! 1. **必须在 STA 套间里调用**。Windows 的 HEIF 解码器在 MTA 下调用
//!    `CreateDecoderFromFilename` 会永久阻塞（进程 0 CPU，实测 30s+ 不返回），
//!    在 STA 下立刻正常。原因是该解码器会回调调用方套间，而只有 STA 的阻塞等待
//!    会顺带泵消息。见 `livephoto_core::media::com_init_sta`。
//!
//! 2. **编码器颜色类型要对**。`image` 的 JPEG 编码器不接受 RGBA8
//!    （会报 "does not support the color type Rgba8"），所以走 JPEG 的路径
//!    直接让 WIC 输出 24bppRGB，既不浪费 25% 内存也省掉一次转换。
//!
//! 因此本模块的 API 是**线程亲和**的：每个工作线程各自是一个 STA，
//! 各自持有自己的工厂与解码器（STA 的 COM 对象不能跨线程直接使用）。
//! 工厂通过 `thread_local!` 惰性创建，`WicFactory` 刻意不实现 `Send`/`Sync`。

use std::cell::RefCell;
use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{Interface, IUnknown, PCWSTR};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat24bppRGB, GUID_WICPixelFormat32bppRGBA,
    IWICBitmapScaler, IWICBitmapSource, IWICFormatConverter, IWICImagingFactory, IWICPalette,
    WICBitmapDitherTypeNone, WICBitmapInterpolationMode, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

/// 输出像素布局。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelLayout {
    /// 4 字节 RGBA，给 GPU/Canvas 用。
    Rgba8,
    /// 3 字节 RGB，给 JPEG 编码用（WIC 的 24bppRGB）。
    Rgb24,
}

impl PixelLayout {
    fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelLayout::Rgba8 => 4,
            PixelLayout::Rgb24 => 3,
        }
    }

    fn wic_guid(self) -> windows::core::GUID {
        match self {
            PixelLayout::Rgba8 => GUID_WICPixelFormat32bppRGBA,
            PixelLayout::Rgb24 => GUID_WICPixelFormat24bppRGB,
        }
    }
}

/// 解码结果。`data` 紧凑排列，stride 恒等于 `w * layout.bytes_per_pixel()`。
#[derive(Clone)]
pub struct Bitmap {
    pub w: u32,
    pub h: u32,
    pub layout: PixelLayout,
    pub data: Vec<u8>,
}

impl Bitmap {
    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    pub fn stride(&self) -> u32 {
        self.w * self.layout.bytes_per_pixel()
    }

    /// 编码为 JPEG。缩略图缓存默认用 JPEG：编码速度比 WebP 快一个数量级。
    pub fn encode_jpeg(&self, quality: u8) -> Result<Vec<u8>> {
        use image::codecs::jpeg::JpegEncoder;
        use image::ExtendedColorType;

        let mut out = Vec::with_capacity(self.data.len() / 6);
        match self.layout {
            PixelLayout::Rgb24 => {
                JpegEncoder::new_with_quality(&mut out, quality).encode(
                    &self.data,
                    self.w,
                    self.h,
                    ExtendedColorType::Rgb8,
                )?;
            }
            PixelLayout::Rgba8 => {
                // 兜底路径：剥掉 alpha 再编码
                let mut rgb = Vec::with_capacity((self.w * self.h * 3) as usize);
                for px in self.data.chunks_exact(4) {
                    rgb.extend_from_slice(&px[..3]);
                }
                JpegEncoder::new_with_quality(&mut out, quality).encode(
                    &rgb,
                    self.w,
                    self.h,
                    ExtendedColorType::Rgb8,
                )?;
            }
        }
        Ok(out)
    }
}

/// 等比缩放到不超过 `max_dim`。
pub fn fit_within(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    if w <= max_dim && h <= max_dim {
        return (w, h);
    }
    let scale = max_dim as f64 / w.max(h) as f64;
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

/// WIC 工厂。**线程亲和**：只能在其创建线程上使用，故不实现 `Send`/`Sync`。
pub struct WicFactory {
    factory: IWICImagingFactory,
}

impl WicFactory {
    /// 在当前线程创建工厂。调用前当前线程必须已经是 STA
    /// （见 `livephoto_core::media::com_init_sta`）。
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: IWICImagingFactory = CoCreateInstance(
                &CLSID_WICImagingFactory,
                None::<&IUnknown>,
                CLSCTX_INPROC_SERVER,
            )
            .context("创建 WIC ImagingFactory 失败：系统 WIC 组件不可用")?;
            Ok(Self { factory })
        }
    }

    /// 解码图片。
    ///
    /// `max_dim` 为 `None` 时输出原始尺寸；否则用 WIC 缩放器直接解到目标尺寸
    /// （Fant 高质量插值），避免"先解全图再缩放"的额外内存与时间。
    pub fn decode(
        &self,
        path: &Path,
        max_dim: Option<u32>,
        layout: PixelLayout,
    ) -> Result<Bitmap> {
        self.decode_with(path, max_dim, layout, WICBitmapInterpolationModeFant)
    }

    pub fn decode_with(
        &self,
        path: &Path,
        max_dim: Option<u32>,
        layout: PixelLayout,
        mode: WICBitmapInterpolationMode,
    ) -> Result<Bitmap> {
        let wide = wide_path(path);

        unsafe {
            let decoder = self
                .factory
                .CreateDecoderFromFilename(
                    PCWSTR(wide.as_ptr()),
                    None,
                    GENERIC_READ,
                    WICDecodeMetadataCacheOnDemand,
                )
                .with_context(|| format!("WIC 无法打开或识别：{}", path.display()))?;

            let frame = decoder
                .GetFrame(0)
                .with_context(|| format!("取第 0 帧失败：{}", path.display()))?;
            let frame_src: IWICBitmapSource = frame.cast()?;

            let mut w = 0u32;
            let mut h = 0u32;
            frame_src.GetSize(&mut w, &mut h)?;

            let source: IWICBitmapSource = match max_dim {
                None => frame_src,
                Some(max_dim) => {
                    let (tw, th) = fit_within(w, h, max_dim);
                    if (tw, th) == (w, h) {
                        frame_src
                    } else {
                        let scaler: IWICBitmapScaler = self.factory.CreateBitmapScaler()?;
                        scaler.Initialize(&frame_src, tw, th, mode)?;
                        scaler.cast()?
                    }
                }
            };

            let mut sw = 0u32;
            let mut sh = 0u32;
            source.GetSize(&mut sw, &mut sh)?;

            let converter: IWICFormatConverter = self.factory.CreateFormatConverter()?;
            converter.Initialize(
                &source,
                &layout.wic_guid(),
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )?;

            let stride = sw * layout.bytes_per_pixel();
            let mut data = vec![0u8; stride as usize * sh as usize];
            converter.CopyPixels(std::ptr::null(), stride, &mut data)?;

            Ok(Bitmap {
                w: sw,
                h: sh,
                layout,
                data,
            })
        }
    }

    /// 只读容器头部拿宽高（不做像素解码）。
    ///
    /// 注意：对 HEIC 而言这一步依然要创建 HEIF 解码器，实测 ≈200ms，
    /// 比解析容器里的 `ispe` box 慢几个数量级。索引阶段**不要**用它，
    /// 应当直接解析 ISO-BMFF 容器（见 M1 计划）。
    pub fn size(&self, path: &Path) -> Result<(u32, u32)> {
        let wide = wide_path(path);
        unsafe {
            let decoder = self.factory.CreateDecoderFromFilename(
                PCWSTR(wide.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )?;
            let frame = decoder.GetFrame(0)?;
            let src: IWICBitmapSource = frame.cast()?;
            let mut w = 0u32;
            let mut h = 0u32;
            src.GetSize(&mut w, &mut h)?;
            Ok((w, h))
        }
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    let mut wide: Vec<u16> =
        std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str()).collect();
    wide.push(0);
    wide
}

thread_local! {
    /// 每个线程（= 每个 STA）自己的工厂。
    static TL_FACTORY: RefCell<Option<WicFactory>> = const { RefCell::new(None) };
}

/// 在当前线程的工厂上执行操作（惰性创建）。
///
/// 调用线程必须已经 `com_init_sta()`，否则内部的 HEIC 解码会死锁。
pub fn with_factory<T>(f: impl FnOnce(&WicFactory) -> Result<T>) -> Result<T> {
    TL_FACTORY.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(WicFactory::new()?);
        }
        f(slot.as_ref().expect("刚创建"))
    })
}

/// 丢弃当前线程的 WIC 工厂（以及它持有的所有 COM 对象）。
///
/// 配合 `media::com_reset_sta()` 一起用，实现"工作线程定期回收"：
/// 缩略图池实测存在渐进退化（平均 224ms → 385ms、最慢一帧 325ms → 3398ms），
/// 怀疑是系统 HEIF 解码器在套间上累积了状态/未及时释放的大块内存。
pub fn reset_thread_factory() {
    TL_FACTORY.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// 便捷解码：使用当前线程的工厂。
pub fn decode(path: &Path, max_dim: Option<u32>, layout: PixelLayout) -> Result<Bitmap> {
    with_factory(|f| f.decode(path, max_dim, layout))
}

/// 用 fast_image_resize（SIMD / Lanczos3）缩放已解码的位图。
///
/// 与 `WicFactory::decode(.., Some(max_dim), ..)` 形成对照：
/// 后者多一次全尺寸解码和一份全尺寸内存，但省掉了 RGBA→RGB 的额外步骤。
/// 哪个更快由 bench 决定，不靠猜。
pub fn resize_lanczos3(src: &Bitmap, max_dim: u32) -> Result<Bitmap> {
    use fast_image_resize as fr;

    let (tw, th) = fit_within(src.w, src.h, max_dim);
    if (tw, th) == (src.w, src.h) {
        return Ok(src.clone());
    }

    let pixel_type = match src.layout {
        PixelLayout::Rgba8 => fr::PixelType::U8x4,
        PixelLayout::Rgb24 => fr::PixelType::U8x3,
    };
    let src_img = fr::images::Image::from_vec_u8(src.w, src.h, src.data.clone(), pixel_type)?;
    let mut dst_img = fr::images::Image::new(tw, th, pixel_type);
    let options = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3));
    fr::Resizer::new().resize(&src_img, &mut dst_img, &options)?;

    Ok(Bitmap {
        w: tw,
        h: th,
        layout: src.layout,
        data: dst_img.into_vec(),
    })
}
