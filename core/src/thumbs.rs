//! 缩略图工作池。
//!
//! ## 为什么是"少量常驻线程 + 优先级队列"而不是"线程越多越好"
//!
//! M0/M1 前置实测（见 `docs/M0-实测发现.md`）：
//! - HEIC 解码 6 线程只提速 **1.4×**，6 个进程也只有 **2.0×** → 加线程无用；
//! - MF 抽帧 6 线程比单线程**还慢**（0.39×）→ 加线程有害；
//! - 同一个操作在不同位置测出 65ms 和 1040ms（差 16 倍）→ 反复新建/释放解码器
//!   会造成"越跑越慢"。
//!
//! 因此这里的对策是：
//! 1. **工作线程数量少而常驻**（默认 2），每个线程是独立 STA 且持有自己的 WIC 工厂；
//! 2. **可见优先 + 取消**：队列按优先级排序，且每个任务执行前检查"是否还需要"，
//!    用户快速滚过时不浪费算力；
//! 3. 任务内部**先把 COM 对象用完就丢**，不在线程上堆积；
//! 4. 每类任务的耗时与失败都统计下来，方便定位"越跑越慢"是否复现。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{bail, Result};
use serde::Serialize;

use crate::cache::ThumbCache;
use crate::media::wic::PixelLayout;
use crate::media::{mf, wic};

/// 网格层级。
///
/// 从 512 提到 **1024**，理由是两个用途共用一层：
/// 1. 网格格子最大 320px CSS × DPR 2.25 ≈ 720 设备像素，1024 够用；
/// 2. **它同时是全屏查看器的占位图** —— 512 拉到全屏要放大 4.8 倍、明显发虚，
///    用户反馈"切大图先看到一张糊的"；1024 只放大 2.4 倍，观感可接受。
///
/// 代价：每张缓存从约 64KB 涨到约 180KB，编码多约 60ms（瓶颈是解码，占 85%）。
pub const TIER_GRID: &str = "grid1024";
pub const TIER_DIM: u32 = 1024;
/// 全屏查看层级：2048px 长边。全屏看图/放大时才请求，避免为整个库多算一份。
pub const TIER_SCREEN: &str = "screen2048";
pub const SCREEN_DIM: u32 = 2048;
pub const JPEG_QUALITY: u8 = 84;
pub const SCREEN_JPEG_QUALITY: u8 = 88;

