//! LivePhoto 浏览器 —— Tauri 命令层。
//!
//! 架构约定（都来自实测结论，见 `docs/M0-实测发现.md`）：
//!
//! - **主线程只做 UI**。绝不在主线程初始化 COM/MTA：tao 创建窗口时会调
//!   `OleInitialize`（要求 STA），主线程被改成 MTA 会直接 panic。
//!   解码初始化一律在缩略图工作线程里做（`livephoto_core::media::worker_init`）。
//! - **缩略图不在命令里同步解码**。命令只负责"登记需求"，解码交给常驻工作池，
//!   完成后用 `thumb-ready` 事件通知前端按 key 去取图。这样 UI 永远不会被解码阻塞。
//! - 缩略图通过自定义协议 `thumb://` 提供（本机是 `http://thumb.localhost/<key>`），
//!   由 Rust 直接吐缓存文件，不走 IPC 序列化。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::http::{Request, Response};
use tauri::{Emitter, Manager};

use livephoto_core::cache::{CacheStats, ThumbCache};
use livephoto_core::files::scan_tree;
use livephoto_core::pairing::{pair_all, Asset, Kind};
use livephoto_core::thumbs::{
    PoolStats, ThumbPool, ThumbRequest, JPEG_QUALITY, SCREEN_DIM, SCREEN_JPEG_QUALITY, TIER_DIM,
    TIER_GRID, TIER_SCREEN,
};

/// 工作线程数：实测加线程不但无益反而有害（6 线程比单线程还慢），
/// 所以默认只开 2 个常驻线程。可用 `LIVEPHOTO_WORKERS` 覆盖。
fn worker_count() -> usize {
    std::env::var("LIVEPHOTO_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

/// 预取窗口：可见范围外再往前/往后各要几屏
const PREFETCH_SCREENS: usize = 2;

/// 任务优先级（数字越小越先算）。
///
/// 实测依据：全屏层的冷启动延迟里，**大部分是排队等待而不是计算** ——
/// 单张 2048px 端到端约 1050ms（解码占 85%，且解码成本与目标尺寸无关），
/// 但如果和网格缩略图抢 2 个工作线程，实测就变成 2332ms、甚至 5349ms。
/// 而用户在看全屏时网格是隐藏的、不会滚动，所以**全屏层应当全面优先于网格**。
///
/// 注意全屏层内部用的是 `dist`（离当前张的距离）**本身**作为优先级，不是分档 ——
/// 之前分了三档（center/±1/其余），导致 ±2 及更远同级、退化成按 id 升序，
/// 实际变成"先加载左边那几张"而不是从中心向两侧扩散（用户指出的 bug）。
const PRIO_SCREEN_CENTER: u8 = 0;
/// 胶片条缩略图：可见界面，但要排在"当前大图 + 最近邻"之后
const PRIO_STRIP_THUMB: u8 = 6;
const PRIO_GRID_VISIBLE: u8 = 40;
const PRIO_GRID_PREFETCH: u8 = 80;
/// 全屏层的优先级 = 距离（0,1,2,…），保证严格从中心向两侧扩散
const PRIO_SCREEN_MAX_DIST: u8 = 32;

struct Library {
    root: String,
    assets: Vec<Asset>,
    /// 每个资产的缩略图缓存键（与 assets 同序）
    keys: Vec<String>,
    /// 当前视图：assets 的下标序列。侧栏选了子目录时就只包含该子树。
    view: Vec<usize>,
}

impl Library {
    /// 重建 `view`。`folder = None` 表示全部。
    fn set_filter(&mut self, folder: Option<&str>) {
        match folder {
            None => self.view = (0..self.assets.len()).collect(),
            Some(prefix) => {
                let p = prefix.to_ascii_lowercase();
                let sep = std::path::MAIN_SEPARATOR.to_string();
                self.view = self
                    .assets
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| {
                        [&a.still_path, &a.video_path].into_iter().flatten().any(|path| {
                            let lp = path.to_ascii_lowercase();
                            lp == p || lp.starts_with(&format!("{p}{sep}"))
                        })
                    })
                    .map(|(i, _)| i)
                    .collect();
            }
        }
    }
}

#[derive(Default)]
struct AppState {
    lib: Mutex<Option<Library>>,
    pool: Mutex<Option<Arc<ThumbPool>>>,
    cache: Option<ThumbCache>,
    /// 当前"钉住"的全屏层级键（相邻预加载也会钉住，切走后解绑）
    screen_keys: Mutex<Vec<String>>,
    /// 当前"钉住"的网格层键（胶片条用）。它们不属于网格的可见窗口，
    /// 但必须钉住 —— 否则工作线程取出任务时会因为"不在 wanted/pinned 集合里"把它丢掉，
    /// 表现就是"提交了但永远不出图"。
    strip_keys: Mutex<Vec<String>>,
}

impl AppState {
    fn new(cache: ThumbCache) -> Self {
        Self {
            lib: Mutex::new(None),
            pool: Mutex::new(None),
            cache: Some(cache),
            screen_keys: Mutex::new(Vec::new()),
            strip_keys: Mutex::new(Vec::new()),
        }
    }

    fn cache(&self) -> ThumbCache {
        self.cache.clone().expect("缓存在构造时已初始化")
    }
}

/// 返回给前端的紧凑资产描述。刻意不含路径（7832 个资产的路径会让首包 JSON 变得很大），
/// 需要路径/EXIF 时再调 `asset_detail`。
#[derive(Serialize, Clone)]
struct AssetDto {
    id: usize,
    key: String,
    kind: &'static str,
    live: bool,
    ph: bool,
    mtime: i64,
}

#[derive(Serialize)]
struct OpenSummary {
    root: String,
    files: usize,
    dirs: usize,
    placeholders: usize,
    scan_ms: f64,
    files_per_sec: f64,
    assets: usize,
    live: usize,
    stills: usize,
    videos: usize,
    orphan_videos: usize,
    pair_ms: f64,
    workers: usize,
    cached_thumbs: usize,
}

#[derive(Serialize)]
struct AssetDetail {
    id: usize,
    kind: String,
    stem: String,
    still_path: Option<String>,
    video_path: Option<String>,
    still_size: u64,
    video_size: u64,
    mtime: i64,
    confidence: String,
    width: u32,
    height: u32,
    video_duration_ms: u64,
    video_codec: String,
    placeholder: bool,
}

fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Still => "still",
        Kind::Live => "live",
        Kind::Video => "video",
    }
}

