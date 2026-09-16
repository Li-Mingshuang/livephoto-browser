//! M0 验证 1 · HEIC 解码与缩略图管线性能
//!
//! 量四件事：
//!   A. WIC 全尺寸解码（RGBA）        —— 「打开原图」的下限
//!   B. WIC 解码 → 缩到 512px → JPEG  —— 缩略图缓存管线（主路径）
//!   C. 全尺寸解码 → fast_image_resize(Lanczos3) → JPEG —— 与 B 对照
//!   D. B 的多线程吞吐                —— 推算整个库首次索引要多久
//!
//! 前提（实测得出）：每个工作线程都必须是 **STA**，且进程要 `MFStartup`，
//! 否则 HEIC 解码会在 MTA 下永久阻塞。
//!
//! 用法： bench_heic [root] [limit] [exts]   默认 F:\DCIM 40 heic,heif
//! 结果： m0-results/heic-decode.json
//!
//! 提示：把 exts 换成 `jpg` 可以做**控制实验** —— 同样的并行框架解 JPEG，
//! 若 JPEG 能接近线性加速而 HEIC 不能，说明瓶颈在 HEIC 解码器而不是测法。

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use livephoto_core::benchutil::{collect_files, round2, write_result, Samples};
use livephoto_core::media::wic::{PixelLayout, WicFactory};
use livephoto_core::media::{com_init_sta, mf_shutdown, mf_startup, wic};
use rayon::prelude::*;