#[derive(Clone, Debug)]
pub struct ThumbRequest {
    pub key: String,
    pub asset_id: usize,
    pub still: Option<PathBuf>,
    pub video: Option<PathBuf>,
    /// 数字越小越优先
    pub priority: u8,
    pub tier: String,
    /// 解码目标（长边像素）
    pub dim: u32,
    /// JPEG 质量
    pub quality: u8,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct PoolStats {
    pub queued: usize,
    pub in_flight: usize,
    pub done_ok: u64,
    pub done_err: u64,
    pub cache_hits: u64,
    pub skipped_not_wanted: u64,
    /// 从 MOV 抽海报帧成功的次数（说明走了快路径）
    pub poster_from_video: u64,
    /// 回退到剧照解码的次数
    pub poster_from_still: u64,
    pub decode_ms_total: f64,
    pub decode_ms_max: f64,
    pub recent_errors: Vec<String>,
}

struct Inner {
    queue: Mutex<Vec<ThumbRequest>>,
    cv: Condvar,
    wanted: Mutex<HashSet<String>>,
    /// "钉住"的键：全屏查看这类请求不能被下一次 set_visible 顺手丢掉，
    /// 所以它们不参与"滚过即弃"的清理。
    pinned: Mutex<HashSet<String>>,
    stop: AtomicBool,
    cache: ThumbCache,
    on_ready: Box<dyn Fn(&str, bool) + Send + Sync>,
    /// **视频解码全局串行闸门。**
    ///
    /// 实测依据：MG 抽帧在 6 线程下比单线程还慢（0.39×），且加入视频任务后
    /// 整个池的最慢一帧从 339ms 飙到 6096ms（max/avg 从 1.76 → 7.65）。
    /// 说明视频解码路径（每个任务新建一个 SourceReader）存在严重的资源竞争，
    /// 并发只会互相拖累。所以这里强制"同一时刻只有一个视频解码在跑"。
    video_slot: Mutex<bool>,
    video_cv: Condvar,
    // 统计
    queued: AtomicU64,
    in_flight: AtomicU64,
    done_ok: AtomicU64,
    done_err: AtomicU64,
    cache_hits: AtomicU64,
    skipped: AtomicU64,
    from_video: AtomicU64,
    from_still: AtomicU64,
    /// 正在解码的键（供 UI 显示"准备中"）
    in_flight_keys: Mutex<HashSet<String>>,
    /// 提交序号：用来判断"队列里这个任务是不是来自更早的批次"。
    ///
    /// 为什么需要：用户从 N 快速翻到 N+1 时，新批次把 N+1 定为优先级 0，
    /// 但**旧批次留下的 N 仍然是优先级 0**（当时它是中心）。
    /// 去重逻辑若只允许"优先级变小"，N 就永远停在 0，队列里出现两个 0，
    /// 平局按 asset_id 升序 → 先做**已经翻过去的旧图**，看起来就是
    /// "从最左侧慢慢往右加载"（用户实测反馈）。
    /// 有了序号，就可以让**最新批次重新定义优先级**（把旧中心降级为 ±1）。
    submit_counter: AtomicU64,
    job_seq: Mutex<std::collections::HashMap<String, u64>>,
    ms_total: Mutex<f64>,
    ms_max: Mutex<f64>,
    errors: Mutex<Vec<String>>,
}

pub struct ThumbPool {
    inner: Arc<Inner>,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    workers: usize,
}

impl ThumbPool {
    pub fn new(
        workers: usize,
        cache: ThumbCache,
        on_ready: Box<dyn Fn(&str, bool) + Send + Sync>,
    ) -> Self {
        let inner = Arc::new(Inner {
            queue: Mutex::new(Vec::new()),
            cv: Condvar::new(),
            wanted: Mutex::new(HashSet::new()),
            pinned: Mutex::new(HashSet::new()),
            stop: AtomicBool::new(false),
            cache,
            on_ready,
            video_slot: Mutex::new(true),
            video_cv: Condvar::new(),
            queued: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            done_ok: AtomicU64::new(0),
            done_err: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            from_video: AtomicU64::new(0),
            from_still: AtomicU64::new(0),
            in_flight_keys: Mutex::new(HashSet::new()),
            submit_counter: AtomicU64::new(0),
            job_seq: Mutex::new(std::collections::HashMap::new()),
            ms_total: Mutex::new(0.0),
            ms_max: Mutex::new(0.0),
            errors: Mutex::new(Vec::new()),
        });

        let mut handles = Vec::new();
        for i in 0..workers.max(1) {
            let inner = inner.clone();
            handles.push(
                std::thread::Builder::new()
                    .name(format!("thumb-worker-{i}"))
                    .spawn(move || worker_loop(inner))
                    .expect("启动缩略图工作线程失败"),
            );
        }

        Self {
            inner,
            handles: Mutex::new(handles),
            workers,
        }
    }

    pub fn cache(&self) -> &ThumbCache {
        &self.inner.cache
    }

    pub fn workers(&self) -> usize {
        self.workers
    }

