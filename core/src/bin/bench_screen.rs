//! 全屏层级（2048px）的耗时拆解：到底是解码贵还是编码贵？
//!
//! 背景：用户实测"全屏切换时先显示小图、过一会才出大图"，自检测出冷启动 **2332ms**、
//! 超出预加载半径 **5349ms** —— 比预估的 1 秒严重得多。
//! 要决定怎么优化（降目标尺寸？换编码器？提高并发？），必须先知道时间花在哪一段。
//!
//! 用法： bench_screen [root] [limit] [exts]
//! 结果： m0-results/screen-cost.json

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use livephoto_core::benchutil::{collect_files, round2, write_result, Samples};
use livephoto_core::media::wic::{PixelLayout, WicFactory};
use livephoto_core::media::{com_init_sta, mf_shutdown, mf_startup, wic};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| r"F:\DCIM".into()));
    let limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(24);
    let exts_arg = args.get(3).cloned().unwrap_or_else(|| "heic".into());
    let exts: Vec<&str> = exts_arg.split(',').map(|s| s.trim()).collect();

    com_init_sta()?;
    mf_startup()?;
    let factory = WicFactory::new()?;

    let files = collect_files(&root, &exts, limit);
    if files.is_empty() {
        anyhow::bail!("没找到 {exts:?} 文件");
    }
    println!("=== 全屏层级耗时拆解 ===");
    println!("样本 {} 个 {exts:?}\n", files.len());

    let mut dec512 = Samples::new("decode@512");
    let mut dec2048 = Samples::new("decode@2048");
    let mut enc512 = Samples::new("jpeg_encode@512");
    let mut enc1536 = Samples::new("jpeg_encode@1536");
    let mut enc2048 = Samples::new("jpeg_encode@2048");
    let mut full_512 = Samples::new("decode+encode@512 (网格层现状)");
    let mut full_2048 = Samples::new("decode+encode@2048 (全屏层现状)");
    let mut full_1536 = Samples::new("decode+encode@1536 (候选)");
    let mut sizes: Vec<(usize, usize, usize)> = Vec::new();

    for p in &files {
        // 解码
        let b512 = match factory.decode(p, Some(512), PixelLayout::Rgb24) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let t = Instant::now();
        let b2048 = factory.decode(p, Some(2048), PixelLayout::Rgb24)?;
        dec2048.push(t.elapsed().as_secs_f64() * 1000.0);

        // 只测解码一次的话 512 会被缓存影响，所以单独再解一次
        let t = Instant::now();
        let _ = factory.decode(p, Some(512), PixelLayout::Rgb24)?;
        dec512.push(t.elapsed().as_secs_f64() * 1000.0);

        // 编码：同一份位图分别按三个目标尺寸编码
        let t = Instant::now();
        let j512 = b512.encode_jpeg(82)?;
        enc512.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        let small1536 = wic::resize_lanczos3(&b2048, 1536)?;
        let j1536 = small1536.encode_jpeg(88)?;
        enc1536.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        let j2048 = b2048.encode_jpeg(88)?;
        enc2048.push(t.elapsed().as_secs_f64() * 1000.0);

        let _ = &j512;
        sizes.push((j512.len(), j1536.len(), j2048.len()));

        // 端到端（用刚才的样本，减去已测部分不精确，这里独立再跑一遍保持可比）
        let t = Instant::now();
        let b = factory.decode(p, Some(512), PixelLayout::Rgb24)?;
        let j = b.encode_jpeg(82)?;
        let _ = j;
        full_512.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        let b = factory.decode(p, Some(2048), PixelLayout::Rgb24)?;
        let j = b.encode_jpeg(88)?;
        let _ = j;
        full_2048.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        let b = factory.decode(p, Some(1536), PixelLayout::Rgb24)?;
        let j = b.encode_jpeg(88)?;
        let _ = j;
        full_1536.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    let mean = |v: &[usize]| -> f64 {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<usize>() as f64 / v.len() as f64 / 1024.0
        }
    };
    let kb512: Vec<usize> = sizes.iter().map(|s| s.0).collect();
    let kb1536: Vec<usize> = sizes.iter().map(|s| s.1).collect();
    let kb2048: Vec<usize> = sizes.iter().map(|s| s.2).collect();

    let dec2048_p50 = dec2048.summary().get("p50_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let enc2048_p50 = enc2048.summary().get("p50_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let full2048_p50 = full_2048.summary().get("p50_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let out = serde_json::json!({
        "kind": "screen-cost-bench",
        "exts": exts,
        "files": files.len(),
        "decode": [dec512.summary(), dec2048.summary()],
        "encode": [enc512.summary(), enc1536.summary(), enc2048.summary()],
        "end_to_end": [full_512.summary(), full_1536.summary(), full_2048.summary()],
        "jpeg_kb": {
            "at512_q82": round2(mean(&kb512)),
            "at1536_q88": round2(mean(&kb1536)),
            "at2048_q88": round2(mean(&kb2048)),
        },
        "split_of_2048": {
            "decode_p50_ms": round2(dec2048_p50),
            "encode_p50_ms": round2(enc2048_p50),
            "end_to_end_p50_ms": round2(full2048_p50),
            "decode_share": round2(if full2048_p50 > 0.0 { dec2048_p50 / full2048_p50 } else { 0.0 }),
            "encode_share": round2(if full2048_p50 > 0.0 { enc2048_p50 / full2048_p50 } else { 0.0 }),
        },
        "note": "decode 与 encode 分别独立计时；两者之和不等于端到端（后者含 WIC 缩放+格式转换）",
    });

    println!("{}", serde_json::to_string_pretty(&out)?);
    let path = write_result(&format!("screen-cost-{}", exts.join("_")), &out)?;
    println!("\n结果已写入: {}", path.display());
    mf_shutdown();
    Ok(())
}

#[allow(dead_code)]
fn short(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}
