//! 生成网格性能测试用的真实缩略图。
//!
//! 从真实库里抽 N 张（HEIC/JPG 混合），缩到 512px 存成 JPEG，
//! 输出到 app/public/thumbs/，并写 manifest.json 供前端 fetch。
//!
//! 用法： gen_thumbs [root] [count] [outdir]

use std::path::PathBuf;

use anyhow::Result;
use livephoto_core::benchutil::collect_files;
use livephoto_core::media::wic::PixelLayout;
use livephoto_core::media::{com_init_sta, mf_shutdown, mf_startup, wic};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| r"F:\DCIM".into()));
    let count: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300);
    let outdir = PathBuf::from(args.get(3).cloned().unwrap_or_else(|| {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.push("app/public/thumbs");
        p.display().to_string()
    }));

    com_init_sta()?;
    mf_startup()?;
    let factory = wic::WicFactory::new()?;
    std::fs::create_dir_all(&outdir)?;

    // 一半 HEIC 一半 JPG，贴近真实混合库
    let half = count / 2;
    let mut files = collect_files(&root, &["heic", "heif"], half);
    files.extend(collect_files(&root, &["jpg", "jpeg"], count - half));
    files.sort();
    println!("候选文件: {}（HEIC 优先，混合 JPG）", files.len());

    let mut manifest: Vec<String> = Vec::new();
    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut total_bytes = 0usize;

    for (i, src) in files.iter().enumerate() {
        if manifest.len() >= count {
            break;
        }
        let name = format!("t{:04}.jpg", i);
        let dst = outdir.join(&name);
        match factory
            .decode(src, Some(512), PixelLayout::Rgb24)
            .and_then(|b| b.encode_jpeg(82))
        {
            Ok(jpeg) => {
                total_bytes += jpeg.len();
                std::fs::write(&dst, &jpeg)?;
                manifest.push(format!("/thumbs/{name}"));
                ok += 1;
            }
            Err(e) => {
                fail += 1;
                if fail <= 3 {
                    println!("  跳过 {}: {e}", src.display());
                }
            }
        }
    }

    let manifest_path = outdir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

    println!(
        "完成: {} 张缩略图，失败 {}，平均 {:.0} KB，总计 {:.1} MB",
        ok,
        fail,
        total_bytes as f64 / ok.max(1) as f64 / 1024.0,
        total_bytes as f64 / 1048576.0
    );
    println!("输出: {}", outdir.display());
    Ok(())
}