const JPEG_QUALITY: u8 = 82;
const THUMB_DIM: u32 = 512;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| r"F:\DCIM".into()));
    let limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let exts_arg = args.get(3).cloned().unwrap_or_else(|| "heic,heif".into());
    let exts: Vec<&str> = exts_arg.split(',').map(|s| s.trim()).collect();
    // 传 `nomf` 可跳过 MFStartup，用于检验 MF 是否是并行不加速的元凶
    let no_mf = args.iter().any(|a| a == "nomf");

    // STA 是 HEIC 解码的硬前提
    com_init_sta()?;
    if no_mf {
        println!("!! 跳过 MFStartup（对照实验）");
    } else {
        mf_startup()?;
    }
    let factory = WicFactory::new()?;

    println!("=== M0 验证 1 · 解码管线（STA + MFStartup）===");
    println!("扫描根目录: {}", root.display());
    println!("扩展名: {exts:?}");

    let files = collect_files(&root, &exts, limit);
    if files.is_empty() {
        anyhow::bail!("在 {} 下没找到 {exts:?} 文件", root.display());
    }
    println!("样本数: {}\n", files.len());

    // 预热文件缓存：把「首次读盘」的冷启动代价排除在解码耗时之外
    let warm: usize = files
        .iter()
        .map(|f| std::fs::read(f).map(|b| b.len()).unwrap_or(0))
        .sum();
    println!("预热读盘: {:.1} MB\n", warm as f64 / 1048576.0);

    let mut a_full = Samples::new("A_wic_full_rgba");
    let mut b_thumb = Samples::new("B_wic_scale512_rgb_jpeg");
    let mut c_fir = Samples::new("C_fir_lanczos3_512_jpeg");
    let mut open_only = Samples::new("A0_wic_decoder_create_only");
    let mut jpeg_sizes: Vec<usize> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut dimensions: Vec<(u32, u32)> = Vec::new();

    for path in &files {
        // 只创建解码器（不做像素解码）—— 决定索引阶段能不能碰 HEIC
        match timed(|| factory.size(path)) {
            Ok(((w, h), ms)) => {
                open_only.push(ms);
                dimensions.push((w, h));
            }
            Err(e) => errors.push(format!("A0 {}: {e}", short(path))),
        }

        match timed(|| factory.decode(path, None, PixelLayout::Rgba8)) {
            Ok((bmp, ms)) => a_full.push(ms),
            Err(e) => errors.push(format!("A {}: {e}", short(path))),
        }

        match timed(|| {
            let bmp = factory.decode(path, Some(THUMB_DIM), PixelLayout::Rgb24)?;
            let jpeg = bmp.encode_jpeg(JPEG_QUALITY)?;
            Ok::<(u32, u32, usize), anyhow::Error>((bmp.w, bmp.h, jpeg.len()))
        }) {
            Ok(((_, _, bytes), ms)) => {
                b_thumb.push(ms);
                jpeg_sizes.push(bytes);
            }
            Err(e) => errors.push(format!("B {}: {e}", short(path))),
        }

        match timed(|| {
            let full = factory.decode(path, None, PixelLayout::Rgb24)?;
            let small = wic::resize_lanczos3(&full, THUMB_DIM)?;
            let jpeg = small.encode_jpeg(JPEG_QUALITY)?;
            Ok::<usize, anyhow::Error>(jpeg.len())
        }) {
            Ok((_, ms)) => c_fir.push(ms),
            Err(e) => errors.push(format!("C {}: {e}", short(path))),
        }
    }

    // ---- D. 多线程吞吐（每个 rayon 工作线程都是独立 STA）----
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let workers = cores.saturating_sub(2).clamp(2, 8);

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .start_handler(|_| {
            // 每个工作线程必须是独立 STA，否则 HEIC 解码死锁
            let _ = com_init_sta();
        })
        .build()?;

    let rounds = 3usize;
    let total_tasks = files.len() * rounds;
    let t0 = Instant::now();
    let ok_count = pool.install(|| {
        (0..total_tasks)
            .into_par_iter()
            .map(|i| {
                let path = &files[i % files.len()];
                wic::decode(path, Some(THUMB_DIM), PixelLayout::Rgb24)
                    .and_then(|b| b.encode_jpeg(JPEG_QUALITY))
                    .is_ok()
            })
            .filter(|ok| *ok)
            .count()
    });
    let parallel_secs = t0.elapsed().as_secs_f64();

    // ---- 汇总 ----
    let mean_jpeg = if jpeg_sizes.is_empty() {
        0.0
    } else {
        jpeg_sizes.iter().sum::<usize>() as f64 / jpeg_sizes.len() as f64
    };
    let sample_dims = dimensions.first().copied().unwrap_or((0, 0));

    let b_p50 = b_thumb
        .summary()
        .get("p50_ms")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let proj_10k_single = b_p50 * 10_000.0 / 1000.0;
    let proj_10k_parallel = if ok_count > 0 {
        parallel_secs * 10_000.0 / ok_count as f64
    } else {
        0.0
    };

    let result = serde_json::json!({
        "kind": "decode-pipeline-bench",
        "exts": exts,
        "compiled_profile": if cfg!(debug_assertions) { "debug（数字仅供参考，必须用 release 复测）" } else { "release" },
        "root": root.display().to_string(),
        "samples": files.len(),
        "sample_dimensions": { "w": sample_dims.0, "h": sample_dims.1 },
        "cpu_cores": cores,
        "apartment": "STA（HEIC 解码的硬前提）",
        "mf_startup": true,
        "jpeg": {
            "quality": JPEG_QUALITY,
            "thumb_dim": THUMB_DIM,
            "layout": "Rgb24（image 的 JPEG 编码器不接受 Rgba8）",
            "mean_bytes_512px": round2(mean_jpeg),
            "mean_kb": round2(mean_jpeg / 1024.0),
        },
        "scenarios": [
            open_only.summary(),
            a_full.summary(),
            b_thumb.summary(),
            c_fir.summary(),
        ],
        "parallel": {
            "pipeline": "wic decode -> scale512(Rgb24) -> jpeg q82",
            "workers": workers,
            "tasks": total_tasks,
            "ok": ok_count,
            "seconds": round2(parallel_secs),
            "images_per_sec": round2(if parallel_secs > 0.0 { ok_count as f64 / parallel_secs } else { 0.0 }),
            "effective_ms_per_image": round2(if ok_count > 0 { parallel_secs * 1000.0 / ok_count as f64 } else { 0.0 }),
        },
        "projection": {
            "10k_library_single_thread_minutes": round2(proj_10k_single / 60.0),
            "10k_library_parallel_minutes": round2(proj_10k_parallel / 60.0),
            "note": "按缩略图管线 (B) 的 p50 与并行吞吐推算；首屏不等它，后台渐进完成"
        },
        "cache_footprint": {
            "512px_jpeg_per_10k_mb": round2(mean_jpeg * 10_000.0 / 1048576.0),
        },
        "errors": errors,
    });

    let path = write_result(&format!("decode-{}", exts.join("-")), &result)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    println!("\n结果已写入: {}", path.display());

    mf_shutdown();
    Ok(())
}

fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(T, f64)> {
    let t0 = Instant::now();
    let out = f()?;
    Ok((out, t0.elapsed().as_secs_f64() * 1000.0))
}

fn short(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}
