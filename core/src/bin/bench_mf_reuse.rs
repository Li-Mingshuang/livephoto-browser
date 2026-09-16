//! M1 前置实验 · 常驻读取器 + 输出格式对比
//!
//! 要回答的问题：
//!   1. 复用 SourceReader 后，抽帧的边际成本降到多少？（M0 测得单次 ≈200ms，
//!      其中大部分是每个 reader 的一次性建链开销）
//!   2. NV12 直出能否显著高于 RGB32（当前的 24.7fps@1440p）？若能，
//!      说明之前的瓶颈是色彩转换+CPU 回读，而不是解码本身。
//!   3. 连续抽帧 vs 反复 seek，哪个更贵？
//!
//! 用法： bench_mf_reuse [root] [limit]     默认 F:\DCIM 8
//! 结果： m0-results/mf-reuse.json

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use livephoto_core::benchutil::{collect_files, round2, write_result, Samples};
use livephoto_core::media::mf::{self, ClipReader, OutFormat};
use livephoto_core::media::{com_init_sta, mf_shutdown, mf_startup};
use rayon::prelude::*;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| r"F:\DCIM".into()));
    let limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);

    com_init_sta()?;
    mf_startup()?;

    println!("=== M1 前置 · 常驻 SourceReader 与输出格式 ===");
    let files = collect_files(&root, &["mov", "mp4"], limit);
    if files.is_empty() {
        anyhow::bail!("没找到 MOV/MP4");
    }
    println!("样本数: {}\n", files.len());

    // ---- 1. 整段解码：三种配置的吞吐对比 ----
    let mut open_ms: Vec<f64> = Vec::new();
    let mut configs: Vec<(&str, f64, usize, f64)> = Vec::new(); // (名字, 总秒, 总帧, 每秒)
    let mut errors = Vec::new();

    for (label, fmt, max_dim) in [
        ("RGB32_native", OutFormat::Rgb32, None),
        ("NV12_native", OutFormat::Nv12, None),
        ("NV12_scaled512", OutFormat::Nv12, Some(512)),
        ("RGB32_scaled512", OutFormat::Rgb32, Some(512)),
    ] {
        let mut frames = 0usize;
        let t0 = Instant::now();
        for path in &files {
            let t_open = Instant::now();
            let reader = match ClipReader::open(path, fmt, max_dim) {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("{label} open {}: {e:#}", short(path)));
                    continue;
                }
            };
            if label == "RGB32_native" {
                open_ms.push(t_open.elapsed().as_secs_f64() * 1000.0);
            }
            let _sz = reader.size();
            let _fb = reader.frame_bytes();
            match mf::decode_all_with(&reader) {
                Ok(n) => frames += n,
                Err(e) => errors.push(format!("{label} decode {}: {e:#}", short(path))),
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let fps = if secs > 0.0 { frames as f64 / secs } else { 0.0 };
        println!("{label:18} 帧数={frames:5} 用时={secs:7.2}s → {fps:7.2} fps");
        configs.push((label, secs, frames, fps));
    }

    // ---- 2. 抽帧延迟：复用同一个 reader，对比 RGB32 / NV12 ----
    let mut first_seek = Samples::new("A_reused_seek_first(该 reader 第一次 seek)");
    let mut later_seek = Samples::new("B_reused_seek_later(同一 reader 后续 seek)");
    let mut seq_frame = Samples::new("C_reused_sequential_next_frame(连续取下一帧)");
    let mut nv12_first = Samples::new("D_nv12_first_frame(不 seek，直接取首帧)");
    let mut nv12_seek = Samples::new("E_nv12_seek_1000ms");
    let times = [200u64, 600, 1000, 1400, 1800];

    for path in &files {
        let reader = match ClipReader::open(path, OutFormat::Rgb32, Some(512)) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for (i, ms) in times.iter().enumerate() {
            let t = Instant::now();
            match reader.seek(*ms).and_then(|_| reader.read_frame()) {
                Ok(Some(_)) => {
                    let cost = t.elapsed().as_secs_f64() * 1000.0;
                    if i == 0 {
                        first_seek.push(cost);
                    } else {
                        later_seek.push(cost);
                    }
                }
                _ => {}
            }
        }
        // 连续取 10 帧的平均成本
        if reader.seek(0).is_ok() {
            for _ in 0..10 {
                let t = Instant::now();
                match reader.read_frame() {
                    Ok(Some(_)) => seq_frame.push(t.elapsed().as_secs_f64() * 1000.0),
                    _ => break,
                }
            }
        }
    }

    // NV12 原生尺寸下的抽帧延迟 —— 这才是产品里真正会用的配置
    for path in &files {
        let reader = match ClipReader::open(path, OutFormat::Nv12, Some(512)) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let t = Instant::now();
        if matches!(reader.read_frame(), Ok(Some(_))) {
            nv12_first.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let t = Instant::now();
        if reader.seek(1000).is_ok() && matches!(reader.read_frame(), Ok(Some(_))) {
            nv12_seek.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }

    // ---- 3. worker 数量扫描 ----
    // 6 线程反而比单线程慢（见下），所以并发度必须实测，不能拍脑袋。
    let rounds = 4usize;
    let tasks = files.len() * rounds;
    let mut sweep: Vec<(usize, f64, usize)> = Vec::new(); // (workers, posters_per_sec, ok)
    for w in [1usize, 2, 3, 4, 6] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(w)
            .start_handler(|_| {
                let _ = com_init_sta();
            })
            .build()?;
        let t0 = Instant::now();
        let ok = pool.install(|| {
            (0..tasks)
                .into_par_iter()
                .map(|i| {
                    let path = &files[i % files.len()];
                    ClipReader::open(path, OutFormat::Nv12, Some(512))
                        .and_then(|r| r.read_frame())
                        .map(|s| s.is_some())
                        .unwrap_or(false)
                })
                .filter(|x| *x)
                .count()
        });
        let secs = t0.elapsed().as_secs_f64();
        let pps = if secs > 0.0 { ok as f64 / secs } else { 0.0 };
        println!("workers={w:2}  {ok:4}/{tasks}  {secs:6.2}s → {pps:7.2} 张/秒");
        sweep.push((w, pps, ok));
    }

    let (best_workers, best_pps, _) = sweep
        .iter()
        .cloned()
        .fold((0usize, 0.0f64, 0usize), |a, b| if b.1 > a.1 { b } else { a });

    let mut open_s = Samples::new("A0_ClipReader_open(建链一次)");
    open_s.ms = open_ms;

    let native_rgb = configs.iter().find(|c| c.0 == "RGB32_native");
    let native_nv12 = configs.iter().find(|c| c.0 == "NV12_native");
    let speedup = match (native_rgb, native_nv12) {
        (Some(a), Some(b)) if a.3 > 0.0 => b.3 / a.3,
        _ => 0.0,
    };

    let result = serde_json::json!({
        "kind": "mf-reader-reuse-bench",
        "compiled_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "root": root.display().to_string(),
        "samples": files.len(),
        "throughput_by_config": configs.iter().map(|(n, s, f, fps)| serde_json::json!({
            "config": n, "seconds": round2(*s), "frames": f, "fps": round2(*fps),
        })).collect::<Vec<_>>(),
        "nv12_vs_rgb32_speedup": round2(speedup),
        "worker_sweep": sweep.iter().map(|(w, pps, ok)| serde_json::json!({
            "workers": w, "posters_per_sec": round2(*pps), "ok": ok,
        })).collect::<Vec<_>>(),
        "best_workers": best_workers,
        "best_posters_per_sec": round2(best_pps),
        "projection_10k_live_photos_minutes": round2(if best_pps > 0.0 { 10_000.0 / best_pps / 60.0 } else { 0.0 }),
        "scroll_demand_note": "快速滚动约 114 格/秒；本机实测的抽帧上限远低于此，故快速滚动必然出现空占位，只能靠热缓存解决",
        "latency": [
            open_s.summary(),
            first_seek.summary(),
            later_seek.summary(),
            seq_frame.summary(),
            nv12_first.summary(),
            nv12_seek.summary(),
        ],
        "interpretation_hints": {
            "later_seek_should_be_much_less_than_m0_200ms": "若后续 seek 仍接近 200ms，说明成本在每次 seek/解码而非建链",
            "nv12_vs_rgb32": "若 NV12 明显更快，瓶颈是色彩转换+CPU 回读；若相近，瓶颈在解码本身",
            "seq_frame": "连续取帧的边际成本 ≈ 真实播放所需的每帧预算（1/30s = 33ms 才能实时）",
        },
        "errors": errors,
    });

    let path = write_result("mf-reuse", &result)?;
    println!("\n{}", serde_json::to_string_pretty(&result)?);
    println!("\n结果已写入: {}", path.display());
    mf_shutdown();
    Ok(())
}

fn short(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}
