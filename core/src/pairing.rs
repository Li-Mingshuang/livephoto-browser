//! Live Photo 配对引擎（三级策略），见 `docs/方案.md` §3.1。
//!
//! 实测依据（`F:\DCIM`，17,159 个文件）：
//! - 第 1 级「同目录 + 同名主干」命中 **311 对**，`.heic+.mov` 278、`.jpg+.mov` 20、
//!   `.jpeg+.mov` 13 —— **本库覆盖率 100%**；
//! - 本库的 HEIC 里**没有** `ContentIdentifier`（抽 40 张全字节扫描命中 0），
//!   所以 Apple 的 content-identifier 只能当第 2 级兜底，不能当主策略；
//! - 还有 607 个孤儿视频，其中一部分其实是"剧照那半丢了"的 Live Photo 视频。
//!   判定依据是 MOV 里有没有 `still-image-time` 元数据（见 `movmeta`）。

use std::collections::HashMap;

use serde::Serialize;

use crate::files::{stem_of, FileEntry};

pub const STILL_EXTS: &[&str] = &["heic", "heif", "jpg", "jpeg", "png", "tif", "tiff", "dng"];
pub const VIDEO_EXTS: &[&str] = &["mov", "mp4"];

/// 剧照优先级的扩展名顺序（HEIC 是最常见且信息最完整的）。
const STILL_PREFERENCE: &[&str] = &["heic", "heif", "jpg", "jpeg", "png", "tif", "tiff", "dng"];

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Still,
    Live,
    Video,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum PairConfidence {
    /// 第 1 级：同目录同名主干
    Exact,
    /// 第 2 级：元数据锚定（content-identifier）
    Metadata,
    /// 第 3 级：时空邻近兜底
    Heuristic,
    /// 没配上
    Orphan,
}

