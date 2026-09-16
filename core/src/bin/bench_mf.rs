//! M0 验证 2 · Media Foundation 抽帧（Live Photo 的 HEVC 视频）
//!
//! Live Photo 的核心内容是 1920×1440 的 HEVC 片段。如果 MF 能硬解，
//! 单帧抽取应当在毫秒级，那么"海报帧优先从 MOV 取"就是划算的策略
//! ——对比 HEIC 解码的 ≈460ms。
//!
//! 用法： bench_mf [root] [limit]      默认 F:\DCIM 16
//! 结果： m0-results/mf-frame.json

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use livephoto_core::benchutil::{collect_files, round2, write_result, Samples};
use livephoto_core::media::{com_init_sta, mf, mf_shutdown, mf_startup};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| r"F:\DCIM".into()));
    let limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(16);

    com_init_sta()?;
    mf_startup()?;

    println!("=== M0 验证 2 · Media Foundation 抽帧 ===");
    let files = collect_files(&root, &["mov", "mp4"], limit);
    if files.is_empty() {
        anyhow::bail!("在 {} 下没找到 MOV/MP4", root.display());
    }
    println!("样本数: {}\n", files.len());

    let mut probe_s = Samples::new("A_mf_probe(info only)");
    let mut first_s = Samples::new("B_first_frame_native");
    let mut seek1s_s = Samples::new("C_seek_1000ms_native");
    let mut scaled_s = Samples::new("D_seek_1000ms_scaled256");
    let mut all_s = Samples::new("E_decode_entire_clip_native");
    let mut infos = Vec::new();
    let mut frame_counts = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // 把第一帧的缩略图存下来，便于人工/程序核对抽帧是否正确（不是黑帧/倒置/偏色）
    let mut saved_sample = false;
    // 整段解码很贵（每段几十帧），只对前几个文件做，够推算吞吐即可
    let decode_all_limit = 2usize;

    for (idx, path) in files.iter().enumerate() {
        eprintln!("[{}/{}] {}", idx + 1, files.len(), short(path));

        let t = Instant::now();
        match mf::probe(path) {
            Ok(info) => {
                probe_s.push(t.elapsed().as_secs_f64() * 1000.0);
                infos.push(serde_json::json!({
                    "file": short(path),
                    "w": info.w, "h": info.h, "codec": info.codec,
                }));
            }
            Err(e) => {
                errors.push(format!("probe {}: {e:#}", short(path)));
                continue;
            }
        }

        match timed(|| mf::extract_frame(path, None, None)) {
            Ok((bmp, ms)) => {
                first_s.push(ms);
                if !saved_sample {
                    let jpeg = bmp.encode_jpeg(90)?;
                    let dir = livephoto_core::benchutil::results_dir();
                    std::fs::create_dir_all(&dir)?;
                    let out = dir.join("mf-first-frame.jpg");
                    std::fs::write(&out, &jpeg)?;
                    // 记下这帧来自哪个文件，便于用 Python 做像素级核对
                    std::fs::write(
                        dir.join("mf-first-frame.src.txt"),
                        format!("{}\n{}x{}\n", path.display(), bmp.w, bmp.h),
                    )?;
                    println!("（首帧样本已存: {} 来自 {}）", out.display(), path.display());
                    saved_sample = true;
                }
            }
            Err(e) => errors.push(format!("first-frame {}: {e:#}", short(path))),
        }

        match timed(|| mf::extract_frame(path, Some(1000), None)) {
            Ok((_, ms)) => seek1s_s.push(ms),
            Err(e) => errors.push(format!("seek1s {}: {e:#}", short(path))),
        }

        match timed(|| mf::extract_frame(path, Some(1000), Some(256))) {
            Ok((bmp, ms)) => {
                scaled_s.push(ms);
                if bmp.w == 0 || bmp.h == 0 {
                    errors.push(format!("scaled {}: 输出尺寸为 0", short(path)));
                }
            }
            Err(e) => errors.push(format!("scaled {}: {e:#}", short(path))),
        }

        if idx < decode_all_limit {
            match timed(|| mf::decode_all_frames(path, None)) {
                Ok((n, ms)) => {
                    all_s.push(ms);
                    frame_counts.push(n);
                }
                Err(e) => errors.push(format!("all-frames {}: {e:#}", short(path))),
            }
        }
    }

    let total_frames: usize = frame_counts.iter().sum();
    let total_all_ms: f64 = all_s.ms.iter().sum();
    let fps = if total_all_ms > 0.0 {
        total_frames as f64 / (total_all_ms / 1000.0)
    } else {
        0.0
    };
    let avg_frames = if frame_counts.is_empty() {
        0.0
    } else {
        total_frames as f64 / frame_counts.len() as f64
    };
    let total_secs: f64 = all_s.ms.iter().sum::<f64>() / 1000.0;

    let result = serde_json::json!({
        "kind": "mf-frame-bench",
        "compiled_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "root": root.display().to_string(),
        "samples": files.len(),
        "videos": infos,
        "scenarios": [
            probe_s.summary(),
            first_s.summary(),
            seek1s_s.summary(),
            scaled_s.summary(),
            all_s.summary(),
        ],
        "decode_throughput": {
            "total_frames": total_frames,
            "avg_frames_per_clip": round2(avg_frames),
            "total_seconds": round2(total_secs),
            "fps": round2(fps),
            "note": "RGB32 输出（含色彩转换与 CPU 回读），不是纯解码上限"
        },
        "errors": errors,
    });

    let path = write_result("mf-frame", &result)?;
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