/// 打开（扫描 + 配对）一个文件夹。**只读元数据，不导入、不复制、不改动任何文件。**
#[tauri::command]
fn open_folder(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, path: String) -> Result<OpenSummary, String> {
    let root = std::path::PathBuf::from(&path);
    if !root.is_dir() {
        return Err(format!("不是有效目录：{path}"));
    }

    let mut entries = Vec::new();
    let scan = scan_tree(&root, 0, &mut entries);

    let t_pair = std::time::Instant::now();
    let pair = pair_all(&entries);
    let pair_ms = (t_pair.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0;

    // 预计算缓存键，之后 set_visible 就只是查表
    let keys: Vec<String> = pair
        .assets
        .iter()
        .map(|a| {
            ThumbCache::key(
                TIER_GRID,
                a.still_path.as_deref(),
                a.still_mtime,
                a.still_size,
                a.video_path.as_deref(),
                a.video_mtime,
                a.video_size,
            )
        })
        .collect();

    let cached_thumbs = {
        let cache = state.cache();
        keys.iter().filter(|k| cache.has(TIER_GRID, k)).count()
    };

    let summary = OpenSummary {
        root: path.clone(),
        files: scan.files,
        dirs: scan.dirs_scanned,
        placeholders: scan.placeholders,
        scan_ms: (scan.ms * 100.0).round() / 100.0,
        files_per_sec: if scan.ms > 0.0 {
            ((scan.files as f64 / (scan.ms / 1000.0)) * 100.0).round() / 100.0
        } else {
            0.0
        },
        assets: pair.assets.len(),
        live: pair.stats.live,
        stills: pair.stats.plain_stills,
        videos: pair.stats.orphan_videos,
        orphan_videos: pair.stats.orphan_videos,
        pair_ms,
        workers: worker_count(),
        cached_thumbs,
    };

    // 换目录时先停掉旧工作池（工作线程最多 500ms 内退出）
    {
        let mut pool = state.pool.lock().unwrap();
        if let Some(p) = pool.take() {
            p.stop();
        }
    }

    let app_for_pool = app.clone();
    let pool = Arc::new(ThumbPool::new(
        worker_count(),
        state.cache(),
        Box::new(move |key: &str, ok: bool| {
            let _ = app_for_pool.emit(
                "thumb-ready",
                serde_json::json!({ "key": key, "ok": ok }),
            );
        }),
    ));
    *state.pool.lock().unwrap() = Some(pool);

    *state.lib.lock().unwrap() = Some(Library {
        root: path,
        view: (0..pair.assets.len()).collect(),
        assets: pair.assets,
        keys,
    });

    Ok(summary)
}

#[derive(Serialize, Clone)]
struct FolderDto {
    path: String,
    name: String,
    depth: usize,
    /// 该子树（含自身）的资产数
    count: usize,
}

/// 列目录树（扁平化 + 缩进深度，前端直接按顺序渲染即可）。
///
/// 只统计含照片的目录，避免把一堆无关子目录铺满侧栏。
#[tauri::command]
fn list_folders(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<FolderDto>, String> {
    let lib = state.lib.lock().unwrap();
    let lib = lib.as_ref().ok_or("还没打开任何文件夹")?;
    let root = lib.root.clone();
    let root_lower = root.to_ascii_lowercase();
    let sep = std::path::MAIN_SEPARATOR;

    // 收集所有出现过的目录，以及每个目录的直属资产数
    let mut all: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for a in &lib.assets {
        let dir = a.dir.to_ascii_lowercase();
        // 把自己和所有祖先目录都登记上（祖先计数为 0，稍后累加）
        let mut cur = dir.clone();
        loop {
            all.entry(cur.clone()).or_insert(0);
            if cur == root_lower || cur.len() <= root_lower.len() {
                break;
            }
            match cur.rfind(sep) {
                Some(i) if i >= root_lower.len().saturating_sub(1) => cur.truncate(i),
                _ => break,
            }
        }
        *all.entry(dir).or_insert(0) += 1;
    }

    // 自底向上累加，得到每个子树的总数
    let keys: Vec<String> = all.keys().cloned().collect();
    let mut totals: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for k in keys.iter().rev() {
        let direct = *all.get(k).unwrap_or(&0);
        let child_sum: usize = keys
            .iter()
            .filter(|o| {
                o.len() > k.len()
                    && o.starts_with(k.as_str())
                    && o.as_bytes().get(k.len()) == Some(&(sep as u8))
            })
            .map(|o| *totals.get(o).unwrap_or(&0))
            .sum();
        totals.insert(k.clone(), direct + child_sum);
    }

    let mut out: Vec<FolderDto> = Vec::new();
    for (path, _) in all.iter() {
        let count = *totals.get(path).unwrap_or(&0);
        if count == 0 {
            continue;
        }
        let name = if *path == root_lower {
            std::path::Path::new(&root)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| root.clone())
        } else {
            path.rsplit(sep).next().unwrap_or(path).to_string()
        };
        let depth = if path.len() > root_lower.len() {
            path[root_lower.len()..].matches(sep).count()
        } else {
            0
        };
        out.push(FolderDto {
            // 用原始大小写的路径回给前端（BTreeMap 的键是小写的）
            path: if *path == root_lower {
                root.clone()
            } else {
                path.clone()
            },
            name,
            depth,
            count,
        });
    }
    if out.len() > 3000 {
        out.truncate(3000);
    }
    Ok(out)
}

/// 一次性取回当前视图的紧凑资产描述（虚拟化网格需要随机访问）。
///
/// `folder` 传子目录路径即只显示该子树；传 `null` 表示全部。
#[tauri::command]
fn list_assets(
    state: tauri::State<'_, Arc<AppState>>,
    folder: Option<String>,
) -> Result<Vec<AssetDto>, String> {
    let mut lib = state.lib.lock().unwrap();
    let lib = lib.as_mut().ok_or("还没打开任何文件夹")?;
    lib.set_filter(folder.as_deref());

    let (assets, keys, view) = (&lib.assets, &lib.keys, &lib.view);
    Ok(view
        .iter()
        .enumerate()
        .map(|(i, &ai)| AssetDto {
            id: i,
            key: keys[ai].clone(),
            kind: kind_str(assets[ai].kind),
            live: matches!(assets[ai].kind, Kind::Live) || assets[ai].video_is_live,
            ph: assets[ai].placeholder,
            mtime: assets[ai].mtime,
        })
        .collect())
}

#[tauri::command]
fn asset_detail(state: tauri::State<'_, Arc<AppState>>, id: usize) -> Result<AssetDetail, String> {
    let lib = state.lib.lock().unwrap();
    let lib = lib.as_ref().ok_or("还没打开任何文件夹")?;
    // id 是**视图内**的下标，要经 view 映射回 assets
    let ai = *lib.view.get(id).ok_or("资产 id 越界")?;
    let a = &lib.assets[ai];
    Ok(AssetDetail {
        id,
        kind: kind_str(a.kind).to_string(),
        stem: a.stem.clone(),
        still_path: a.still_path.clone(),
        video_path: a.video_path.clone(),
        still_size: a.still_size,
        video_size: a.video_size,
        mtime: a.mtime,
        confidence: format!("{:?}", a.confidence).to_lowercase(),
        width: a.width,
        height: a.height,
        video_duration_ms: a.video_duration_ms,
        video_codec: a.video_codec.clone(),
        placeholder: a.placeholder,
    })
}

/// 前端告诉后端"我现在需要这些缩略图"。
///
/// `ids` 是可见格子的资产 id；后端会自动再外扩几屏做预取。
/// 不在"需要集合"里的待办会被取消 —— 用户快速滚过时不会白算。
#[tauri::command]
fn set_visible(state: tauri::State<'_, Arc<AppState>>, ids: Vec<usize>) -> Result<usize, String> {
    let (reqs, wanted) = {
        let lib_guard = state.lib.lock().unwrap();
        let lib = lib_guard.as_ref().ok_or("还没打开任何文件夹")?;
        if ids.is_empty() {
            (Vec::new(), HashSet::new())
        } else {
            let min = *ids.iter().min().unwrap();
            let max = *ids.iter().max().unwrap();
            let span = (max - min).max(1);
            let n = lib.view.len();
            let lo = min.saturating_sub(span * PREFETCH_SCREENS / 2);
            let hi = (max + span * PREFETCH_SCREENS / 2).min(n.saturating_sub(1));
            let visible: HashSet<usize> = ids.iter().copied().collect();

            let mut reqs = Vec::new();
            let mut wanted = HashSet::new();
            for id in lo..=hi {
                if id >= n {
                    break;
                }
                // 视图 id → assets 下标
                let ai = lib.view[id];
                let a = &lib.assets[ai];
                let key = lib.keys[ai].clone();
                wanted.insert(key.clone());
                reqs.push(ThumbRequest {
                    key,
                    asset_id: id,
                    still: a.still_path.as_ref().map(std::path::PathBuf::from),
                    video: a.video_path.as_ref().map(std::path::PathBuf::from),
                    // 可见的优先，预取的靠后
                    priority: if visible.contains(&id) { PRIO_GRID_VISIBLE } else { PRIO_GRID_PREFETCH },
                    tier: TIER_GRID.to_string(),
                    dim: TIER_DIM,
                    quality: JPEG_QUALITY,
                });
            }
            (reqs, wanted)
        }
    };

    let pool = state.pool.lock().unwrap();
    let pool = pool.as_ref().ok_or("工作池未就绪")?;
    let n = reqs.len();
    pool.set_wanted(wanted);
    pool.submit(reqs);
    Ok(n)
}

/// 批量请求全屏层级（含相邻预加载）。返回与 `ids` 同序的缓存键。
///
/// 为什么需要批量：用户实测"切换左右时先显示小图、过一会才出大图"。
/// 单张 2048px 需要先解原图再缩放着色，HEIC 可能接近 1 秒 —— 靠"切过去再请求"必然卡。
/// 所以看当前这张时，就把它前后几张一起算好；切换时直接命中缓存，延迟接近 0。
///
/// `center` 用于分配优先级：中心最高，向两侧递减，保证最该先出的先出。
#[tauri::command]
fn request_screens(
    state: tauri::State<'_, Arc<AppState>>,
    ids: Vec<usize>,
    center: usize,
) -> Result<Vec<String>, String> {
    let mut keys = Vec::with_capacity(ids.len());
    let mut reqs = Vec::with_capacity(ids.len());
    {
        let lib_guard = state.lib.lock().unwrap();
        let lib = lib_guard.as_ref().ok_or("还没打开任何文件夹")?;
        for &id in &ids {
            let ai = match lib.view.get(id) {
                Some(&v) => v,
                None => continue,
            };
            let a = &lib.assets[ai];
            let key = ThumbCache::key(
                TIER_SCREEN,
                a.still_path.as_deref(),
                a.still_mtime,
                a.still_size,
                a.video_path.as_deref(),
                a.video_mtime,
                a.video_size,
            );
            let dist = (id as isize - center as isize).unsigned_abs();
            reqs.push(ThumbRequest {
                key: key.clone(),
                asset_id: id,
                still: a.still_path.as_ref().map(std::path::PathBuf::from),
                video: a.video_path.as_ref().map(std::path::PathBuf::from),
                // **严格从中心向两侧扩散**：优先级 = 距离本身。
                // 距离相同时（比如 -1 与 +1）再按 id 排序即可，两者等价。
                priority: PRIO_SCREEN_CENTER
                    .saturating_add((dist as u8).min(PRIO_SCREEN_MAX_DIST)),
                tier: TIER_SCREEN.to_string(),
                dim: SCREEN_DIM,
                quality: SCREEN_JPEG_QUALITY,
            });
            keys.push(key);
        }
    }

    let pool = state.pool.lock().unwrap();
    let pool = pool.as_ref().ok_or("工作池未就绪")?;

    // 只解绑"这一批不再需要"的旧键，避免误伤正在算的任务
    let new_set: std::collections::HashSet<&String> = keys.iter().collect();
    {
        let mut prev = state.screen_keys.lock().unwrap();
        for old in prev.iter() {
            if !new_set.contains(old) {
                pool.unpin(old);
            }
        }
        for k in &keys {
            if !prev.contains(k) {
                pool.pin(k);
            }
        }
        *prev = keys.clone();
    }

    pool.submit(reqs);
    Ok(keys)
}

/// 为任意一组资产请求**网格层**缩略图（不改变网格的可见窗口）。
///
/// 为什么需要：胶片条的 `<img>` 走 `thumb://` 协议，而**协议只读缓存、不触发解码**；
/// 唯一会去解码网格层的是网格自己的 `set_visible`。于是胶片条里超出用户滚过范围的
/// 那些 id 永远没人生成 → 404 → 空白（用户实测反馈）。
///
/// 优先级取网格可见（40）与预取（80）之间：它比后台预取重要，但不该抢网格主体。
#[tauri::command]
fn request_grid(state: tauri::State<'_, Arc<AppState>>, ids: Vec<usize>) -> Result<usize, String> {
    let (reqs, keys) = {
        let lib_guard = state.lib.lock().unwrap();
        let lib = lib_guard.as_ref().ok_or("还没打开任何文件夹")?;
        let mut reqs = Vec::new();
        let mut keys = Vec::new();
        for &id in &ids {
            let Some(&ai) = lib.view.get(id) else { continue };
            let a = &lib.assets[ai];
            let key = lib.keys[ai].clone();
            keys.push(key.clone());
            reqs.push(ThumbRequest {
                key,
                asset_id: id,
                still: a.still_path.as_ref().map(std::path::PathBuf::from),
                video: a.video_path.as_ref().map(std::path::PathBuf::from),
                // 胶片条是**直接可见的界面**，优先级必须高于远距离预加载（±2..±4）。
                // 之前给的是 50（比网格可见格 40 还低），结果它要等队列排空才出图 ——
                // 用户看到的就是"过了一会一排一起加载出来"。
                priority: PRIO_STRIP_THUMB,
                tier: TIER_GRID.to_string(),
                dim: TIER_DIM,
                quality: JPEG_QUALITY,
            });
        }
        (reqs, keys)
    };
    let n = reqs.len();
    let pool = state.pool.lock().unwrap();
    let pool = pool.as_ref().ok_or("工作池未就绪")?;

    // **必须钉住**：只 submit 不声明的话，工作线程取出任务时会判定"不需要"而丢弃。
    // 这正是"网格能加载、胶片条不行"的原因 —— 网格走 set_visible（同时声明 wanted），
    // 而这里一开始只提交了任务。
    let new_set: std::collections::HashSet<&String> = keys.iter().collect();
    {
        let mut prev = state.strip_keys.lock().unwrap();
        for old in prev.iter() {
            if !new_set.contains(old) {
                pool.unpin(old);
            }
        }
        for k in &keys {
            if !prev.contains(k) {
                pool.pin(k);
            }
        }
        *prev = keys;
    }

    pool.submit(reqs);
    Ok(n)
}

/// 清掉胶片条钉住的网格层键（关闭查看器时调用，避免任务一直留着）。
#[tauri::command]
fn release_grid(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    let pool = state.pool.lock().unwrap();
    let Some(pool) = pool.as_ref() else { return Ok(()) };
    let mut prev = state.strip_keys.lock().unwrap();
    for k in prev.iter() {
        pool.unpin(k);
    }
    prev.clear();
    Ok(())
}

/// 查询一批全屏层键的状态：`cached` / `busy` / `queued` / `idle`。
///
/// 前端用它画底部胶片条的"预加载状态"，让用户在切快了要等的时候有心理预期。
#[tauri::command]
fn screen_states(
    state: tauri::State<'_, Arc<AppState>>,
    keys: Vec<String>,
) -> Result<Vec<String>, String> {
    let pool = state.pool.lock().unwrap();
    let pool = pool.as_ref().ok_or("工作池未就绪")?;
    Ok(pool.states_of(TIER_SCREEN, &keys))
}

/// 请求全屏层级（2048px）的图。返回它的缓存键。
///
/// 前端拿到键后直接访问 `thumb://screen2048/<key>`；若还没算好会拿到 404，
/// 等 `thumb-ready` 事件（带同一个键）到了再挂 src。
/// 这个任务会被 `pin` 住，不会被"滚过即弃"清掉。
#[tauri::command]
fn request_screen(state: tauri::State<'_, Arc<AppState>>, id: usize) -> Result<String, String> {
    let (req, key) = {
        let lib_guard = state.lib.lock().unwrap();
        let lib = lib_guard.as_ref().ok_or("还没打开任何文件夹")?;
        let ai = *lib.view.get(id).ok_or("资产 id 越界")?;
        let a = &lib.assets[ai];
        let key = ThumbCache::key(
            TIER_SCREEN,
            a.still_path.as_deref(),
            a.still_mtime,
            a.still_size,
            a.video_path.as_deref(),
            a.video_mtime,
            a.video_size,
        );
        (
            ThumbRequest {
                key: key.clone(),
                asset_id: id,
                still: a.still_path.as_ref().map(std::path::PathBuf::from),
                video: a.video_path.as_ref().map(std::path::PathBuf::from),
                priority: 0,
                tier: TIER_SCREEN.to_string(),
                dim: SCREEN_DIM,
                quality: SCREEN_JPEG_QUALITY,
            },
            key,
        )
    };

    let pool = state.pool.lock().unwrap();
    let pool = pool.as_ref().ok_or("工作池未就绪")?;
    pool.pin(&key);
    pool.submit(vec![req]);
    Ok(key)
}

/// 不再需要某个全屏图时取消钉住（切走时调用，避免它一直被算）。
#[tauri::command]
fn release_screen(state: tauri::State<'_, Arc<AppState>>, key: String) -> Result<(), String> {
    if let Some(pool) = state.pool.lock().unwrap().as_ref() {
        pool.unpin(&key);
    }
    Ok(())
}

#[tauri::command]
fn pool_stats(state: tauri::State<'_, Arc<AppState>>) -> Result<PoolStats, String> {
    let pool = state.pool.lock().unwrap();
    Ok(pool.as_ref().map(|p| p.stats()).unwrap_or_default())
}

/// 缓存上限（字节）。默认 4GB，可用 `LIVEPHOTO_CACHE_GB` 覆盖。
///
/// 不设上限的话：全屏层约 700KB/张，1 万张就是 7GB。缓存是纯派生数据，
/// 删掉只损失时间，所以按"最旧优先"淘汰是安全的。
fn cache_limit_bytes() -> u64 {
    let gb: f64 = std::env::var("LIVEPHOTO_CACHE_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4.0);
    (gb * 1024.0 * 1024.0 * 1024.0) as u64
}

/// 手动触发一次缓存回收（也用于验收）。
#[tauri::command]
fn evict_cache(state: tauri::State<'_, Arc<AppState>>) -> livephoto_core::cache::EvictStats {
    state.cache().enforce_limit(cache_limit_bytes())
}

/// 缓存上限（前端显示用）
#[tauri::command]
fn cache_limit() -> u64 {
    cache_limit_bytes()
}

#[tauri::command]
fn cache_stats(state: tauri::State<'_, Arc<AppState>>) -> CacheStats {
    state.cache().stats()
}

/// 前端诊断上报。把事件/状态写到 `m0-results/` 下的 JSON，便于在没有人工观察的情况下定位问题。
///
/// 之所以需要它：我（开发方）看不到屏幕，无法用"看一眼"来确认滚动/点击这类交互是否真的到达了页面。
#[tauri::command]
fn debug_report(name: String, payload: String) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(&payload).map_err(|e| format!("payload 不是合法 JSON: {e}"))?;
    let path = livephoto_core::benchutil::write_result(&name, &value)
        .map_err(|e| format!("写结果失败: {e}"))?;
    Ok(path.display().to_string())
}

/// 启动时自动打开的目录。来自环境变量 `LIVEPHOTO_OPEN`。
///
/// 用途：自动化验收（无需人工点界面），以及将来做"用 LivePhoto 浏览器打开"的右键菜单。
#[tauri::command]
fn initial_folder() -> Option<String> {
    std::env::var("LIVEPHOTO_OPEN").ok().filter(|s| !s.is_empty())
}

/// 工作池线程数（用于界面显示与排查）。
#[tauri::command]
fn worker_count_cmd() -> usize {
    worker_count()
}

/// 清空缩略图缓存（纯派生数据，删了不丢照片）。
#[tauri::command]
fn clear_cache(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    let cache = state.cache();
    let root = cache.root().to_path_buf();
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|e| format!("删除缓存失败: {e}"))?;
    }
    std::fs::create_dir_all(&root).map_err(|e| format!("重建缓存目录失败: {e}"))?;
    Ok(())
}