    /// 提交任务。同名键已在队列里的会被更新（**以最新批次为准**）。
    pub fn submit(&self, reqs: Vec<ThumbRequest>) {
        // 每次调用算一个新批次号：本批次里的优先级是"相对当前焦点"算出来的，
        // 所以它比队列里旧批次的值更权威（哪怕旧值是 0）。
        let seq = self.inner.submit_counter.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut q = self.inner.queue.lock().unwrap();
            let mut seqs = self.inner.job_seq.lock().unwrap();
            for r in reqs {
                if let Some(pos) = q.iter().position(|j| j.key == r.key) {
                    let old = seqs.get(&r.key).copied().unwrap_or(0);
                    if seq > old {
                        // 最新批次重新定义优先级（旧中心会被正确地降级）
                        q[pos].priority = r.priority;
                        seqs.insert(r.key.clone(), seq);
                    }
                    continue;
                }
                seqs.insert(r.key.clone(), seq);
                q.push(r);
                self.inner.queued.fetch_add(1, Ordering::Relaxed);
            }
            // 按优先级排序（数字小在前），同级按 asset_id 保证稳定
            q.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.asset_id.cmp(&b.asset_id)));
        }
        self.inner.cv.notify_all();
    }

    /// 声明"当前真正需要的键集合"。不在集合里的待办会被丢弃
    /// （被 `pin` 钉住的除外）。
    pub fn set_wanted(&self, keys: HashSet<String>) {
        *self.inner.wanted.lock().unwrap() = keys;
        let mut q = self.inner.queue.lock().unwrap();
        let wanted = self.inner.wanted.lock().unwrap().clone();
        let pinned = self.inner.pinned.lock().unwrap().clone();
        let before = q.len();
        q.retain(|j| wanted.contains(&j.key) || pinned.contains(&j.key));
        let dropped = before - q.len();
        if dropped > 0 {
            self.inner.skipped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }

    /// 钉住一个键（全屏查看请求），不让它被"滚过即弃"清掉。
    pub fn pin(&self, key: &str) {
        self.inner.pinned.lock().unwrap().insert(key.to_string());
    }

    pub fn unpin(&self, key: &str) {
        self.inner.pinned.lock().unwrap().remove(key);
    }

    /// 查询若干键的状态，供 UI 显示"已就绪 / 准备中 / 排队中"。
    ///
    /// 存在意义：实测全屏层单张 HEIC 要 ~1050ms，用户切快了必然要等。
    /// 与其让界面看起来"卡住"，不如把"哪几张已经好了"直接告诉用户，
    /// 等待就变成可预期的事。
    pub fn states_of(&self, tier: &str, keys: &[String]) -> Vec<String> {
        let cached: Vec<bool> = keys.iter().map(|k| self.inner.cache.has(tier, k)).collect();
        let in_flight = self.inner.in_flight_keys.lock().unwrap().clone();
        let queued: HashSet<String> = self
            .inner
            .queue
            .lock()
            .unwrap()
            .iter()
            .map(|j| j.key.clone())
            .collect();
        keys.iter()
            .zip(cached)
            .map(|(k, hit)| {
                if hit {
                    "cached".to_string()
                } else if in_flight.contains(k) {
                    "busy".to_string()
                } else if queued.contains(k) {
                    "queued".to_string()
                } else {
                    "idle".to_string()
                }
            })
            .collect()
    }

    pub fn stats(&self) -> PoolStats {
        let recent_errors = self.inner.errors.lock().unwrap().clone();
        PoolStats {
            queued: self.inner.queue.lock().unwrap().len(),
            in_flight: self.inner.in_flight.load(Ordering::Relaxed) as usize,
            done_ok: self.inner.done_ok.load(Ordering::Relaxed),
            done_err: self.inner.done_err.load(Ordering::Relaxed),
            cache_hits: self.inner.cache_hits.load(Ordering::Relaxed),
            skipped_not_wanted: self.inner.skipped.load(Ordering::Relaxed),
            poster_from_video: self.inner.from_video.load(Ordering::Relaxed),
            poster_from_still: self.inner.from_still.load(Ordering::Relaxed),
            decode_ms_total: round2(*self.inner.ms_total.lock().unwrap()),
            decode_ms_max: round2(*self.inner.ms_max.lock().unwrap()),
            recent_errors,
        }
    }

    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.cv.notify_all();
        let handles = std::mem::take(&mut *self.handles.lock().unwrap());
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for ThumbPool {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker_loop(inner: Arc<Inner>) {
    // 每个工作线程都是独立 STA，并完成进程级 MFStartup
    if let Err(e) = crate::media::worker_init() {
        inner
            .errors
            .lock()
            .unwrap()
            .push(format!("worker_init 失败: {e:#}"));
        return;
    }

    // 定期回收本线程的 COM 套间与 WIC 工厂。
    // 针对实测到的渐进退化（平均 224ms → 385ms、最慢一帧 325ms → 3398ms）：
    // 那看起来是系统 HEIF 解码器在套间上累积状态/未及时释放大块内存，
    // 所以每隔 N 个任务把套间整个重建一次。0 = 关闭。
    let recycle_after: u64 = std::env::var("LIVEPHOTO_RECYCLE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut jobs_done: u64 = 0;

    loop {
        let job = {
            let mut q = inner.queue.lock().unwrap();
            loop {
                if inner.stop.load(Ordering::SeqCst) {
                    return;
                }
                if !q.is_empty() {
                    let j = q.remove(0);
                    break Some(j);
                }
                let (guard, timeout) = inner
                    .cv
                    .wait_timeout(q, std::time::Duration::from_millis(500))
                    .unwrap();
                q = guard;
                if timeout.timed_out() && inner.stop.load(Ordering::SeqCst) {
                    return;
                }
            }
        };

        let Some(job) = job else { continue };

        // 执行前再确认一次：用户可能已经滚走了（钉住的键例外）
        if !inner.wanted.lock().unwrap().contains(&job.key)
            && !inner.pinned.lock().unwrap().contains(&job.key)
        {
            inner.skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        if inner.cache.has(&job.tier, &job.key) {
            inner.cache_hits.fetch_add(1, Ordering::Relaxed);
            (inner.on_ready)(&job.key, true);
            continue;
        }

        // 视频任务走全局串行闸门（见 Inner::video_slot 的注释）
        let _video_guard = if job.video.is_some() {
            let mut free = inner.video_slot.lock().unwrap();
            while !*free && !inner.stop.load(Ordering::SeqCst) {
                free = inner.video_cv.wait(free).unwrap();
            }
            *free = false;
            Some(VideoGuard {
                inner: inner.clone(),
            })
        } else {
            None
        };

        inner.in_flight.fetch_add(1, Ordering::Relaxed);
        inner
            .in_flight_keys
            .lock()
            .unwrap()
            .insert(job.key.clone());
        let t0 = std::time::Instant::now();
        let result = generate(&job, &inner);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        inner.in_flight.fetch_sub(1, Ordering::Relaxed);
        inner.in_flight_keys.lock().unwrap().remove(&job.key);

        {
            let mut total = inner.ms_total.lock().unwrap();
            *total += ms;
            let mut mx = inner.ms_max.lock().unwrap();
            if ms > *mx {
                *mx = ms;
            }
        }

        match result {
            Ok(source) => {
                inner.done_ok.fetch_add(1, Ordering::Relaxed);
                match source {
                    Source::Video => inner.from_video.fetch_add(1, Ordering::Relaxed),
                    Source::Still => inner.from_still.fetch_add(1, Ordering::Relaxed),
                };
                (inner.on_ready)(&job.key, true);
            }
            Err(e) => {
                inner.done_err.fetch_add(1, Ordering::Relaxed);
                let mut errs = inner.errors.lock().unwrap();
                if errs.len() < 20 {
                    errs.push(format!("{}: {e:#}", job.key));
                }
                (inner.on_ready)(&job.key, false);
            }
        }

        // 定期回收：丢掉线程上的 COM 对象并重建套间
        jobs_done += 1;
        if recycle_after > 0 && jobs_done % recycle_after == 0 {
            crate::media::wic::reset_thread_factory();
            if let Err(e) = crate::media::com_reset_sta() {
                inner
                    .errors
                    .lock()
                    .unwrap()
                    .push(format!("套间回收失败: {e:#}"));
            }
        }
    }
}

/// 便捷构造函数：网格层级
pub fn grid_request(
    key: String,
    asset_id: usize,
    still: Option<PathBuf>,
    video: Option<PathBuf>,
    priority: u8,
) -> ThumbRequest {
    ThumbRequest {
        key,
        asset_id,
        still,
        video,
        priority,
        tier: TIER_GRID.to_string(),
        dim: TIER_DIM,
        quality: JPEG_QUALITY,
    }
}

/// 便捷构造函数：全屏层级
pub fn screen_request(
    key: String,
    asset_id: usize,
    still: Option<PathBuf>,
    video: Option<PathBuf>,
) -> ThumbRequest {
    ThumbRequest {
        key,
        asset_id,
        still,
        video,
        priority: 0,
        tier: TIER_SCREEN.to_string(),
        dim: SCREEN_DIM,
        quality: SCREEN_JPEG_QUALITY,
    }
}

enum Source {
    Video,
    Still,
}

/// 视频串行闸门的守卫：离开作用域即放行下一个视频任务。
struct VideoGuard {
    inner: Arc<Inner>,
}

impl Drop for VideoGuard {
    fn drop(&mut self) {
        let mut free = self.inner.video_slot.lock().unwrap();
        *free = true;
        self.inner.video_cv.notify_one();
    }
}

fn generate(job: &ThumbRequest, inner: &Inner) -> Result<Source> {
    // 快路径：Live Photo 从 MOV 抽海报帧。
    //
    // **默认关闭**，因为它在真实管线里被证伪了：
    //   - 独立测量（`bench_mf_reuse`）：NV12 首帧 65ms、RGB32 222ms；
    //   - 放进工作池与剧照解码并行后：约 **3000ms/张**（慢 14 倍），
    //     并让整个池的最慢一帧飙到 6~8 秒、max/avg 从 1.76 恶化到 10.9。
    //   按算术核对：371×3000ms + 1118×193ms → 平均 885ms，与实测 745~797ms 吻合，
    //   即"抖动"其实全来自这条路。
    //   → 结论：独立测出来的 6 倍优势不成立，不能默认启用。
    //   要用就 `LIVEPHOTO_POSTER_FROM_VIDEO=1`，并先补一轮专门的测量。
    let use_video = std::env::var("LIVEPHOTO_POSTER_FROM_VIDEO")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if use_video {
        if let Some(v) = &job.video {
            match mf::extract_frame(v, None, Some(job.dim)) {
                Ok(bmp) if bmp.w > 0 && bmp.h > 0 => {
                    let jpeg = bmp.encode_jpeg(job.quality)?;
                    inner.cache.put(&job.tier, &job.key, &jpeg)?;
                    return Ok(Source::Video);
                }
                Ok(_) => {}
                Err(_e) => {
                    // 视频抽帧失败就静默回退到剧照，不打扰用户
                }
            }
        }
    }

    if let Some(s) = &job.still {
        let bmp = wic::decode(s, Some(job.dim), PixelLayout::Rgb24)?;
        let jpeg = bmp.encode_jpeg(job.quality)?;
        inner.cache.put(&job.tier, &job.key, &jpeg)?;
        return Ok(Source::Still);
    }

    // 没有剧照（孤儿视频）时必须走视频路径
    if let Some(v) = &job.video {
        let bmp = mf::extract_frame(v, None, Some(job.dim))?;
        let jpeg = bmp.encode_jpeg(job.quality)?;
        inner.cache.put(&job.tier, &job.key, &jpeg)?;
        return Ok(Source::Video);
    }

    bail!("这个资产既没有可用的视频也没有剧照")
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
