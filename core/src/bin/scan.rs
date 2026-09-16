//! M1 · 目录扫描 + Live Photo 配对（只读元数据）
//!
//! 验收标准（对 `F:\DCIM`）：
//!   - 扫描 17,159 个文件的时间 ≤ 2s（不读任何文件内容）
//!   - 配对出 **311** 对 Live Photo，组合为 heic+mov 278 / jpg+mov 20 / jpeg+mov 13
//!     —— 必须与侦察脚本 `tools/livephoto-probe3.py` 的结果一致
//!   - 识别出云端占位文件数量（本机 `Pictures\iCloud Photos` 应为 663/666）
//!
//! 用法： scan <root> [--limit N] [--classify-orphans N]
//! 结果： m0-results/scan-<slug>.json

use std::path::{Path, PathBuf};

use anyhow::Result;
use livephoto_core::benchutil::{round2, write_result};
use livephoto_core::files::{scan_tree, FileEntry};
use livephoto_core::pairing::{pair_all, Kind, PairConfidence};
use livephoto_core::movmeta;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| r"F:\DCIM".into()));

    // 若给的是单个文件，只打印它的 MOV 元数据（用于和 tools/livephoto-probe4.py 对账）
    if root.is_file() {
        let m = movmeta::read(&root)?;
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "file": root.display().to_string(),
            "duration_ms": m.duration_ms,
            "codec": m.codec,
            "width": m.width,
            "height": m.height,
            "is_live_video": m.is_live_video(),
            "is_hevc": m.is_hevc(),
            "has_still_image_time": m.has_still_image_time,
            "has_content_identifier": m.has_content_identifier,
            "has_live_photo_info": m.has_live_photo_info,
            "apple_keys": m.apple_keys,
        }))?);
        return Ok(());
    }

    let limit = arg_value(&args, "--limit").unwrap_or(0);
    // 抽样检查多少个孤儿视频是"其实有 live photo 元数据"的
    let classify = arg_value(&args, "--classify-orphans").unwrap_or(200);

    println!("=== M1 · 扫描 + 配对 ===");
    println!("根目录: {}", root.display());

    let mut entries: Vec<FileEntry> = Vec::new();
    let scan = scan_tree(&root, limit, &mut entries);
    println!(
        "\n扫描: {} 个目录, {} 个文件, 其中云端占位 {} 个, 用时 {:.0}ms",
        scan.dirs_scanned, scan.files, scan.placeholders, scan.ms
    );
    if scan.truncated {
        println!("（达到 --limit 上限，提前停止）");
    }
    for e in scan.errors.iter().take(5) {
        println!("  扫描告警: {e}");
    }
    if !entries.is_empty() && scan.ms > 0.0 {
        println!(
            "  吞吐: {:.0} 文件/秒",
            scan.files as f64 / (scan.ms / 1000.0)
        );
    }

    let t0 = std::time::Instant::now();
    let result = pair_all(&entries);
    let pair_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let s = &result.stats;

    println!("\n配对（用时 {pair_ms:.0}ms）:");
    println!("  剧照 {} / 视频 {}", s.stills, s.videos);
    println!("  Live Photo **{}** 对", s.live);
    println!("  纯剧照 {} / 孤儿视频 {}", s.plain_stills, s.orphan_videos);
    let mut combos: Vec<_> = s.combos.iter().collect();
    combos.sort_by(|a, b| b.1.cmp(a.1));
    println!("  配对组合: {:?}", combos);
    println!("  资产总数: {}", result.assets.len());

    // 抽样：孤儿视频里有多少其实是 Live Photo 视频（剧照丢了）
    let orphan_videos: Vec<&str> = result
        .assets
        .iter()
        .filter(|a| a.kind == Kind::Video && a.confidence == PairConfidence::Orphan)
        .filter_map(|a| a.video_path.as_deref())
        .take(classify)
        .collect();

    let mut live_meta = 0usize;
    let mut codecs: std::collections::HashMap<String, usize> = Default::default();
    let mut durations: Vec<u64> = Vec::new();
    let mut dims: std::collections::HashMap<String, usize> = Default::default();
    let mut sample_keys: Vec<String> = Vec::new();
    for p in &orphan_videos {
        if let Ok(m) = movmeta::read(Path::new(p)) {
            if m.is_live_video() {
                live_meta += 1;
            }
            if !m.codec.is_empty() {
                *codecs.entry(m.codec.clone()).or_insert(0) += 1;
            }
            if m.duration_ms > 0 {
                durations.push(m.duration_ms);
            }
            if m.width > 0 {
                *dims.entry(format!("{}x{}", m.width, m.height)).or_insert(0) += 1;
            }
            if sample_keys.is_empty() && !m.apple_keys.is_empty() {
                sample_keys = m.apple_keys.clone();
            }
        }
    }
    durations.sort();
    let dur_p50 = durations.get(durations.len() / 2).copied().unwrap_or(0);

    println!("\n孤儿视频抽样（{} 个）:", orphan_videos.len());
    println!("  带 live photo 元数据的: {live_meta}");
    println!("  编码分布: {codecs:?}");
    println!("  分辨率分布: {dims:?}");
    println!("  时长 p50: {dur_p50}ms");
    if !sample_keys.is_empty() {
        println!("  元数据键示例: {sample_keys:?}");
    }

    // 校验：与侦察脚本的期望值对比
    let expected_live = 311usize;
    let ok = if scan.truncated || limit > 0 {
        None
    } else {
        Some(s.live == expected_live)
    };

    let slug = root
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "root".into());

    let out = serde_json::json!({
        "kind": "scan-pair",
        "root": root.display().to_string(),
        "scan": {
            "dirs": scan.dirs_scanned,
            "files": scan.files,
            "placeholders": scan.placeholders,
            "ms": round2(scan.ms),
            "files_per_sec": round2(if scan.ms > 0.0 { scan.files as f64 / (scan.ms / 1000.0) } else { 0.0 }),
            "truncated": scan.truncated,
            "errors": scan.errors,
        },
        "pairing": {
            "ms": round2(pair_ms),
            "stills": s.stills,
            "videos": s.videos,
            "live": s.live,
            "plain_stills": s.plain_stills,
            "orphan_videos": s.orphan_videos,
            "assets": result.assets.len(),
            "combos": s.combos,
        },
        "orphan_video_sample": {
            "sampled": orphan_videos.len(),
            "with_live_metadata": live_meta,
            "codecs": codecs,
            "dimensions": dims,
            "duration_p50_ms": dur_p50,
            "apple_keys_sample": sample_keys,
        },
        "verification": {
            "expected_live_pairs": expected_live,
            "matches_recon_script": ok,
        },
    });

    let path = write_result(&format!("scan-{slug}"), &out)?;
    println!("\n{}", serde_json::to_string_pretty(&out)?);
    println!("\n结果已写入: {}", path.display());
    if ok == Some(false) {
        println!("\n!! 与侦察脚本期望值不符：期望 {expected_live} 对，实得 {}", s.live);
        std::process::exit(2);
    }
    Ok(())
}

fn arg_value(args: &[String], flag: &str) -> Option<usize> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}