/// 自定义协议：把缓存里的 JPEG 直接吐给 WebView。
fn serve_thumb(cache: &ThumbCache, req: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let path = req.uri().path().trim_start_matches('/');
    let (tier, key) = match path.split_once('/') {
        Some((t, k)) => (t, k),
        None => (TIER_GRID, path),
    };
    let key: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(64)
        .collect();

    match cache.get(tier, &key) {
        Some(bytes) => Response::builder()
            .status(200)
            .header("Content-Type", "image/jpeg")
            .header("Cache-Control", "public, max-age=31536000, immutable")
            .body(bytes)
            .unwrap_or_else(|_| Response::new(Vec::new())),
        None => Response::builder()
            .status(404)
            .header("Content-Type", "text/plain")
            .body(b"not ready".to_vec())
            .unwrap_or_else(|_| Response::new(Vec::new())),
    }
}

/// 自定义协议：把原图/原视频直接吐给 WebView（**支持 HTTP Range**）。
///
/// 为什么需要 Range：`<video>` 要能 seek 就必须支持分段请求，
/// 否则 Chromium 只会拉整个文件甚至直接不播。
///
/// URL：`http://media.localhost/v/<viewId>` 取视频，`/s/<viewId>` 取剧照。
/// 用视图 id 而不是路径，避免把任意本地路径暴露给页面。
fn serve_media(shared: &Arc<AppState>, req: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let path = req.uri().path().trim_start_matches('/');
    let mut it = path.splitn(2, '/');
    let kind = it.next().unwrap_or("");
    let id: usize = match it.next().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return notfound("bad url"),
    };

    let file_path = {
        let lib_guard = shared.lib.lock().unwrap();
        let Some(lib) = lib_guard.as_ref() else {
            return notfound("no folder");
        };
        let Some(&ai) = lib.view.get(id) else {
            return notfound("id out of range");
        };
        let a = &lib.assets[ai];
        let p = match kind {
            "v" => a.video_path.clone(),
            "s" => a.still_path.clone(),
            _ => None,
        };
        match p {
            Some(p) => std::path::PathBuf::from(p),
            None => return notfound("no such stream"),
        }
    };

    let meta = match std::fs::metadata(&file_path) {
        Ok(m) => m,
        Err(e) => return notfound(&format!("stat failed: {e}")),
    };
    let total = meta.len();
    let ctype = match file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mov") => "video/quicktime",
        Some("mp4") => "video/mp4",
        Some("heic") | Some("heif") => "image/heic",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("tif") | Some("tiff") => "image/tiff",
        _ => "application/octet-stream",
    };

    // 解析 Range: bytes=start-end
    let range = req
        .headers()
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("bytes="))
        .and_then(|s| {
            let mut sp = s.splitn(2, '-');
            let a = sp.next().unwrap_or("").trim().parse::<u64>().ok();
            let b = sp.next().unwrap_or("").trim().parse::<u64>().ok();
            match (a, b) {
                (Some(a), b) => Some((a, b.unwrap_or(total.saturating_sub(1)))),
                (None, Some(n)) => Some((total.saturating_sub(n), total.saturating_sub(1))),
                _ => None,
            }
        });

    use std::io::{Read, Seek, SeekFrom};
    let (start, end, status) = match range {
        Some((a, b)) => {
            let a = a.min(total.saturating_sub(1));
            let b = b.min(total.saturating_sub(1));
            if a > b {
                return Response::builder()
                    .status(416)
                    .header("Content-Range", format!("bytes */{total}"))
                    .body(Vec::new())
                    .unwrap();
            }
            (a, b, 206)
        }
        None => (0, total.saturating_sub(1), 200),
    };
    let len = end - start + 1;

    let mut buf = vec![0u8; len as usize];
    if let Ok(mut f) = std::fs::File::open(&file_path) {
        if f.seek(SeekFrom::Start(start)).is_ok() && f.read_exact(&mut buf).is_err() {
            buf.clear();
        }
    } else {
        return notfound("open failed");
    }

    let mut b = Response::builder()
        .status(status)
        .header("Content-Type", ctype)
        .header("Accept-Ranges", "bytes")
        .header("Content-Length", len.to_string())
        .header("Cache-Control", "no-store");
    if status == 206 {
        b = b.header("Content-Range", format!("bytes {start}-{end}/{total}"));
    }
    b.body(buf).unwrap_or_else(|_| Response::new(Vec::new()))
}