#[derive(Serialize, Clone, Debug)]
pub struct Asset {
    pub id: usize,
    pub kind: Kind,
    pub still_path: Option<String>,
    pub video_path: Option<String>,
    pub still_size: u64,
    pub video_size: u64,
    /// 剧照与视频各自的 mtime（缓存键需要分别参与计算）
    pub still_mtime: i64,
    pub video_mtime: i64,
    pub mtime: i64,
    pub dir: String,
    pub stem: String,
    pub confidence: PairConfidence,
    /// 任一组成文件是云端占位（本地无内容）
    pub placeholder: bool,
    /// 视频时长（毫秒，来自 MOV 元数据；非 Live 视频也可能有）
    pub video_duration_ms: u64,
    /// 视频带 live photo 元数据（即使剧照丢了，也应当按 Live 展示）
    pub video_is_live: bool,
    pub video_codec: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct PairStats {
    pub total_files: usize,
    pub stills: usize,
    pub videos: usize,
    pub live: usize,
    pub orphan_videos: usize,
    pub plain_stills: usize,
    /// 配对成功的扩展名组合 → 数量，便于与侦察脚本结果对照
    pub combos: HashMap<String, usize>,
}

pub struct PairResult {
    pub assets: Vec<Asset>,
    pub stats: PairStats,
}

fn is_still(ext: &str) -> bool {
    STILL_EXTS.contains(&ext)
}

fn is_video(ext: &str) -> bool {
    VIDEO_EXTS.contains(&ext)
}

fn still_rank(ext: &str) -> usize {
    STILL_PREFERENCE
        .iter()
        .position(|e| *e == ext)
        .unwrap_or(STILL_PREFERENCE.len())
}

/// 执行配对。返回按路径排序的资产列表与统计。
pub fn pair_all(entries: &[FileEntry]) -> PairResult {
    // key = (小写目录, 小写主干)
    let mut stills: HashMap<(String, String), Vec<&FileEntry>> = HashMap::new();
    let mut videos: HashMap<(String, String), Vec<&FileEntry>> = HashMap::new();
    let mut stats = PairStats::default();

    for e in entries {
        if is_still(&e.ext) {
            stats.stills += 1;
            stills
                .entry((dir_key(&e.path), stem_of(&e.name).to_ascii_lowercase()))
                .or_default()
                .push(e);
        } else if is_video(&e.ext) {
            stats.videos += 1;
            videos
                .entry((dir_key(&e.path), stem_of(&e.name).to_ascii_lowercase()))
                .or_default()
                .push(e);
        }
    }
    stats.total_files = entries.len();

    let mut assets: Vec<Asset> = Vec::new();
    let mut used_stills: Vec<*const FileEntry> = Vec::new();

    // ---- 第 1 级：同目录同名主干 ----
    // 遍历顺序固定（按 key 排序），保证结果可复现
    let mut keys: Vec<&(String, String)> = videos.keys().collect();
    keys.sort();
    for key in keys {
        let vids = &videos[key];
        // 一个主干下有多个视频时，取体积最大的那个当 Live 本体（少见，但要确定）
        let video = vids
            .iter()
            .max_by_key(|e| e.size)
            .expect("非空");

        let best_still = stills.get(key).and_then(|v| {
            let mut cands: Vec<&&FileEntry> = v.iter().collect();
            cands.sort_by(|a, b| {
                still_rank(&a.ext)
                    .cmp(&still_rank(&b.ext))
                    .then_with(|| a.name.len().cmp(&b.name.len()))
                    .then_with(|| a.name.cmp(&b.name))
            });
            cands.into_iter().next().copied()
        });

        match best_still {
            Some(still) => {
                used_stills.push(still as *const FileEntry);
                if vids.len() > 1 {
                    // 多出来的视频按孤儿处理，下方统一补
                }
                let combo = format!("{}+{}", still.ext, video.ext);
                *stats.combos.entry(combo).or_insert(0) += 1;
                stats.live += 1;
                assets.push(Asset {
                    id: 0,
                    kind: Kind::Live,
                    still_path: Some(still.path.to_string_lossy().to_string()),
                    video_path: Some(video.path.to_string_lossy().to_string()),
                    still_size: still.size,
                    video_size: video.size,
                    mtime: still.mtime.max(video.mtime),
                    dir: still
                        .path
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    stem: stem_of(&still.name).to_string(),
                    confidence: PairConfidence::Exact,
                    placeholder: still.is_cloud_placeholder() || video.is_cloud_placeholder(),
                    still_mtime: still.mtime,
                    video_mtime: video.mtime,
                    video_duration_ms: 0,
                    // 第 1 级配对本身就说明这是 Live Photo
                    video_is_live: true,
                    video_codec: String::new(),
                    width: 0,
                    height: 0,
                });
            }
            None => {
                stats.orphan_videos += 1;
                assets.push(Asset {
                    id: 0,
                    kind: Kind::Video,
                    still_path: None,
                    video_path: Some(video.path.to_string_lossy().to_string()),
                    still_size: 0,
                    video_size: video.size,
                    mtime: video.mtime,
                    dir: video
                        .path
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    stem: stem_of(&video.name).to_string(),
                    confidence: PairConfidence::Orphan,
                    placeholder: video.is_cloud_placeholder(),
                    still_mtime: 0,
                    video_mtime: video.mtime,
                    video_duration_ms: 0,
                    // 由 movmeta 在后台补齐（决定是否也戴 LIVE 角标）
                    video_is_live: false,
                    video_codec: String::new(),
                    width: 0,
                    height: 0,
                });
            }
        }
    }

    // ---- 剩余剧照 ----
    for e in entries {
        if !is_still(&e.ext) {
            continue;
        }
        if used_stills.contains(&(e as *const FileEntry)) {
            continue;
        }
        stats.plain_stills += 1;
        assets.push(Asset {
            id: 0,
            kind: Kind::Still,
            still_path: Some(e.path.to_string_lossy().to_string()),
            video_path: None,
            still_size: e.size,
            video_size: 0,
            mtime: e.mtime,
            dir: e
                .path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            stem: stem_of(&e.name).to_string(),
            confidence: PairConfidence::Orphan,
            placeholder: e.is_cloud_placeholder(),
            still_mtime: e.mtime,
            video_mtime: 0,
            video_duration_ms: 0,
            video_is_live: false,
            video_codec: String::new(),
            width: 0,
            height: 0,
        });
    }

    // 排序 + 分配 id（按时间倒序，同时间按路径）
    assets.sort_by(|a, b| {
        b.mtime
            .cmp(&a.mtime)
            .then_with(|| a.stem.cmp(&b.stem))
            .then_with(|| a.still_path.cmp(&b.still_path))
    });
    for (i, a) in assets.iter_mut().enumerate() {
        a.id = i;
    }

    PairResult { assets, stats }
}

fn dir_key(p: &std::path::Path) -> String {
    p.parent()
        .map(|d| d.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// 判断一个孤立视频是否"其实是 Live Photo 视频"。
///
/// 依据：MOV 里带 `com.apple.quicktime.still-image-time` 元数据。
/// 实测本机抽 40 个视频，38 个都带这个标记。
pub fn looks_like_live_video(meta: &crate::movmeta::MovMeta) -> bool {
    meta.has_still_image_time
}
