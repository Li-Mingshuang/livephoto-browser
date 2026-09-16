//! M1-4 验收 · 缩略图工作池与磁盘缓存
//!
//! 不用开界面就能回答三个问题：
//!   1. 工作池能不能把整个目录的缩略图算出来（成功/失败数、耗时分布）
//!   2. "海报优先来自 MOV" 这条快路径实际命中率多少、平均快多少
//!   3. 之前"同一个操作 65ms vs 1040ms"的资源抖动在常驻工作池下是否消失
//!      —— 判据：最慢一帧 / 平均 的比值是否收敛，以及第二遍（全命中缓存）是否接近零成本
//!
//! 用法： bench_thumbs [root] [limit] [workers]
//! 结果： m0-results/thumbs-<slug>.json

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use livephoto_core::benchutil::{round2, write_result};
use livephoto_core::cache::ThumbCache;
use livephoto_core::files::scan_tree;
use livephoto_core::pairing::pair_all;
use livephoto_core::thumbs::{ThumbPool, ThumbRequest, TIER_GRID};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| r"F:\DCIM".into()));
    let limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300);
    let workers: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);
    // 可选：只取某些扩展名的资产。
    // 用来做判决性验证 —— 若"吞吐下降"只是内容构成变化，
    // 那么单跑 HEIC / 单跑 JPG 时曲线应当各自平坦。
    let ext_filter: Option<Vec<String>> = args.get(4).map(|s| {
        s.split(',')
            .map(|x| x.trim().to_ascii_lowercase())
            .filter(|x| !x.is_empty())
            .collect()
    });

    println!("=== M1-4 · 缩略图工作池验收 ===");
    println!("目录: {}  数量上限: {}  工作线程: {}", root.display(), limit, workers);

    let mut entries = Vec::new();
    let scan = scan_tree(&root, 0, &mut entries);
    let pair = pair_all(&entries);
    println!(
        "扫描 {} 个文件（{:.0}ms），配对 {} 项（Live {}）\n",
        scan.files,
        scan.ms,
        pair.assets.len(),
        pair.stats.live
    );

    // 用**独立缓存目录**跑验收，避免污染真实缓存、也让结果可复现
    let cache_root = livephoto_core::benchutil::results_dir().join("thumb-cache");
    if cache_root.exists() {
        std::fs::remove_dir_all(&cache_root).ok();
    }
    let cache = ThumbCache::with_root(cache_root.clone())?;

    let done = Arc::new(AtomicUsize::new(0));
    let done_for_cb = done.clone();
    let pool = ThumbPool::new(
        workers,
        cache.clone(),
        Box::new(move |_key: &str, _ok: bool| {
            done_for_cb.fetch_add(1, Ordering::Relaxed);
        }),
    );

    // 只取前 limit 个资产，全部当作"可见"
    let ext_of_asset = |a: &livephoto_core::pairing::Asset| -> String {
        a.still_path
            .as_deref()
            .or(a.video_path.as_deref())
            .and_then(|p| p.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()))
            .unwrap_or_else(|| "?".into())
    };

    let selected: Vec<&livephoto_core::pairing::Asset> = pair
        .assets
        .iter()
        .filter(|a| match &ext_filter {
            None => true,
            Some(list) => list.contains(&ext_of_asset(a)),
        })
        .take(limit)
        .collect();

    let take = selected.len();
    let requested_with_video = selected.iter().filter(|a| a.video_path.is_some()).count();
    println!("取 {take} 项：其中带视频（Live Photo）的 {requested_with_video} 项\n");
    let mut reqs = Vec::with_capacity(take);
    let mut wanted = HashSet::new();
    for (i, a) in selected.iter().enumerate() {
        let key = ThumbCache::key(
            TIER_GRID,
            a.still_path.as_deref(),
            a.still_mtime,
            a.still_size,
            a.video_path.as_deref(),
            a.video_mtime,
            a.video_size,
        );
        wanted.insert(key.clone());
        reqs.push(livephoto_core::thumbs::grid_request(
            key,
            i,
            a.still_path.as_ref().map(PathBuf::from),
            a.video_path.as_ref().map(PathBuf::from),
            0,
        ));
    }

    // ---- 第一遍：全部要解码 ----
    println!("第一遍：{take} 张缩略图（缓存为空）");
    pool.set_wanted(wanted.clone());
    pool.submit(reqs.clone());

    let t0 = std::time::Instant::now();
    let mut last_report = std::time::Instant::now();
    // 时间序列采样：用来把"渐进退化"量化成"每 100 张的吞吐曲线"，
    // 只看累计平均是看不出退化斜率的。
    let mut samples: Vec<(f64, usize)> = Vec::new();
    loop {
        if done.load(Ordering::Relaxed) >= take {
            break;
        }
        if t0.elapsed().as_secs() > 900 {
            println!("!! 等待超时");
            break;
        }
        {
            let s = pool.stats();
            samples.push((
                t0.elapsed().as_secs_f64(),
                (s.done_ok + s.done_err) as usize,
            ));
        }
        if last_report.elapsed().as_secs() >= 5 {
            last_report = std::time::Instant::now();
            let s = pool.stats();
            println!(
                "   已完成 {}/{}  队列 {}  平均 {:.0}ms  最慢 {:.0}ms",
                s.done_ok + s.done_err,
                take,
                s.queued,
                s.decode_ms_total / (s.done_ok.max(1) as f64),
                s.decode_ms_max
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let pass1_secs = t0.elapsed().as_secs_f64();
    let s1 = pool.stats();
    println!(
        "  完成 {:.1}s  成功 {} / 失败 {}  平均 {:.0}ms  最慢 {:.0}ms  ({:.2} 张/秒)",
        pass1_secs,
        s1.done_ok,
        s1.done_err,
        s1.decode_ms_total / (s1.done_ok.max(1) as f64),
        s1.decode_ms_max,
        (s1.done_ok + s1.done_err) as f64 / pass1_secs
    );
    println!(
        "  海报来源：MOV {} / 剧照 {}",
        s1.poster_from_video, s1.poster_from_still
    );

    // ---- 第二遍：全部命中缓存 ----
    println!("\n第二遍：同样的 {take} 张（应当全部命中磁盘缓存）");
    let done2 = done.load(Ordering::Relaxed);
    pool.set_wanted(wanted.clone());
    pool.submit(reqs.clone());
    let t1 = std::time::Instant::now();
    loop {
        if done.load(Ordering::Relaxed) >= done2 + take {
            break;
        }
        if t1.elapsed().as_secs() > 120 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pass2_secs = t1.elapsed().as_secs_f64();
    let s2 = pool.stats();
    println!(
        "  完成 {:.2}s  缓存命中 {}  新增解码 {}  ({:.0} 张/秒)",
        pass2_secs,
        s2.cache_hits,
        s2.done_ok.saturating_sub(s1.done_ok),
        take as f64 / pass2_secs.max(0.001)
    );

    let cs = cache.stats();
    let avg1 = s1.decode_ms_total / (s1.done_ok.max(1) as f64);
    let ratio = if avg1 > 0.0 { s1.decode_ms_max / avg1 } else { 0.0 };

    // 把采样点折成"每 100 张耗时"的桶，并记录每个桶里的**内容构成**。
    //
    // 这一步很关键：吞吐下降可能根本不是"资源退化"，而只是
    // "资产按时间倒序，前段是快的 JPG、后段是慢的 HEIC"。
    // 不把构成和耗时放在一起看，就会把一个内容问题误判成性能问题。
    let ext_of = |a: &&livephoto_core::pairing::Asset| -> String { ext_of_asset(a) };
    let mut buckets: Vec<serde_json::Value> = Vec::new();
    if let Some(&(t_start, _)) = samples.first() {
        let mut bucket_start_t = t_start;
        let mut bucket_start_n = 0usize;
        let n_end = samples.last().map(|(_, n)| *n).unwrap_or(0);
        for target in (100..=n_end).step_by(100) {
            if let Some(&(t, _)) = samples.iter().find(|(_, n)| *n >= target) {
                let dn = target - bucket_start_n;
                let dt = t - bucket_start_t;
                if dt > 0.0 && dn > 0 {
                    // 统计该桶内资产的扩展名构成
                    let mut mix: std::collections::HashMap<String, usize> = Default::default();
                    for a in selected.iter().take(target).skip(bucket_start_n) {
                        *mix.entry(ext_of(a)).or_insert(0) += 1;
                    }
                    let mut mix_vec: Vec<(String, usize)> = mix.into_iter().collect();
                    mix_vec.sort_by(|a, b| b.1.cmp(&a.1));
                    let mix_str = mix_vec
                        .iter()
                        .take(3)
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect::<Vec<_>>()
                        .join(" ");

                    let per_sec = dn as f64 / dt;
                    println!(
                        "   桶 {bucket_start_n:>4}-{target:<4} {per_sec:6.2}/s  {mix_str}"
                    );
                    buckets.push(serde_json::json!({
                        "items": format!("{}-{}", bucket_start_n, target),
                        "seconds": round2(dt),
                        "per_sec": round2(per_sec),
                        "ext_mix": mix_str,
                    }));
                }
                bucket_start_t = t;
                bucket_start_n = target;
            }
        }
    }

    // 整个请求集的构成
    let mut total_mix: std::collections::HashMap<String, usize> = Default::default();
    for a in selected.iter() {
        *total_mix.entry(ext_of(a)).or_insert(0) += 1;
    }

    let first_bucket = buckets.first().and_then(|b| b["per_sec"].as_f64()).unwrap_or(0.0);
    let last_bucket = buckets.last().and_then(|b| b["per_sec"].as_f64()).unwrap_or(0.0);

    let out = serde_json::json!({
        "kind": "thumbs-bench",
        "compiled_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "root": root.display().to_string(),
        "workers": workers,
        "requested": take,
        "requested_with_video": requested_with_video,
        "scan": { "files": scan.files, "ms": round2(scan.ms) },
        "pairing": { "assets": pair.assets.len(), "live": pair.stats.live },
        "pass1": {
            "seconds": round2(pass1_secs),
            "ok": s1.done_ok, "err": s1.done_err,
            "avg_decode_ms": round2(avg1),
            "max_decode_ms": round2(s1.decode_ms_max),
            "throughput_per_sec": round2((s1.done_ok + s1.done_err) as f64 / pass1_secs.max(0.001)),
            "poster_from_video": s1.poster_from_video,
            "poster_from_still": s1.poster_from_still,
            "max_over_avg": round2(ratio),
        },
        "pass2_all_cached": {
            "seconds": round2(pass2_secs),
            "cache_hits": s2.cache_hits,
            "throughput_per_sec": round2(take as f64 / pass2_secs.max(0.001)),
        },
        "cache": { "root": cache_root.display().to_string(), "files": cs.files, "bytes": cs.bytes,
                   "avg_kb": round2(cs.bytes as f64 / cs.files.max(1) as f64 / 1024.0) },
        "instability_check": {
            "max_over_avg": round2(ratio),
            "first_100_per_sec": round2(first_bucket),
            "last_100_per_sec": round2(last_bucket),
            "degradation_ratio": round2(if last_bucket > 0.0 { first_bucket / last_bucket } else { 0.0 }),
            "note": "degradation_ratio > 1.2 就说明存在渐进退化（越跑越慢）",
        },
        "throughput_buckets": buckets,
        "requested_ext_mix": total_mix,
        "errors": s2.recent_errors,
    });

    pool.stop();
    let slug = root
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "root".into());
    let path = write_result(
        &format!(
            "thumbs-{slug}-{take}-{workers}w{}",
            ext_filter
                .as_ref()
                .map(|e| format!("-{}", e.join("_")))
                .unwrap_or_default()
        ),
        &out,
    )?;
    println!("\n{}", serde_json::to_string_pretty(&out)?);
    println!("\n结果已写入: {}", path.display());
    Ok(())
}