fn notfound(msg: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(404)
        .header("Content-Type", "text/plain")
        .body(msg.as_bytes().to_vec())
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 注意：这里**不能**初始化 COM（见文件头注释）。
    let cache = ThumbCache::open_default().expect("初始化缓存目录失败");
    let cache_for_protocol = cache.clone();

    // 用 Arc 共享状态：这样自定义协议处理器也能读到当前打开的目录与视图
    let shared: Arc<AppState> = Arc::new(AppState::new(cache));
    let shared_for_media = shared.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(shared)
        .register_uri_scheme_protocol("thumb", move |_ctx, req| {
            serve_thumb(&cache_for_protocol, req)
        })
        .register_uri_scheme_protocol("media", move |_ctx, req| {
            serve_media(&shared_for_media, req)
        })
        .invoke_handler(tauri::generate_handler![
            open_folder,
            list_assets,
            list_folders,
            asset_detail,
            request_screen,
            request_screens,
            screen_states,
            request_grid,
            release_grid,
            release_screen,
            set_visible,
            pool_stats,
            cache_stats,
            cache_limit,
            evict_cache,
            clear_cache,
            initial_folder,
            worker_count_cmd,
            debug_report
        ])
        .setup(|app| {
            // 注意类型：manage 的是 Arc<AppState>，不是 AppState
            let st = app.state::<Arc<AppState>>();
            println!("[livephoto] 缓存目录: {}", st.cache().root().display());
            // 启动时先按上限回收一次（缓存是派生数据，超了就按最旧优先清理）
            let ev = st.cache().enforce_limit(cache_limit_bytes());
            println!(
                "[livephoto] 缓存回收: {:.1}MB → {:.1}MB，删除 {} 个文件",
                ev.bytes_before as f64 / 1048576.0,
                ev.bytes_after as f64 / 1048576.0,
                ev.removed
            );

            // 启动后 3 秒注一段诊断脚本，把"前端 bundle 到底有没有执行"写回文件。
            // 之所以需要它：release 版曾出现窗口起来但 JS 不执行、且没有任何报错，
            // 而开发方看不到屏幕，必须让程序自己作证。
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let script = r#"
                  (function () {
                    const info = {
                      mainMarker: !!window.__LP_MAIN__,
                      href: location.href,
                      readyState: document.readyState,
                      scripts: Array.from(document.scripts).map(s => s.src || '(inline)'),
                      stylesheets: Array.from(document.styleSheets).map(s => s.href || '(inline)'),
                      bodyChildren: document.body ? document.body.childElementCount : -1,
                      gridFound: !!document.getElementById('grid'),
                      tauriInternals: !!window.__TAURI_INTERNALS__,
                      lastError: window.__LP_LAST_ERROR__ || null,
                    };
                    if (window.__TAURI_INTERNALS__) {
                      window.__TAURI_INTERNALS__.invoke('debug_report', {
                        name: 'diag-eval',
                        payload: JSON.stringify(info),
                      }).catch(() => {});
                    }
                  })();
                "#;
                for round in 0..5 {
                    // **纯 Rust 侧的证据**：不依赖任何 JS 就能确认
                    // Rust 到底有没有拿到窗口、窗口停在什么 URL 上。
                    // 上一轮注入脚本毫无回报，必须先排除"连窗口都没拿到"。
                    let wins: Vec<serde_json::Value> = handle
                        .webview_windows()
                        .iter()
                        .map(|(label, w)| {
                            serde_json::json!({
                                "label": label,
                                "url": w.url().map(|u| u.to_string()).unwrap_or_else(|e| format!("<err {e}>")),
                                "title": w.title().unwrap_or_default(),
                            })
                        })
                        .collect();
                    let info = serde_json::json!({
                        "round": round,
                        "webviewWindowCount": wins.len(),
                        "windows": wins,
                    });
                    let _ = livephoto_core::benchutil::write_result("rside-diag", &info);
                    for (_label, w) in handle.webview_windows() {
                        let _ = w.eval(script);
                    }
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
