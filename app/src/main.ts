/**
 * LivePhoto 浏览器 · 前端
 *
 * 硬约定：
 * 1. 解码永远不在前端发生。前端只说"我需要哪些格子"，收到 `thumb-ready` 再把
 *    `<img>` 挂上 src。所以滚动/缩放从不等待解码。
 * 2. 开发方看不到屏幕，交互必须自证：wheel / scroll / click / resize / zoom 等
 *    计数与关键状态会周期性写到 `m0-results/ui-<tag>.json`。
 *
 * 布局模型：不是"等宽网格"，而是**行列表**（日期标题行 + 照片行）。
 * 这样日期分组与虚拟化天然共存：每行有 y/h，二分查找即可定位可见区。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * 模块执行标记。
 *
 * 用途：release 版（内嵌 dist）曾出现"窗口起来但前端完全没跑、且无任何报错"。
 * Rust 侧会在页面加载后注入脚本读取这个标记，从而在看不到屏幕的情况下
 * 判断到底是"bundle 没执行"还是"执行了但后面出错"。
 */
(window as unknown as Record<string, unknown>).__LP_MAIN__ = true;

const GAP = 3;
const HEADER_H = 34;
const ZOOM_MIN = 80;
const ZOOM_MAX = 320;
const TIER = "grid1024";
const TIER_SCREEN = "screen2048";
const THUMB_BASE = "http://thumb.localhost";
const MEDIA_BASE = "http://media.localhost";

/**
 * 预加载半径 —— **同一个常量同时决定"请求几张"和"胶片条显示几张"**。
 *
 * 之前这两个数字是分开写的（请求 2、条带 4），结果最外那几张永远没被请求过，
 * 只能一直停在 `idle`（暗淡无动画），看起来就是"预加载动画不见了"。
 * 合成一个常量从结构上杜绝这种不一致。
 *
 * 关于半径大小：改成**严格中心优先**之后，翻页时新中心是插队到队列最前面的，
 * 唯一要等的是两个"正在飞"的解码（每个 ≤1s），与队列里排了多少张无关 ——
 * 所以半径大并不会拖慢翻页，反而缓冲更足。
 */
const PRELOAD = 4;

type AssetDto = {
  id: number;
  key: string;
  kind: "still" | "live" | "video";
  live: boolean;
  ph: boolean;
  mtime: number;
};

type Row = {
  kind: "header" | "photos";
  y: number;
  h: number;
  label?: string;
  sub?: string;
  month?: string;
  first?: number;
  count?: number;
};

type FolderDto = { path: string; name: string; depth: number; count: number };

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const grid = $<HTMLDivElement>("grid");
const spacer = $<HTMLDivElement>("spacer");
const win = $<HTMLDivElement>("window");
const statusEl = $<HTMLSpanElement>("status");
const emptyEl = $<HTMLDivElement>("empty");
const statsEl = $<HTMLElement>("stats");
const statsBody = $<HTMLPreElement>("statsBody");
const detailEl = $<HTMLElement>("detail");
const dTitle = $<HTMLElement>("dTitle");
const dBody = $<HTMLPreElement>("dBody");
const stickyEl = $<HTMLDivElement>("sticky");
const timelineEl = $<HTMLDivElement>("timeline");
const treeEl = $<HTMLDivElement>("tree");

let assets: AssetDto[] = [];
let ready = new Set<string>();
let rows: Row[] = [];
let assetRow: Int32Array = new Int32Array(0);
let totalH = 0;
let cols = 1;
let cellSize = 176;
let pitch = cellSize + GAP;
let selectedId: number | null = null;
let currentFolder: string | null = null;
const mounted = new Map<number, HTMLDivElement>();
const mountedHeads = new Map<number, HTMLDivElement>();

/* ------------------------------------------------------------------ *
 * 埋点
 * ------------------------------------------------------------------ */
const diag = {
  wheel: 0,
  ctrlWheel: 0,
  scroll: 0,
  pointerdown: 0,
  cellClick: 0,
  resize: 0,
  key: 0,
  zoom: 0,
  tlClick: 0,
  folderClick: 0,
  viewerOpen: 0,
  viewerClose: 0,
  screenLoaded: 0,
  dblClick: 0,
  livePlays: 0,
  hoverPlays: 0,
  viewerZoom: 0,
  maxScrollTop: 0,
  lastWheelDelta: 0,
  lastCellClickId: -1,
  lastError: "" as string,
  log: [] as string[],
  scrollTest: null as unknown,
  viewerTest: null as unknown,
  mediaTest: null as unknown,
  liveTest: null as unknown,
  screenTest: null as unknown,
  strip: null as unknown,
  stripHistory: [] as Array<{ t: number; cur: number; s: string; ids: number[] }>,
  stripMissingRequests: 0,
  lastOpenPrecached: null as unknown,
  videoSweep: null as unknown,
  startedAt: Date.now(),
};

function note(line: string) {
  if (diag.log.length < 140) diag.log.push(`${Date.now() - diag.startedAt}ms ${line}`);
}

function sortedCopy(a: number[]): number[] {
  return [...a].sort((x, y) => x - y);
}

async function flushDiag(tag: string) {
  try {
    const payload = {
      tag,
      now: new Date().toISOString(),
      viewport: `${window.innerWidth}x${window.innerHeight}`,
      grid: {
        clientWidth: grid.clientWidth,
        clientHeight: grid.clientHeight,
        scrollHeight: grid.scrollHeight,
        scrollTop: Math.round(grid.scrollTop),
      },
      layout: {
        cols,
        cellSize,
        pitch,
        rows: rows.length,
        assets: assets.length,
        totalH: Math.round(totalH),
        months: months.length,
        folders: treeEl.childElementCount,
        folder: currentFolder,
      },
      counters: {
        wheel: diag.wheel,
        ctrlWheel: diag.ctrlWheel,
        scroll: diag.scroll,
        cellClick: diag.cellClick,
        resize: diag.resize,
        zoom: diag.zoom,
        timelineClick: diag.tlClick,
        folderClick: diag.folderClick,
        viewerOpen: diag.viewerOpen,
        viewerClose: diag.viewerClose,
        screenLoaded: diag.screenLoaded,
        dblClick: diag.dblClick,
        livePlays: diag.livePlays,
        hoverPlays: diag.hoverPlays,
        viewerZoom: diag.viewerZoom,
        key: diag.key,
        maxScrollTop: Math.round(diag.maxScrollTop),
      },
      lastWheelDelta: diag.lastWheelDelta,
      lastCellClickId: diag.lastCellClickId,
      scrollTest: diag.scrollTest,
      viewerTest: diag.viewerTest,
      mediaTest: diag.mediaTest,
      liveTest: diag.liveTest,
      screenTest: diag.screenTest,
      strip: diag.strip,
      stripHistory: diag.stripHistory,
      videoSweep: diag.videoSweep,
      screenCachedCount: screenCached.size,
      stripMissingRequests: diag.stripMissingRequests,
      lastOpenWasPrecached: diag.lastOpenPrecached,
      screenLatency: {
        samples: screenLatency.length,
        p50: screenLatency.length ? Math.round(sortedCopy(screenLatency)[Math.floor(screenLatency.length / 2)]) : null,
        max: screenLatency.length ? Math.round(Math.max(...screenLatency)) : null,
        min: screenLatency.length ? Math.round(Math.min(...screenLatency)) : null,
        last10: screenLatency.slice(-10).map((x) => Math.round(x)),
      },
      screenThroughput: (() => {
        // 用最近 8 个样本估算"每秒能产出几张全屏层"
        const s = screenThroughput.slice(-8);
        if (s.length < 2) return { samples: screenReadyCount, perSec: null };
        const dt = (s[s.length - 1].t - s[0].t) / 1000;
        const dn = s[s.length - 1].n - s[0].n;
        return {
          samples: screenReadyCount,
          perSec: dt > 0 ? Number((dn / dt).toFixed(2)) : null,
          preloadRadius: PRELOAD,
        };
      })(),
      mounted: mounted.size,
      thumbsReady: ready.size,
      selectedId,
      lastError: diag.lastError,
      log: diag.log,
    };
    await invoke("debug_report", { name: `ui-${tag}`, payload: JSON.stringify(payload) });
  } catch (e) {
    console.error("[diag] flush 失败", e);
  }
}

/** 滚动与缩放能力的自检 */
function scrollSelfTest() {
  const before = grid.scrollTop;
  grid.scrollTop = 3000;
  const after = grid.scrollTop;
  grid.scrollTop = before;

  const beforeWheel = grid.scrollTop;
  grid.dispatchEvent(
    new WheelEvent("wheel", { deltaY: 400, deltaMode: 0, bubbles: true, cancelable: true }),
  );
  const afterWheel = grid.scrollTop;
  grid.scrollTop = beforeWheel;

  // Ctrl+滚轮缩放自检：合成一个 ctrl+wheel，看 cellSize/cols 是否变化，再缩回去
  const cellBefore = cellSize;
  const colsBefore = cols;
  grid.dispatchEvent(
    new WheelEvent("wheel", { deltaY: -120, deltaMode: 0, ctrlKey: true, bubbles: true, cancelable: true }),
  );
  const cellAfterIn = cellSize;
  const colsAfterIn = cols;
  grid.dispatchEvent(
    new WheelEvent("wheel", { deltaY: 120, deltaMode: 0, ctrlKey: true, bubbles: true, cancelable: true }),
  );
  const cellAfterOut = cellSize;

  const cs = getComputedStyle(grid);
  return {
    assignable: after === 3000,
    overflowY: cs.overflowY,
    contain: cs.contain,
    clientHeight: grid.clientHeight,
    scrollHeight: grid.scrollHeight,
    wheelHandlerWorks: afterWheel === beforeWheel + 400,
    wheelScrollDelta: afterWheel - beforeWheel,
    ctrlZoomWorks: cellAfterIn > cellBefore,
    cellBefore,
    cellAfterIn,
    colsBefore,
    colsAfterIn,
    zoomRestored: cellAfterOut === cellBefore,
    timelineTicks: tlTicks.length,
    months: months.length,
    stickyVisible: stickyEl.classList.contains("on"),
  };
}

/* ------------------------------------------------------------------ *
 * 日期
 * ------------------------------------------------------------------ */
function dayKey(ms: number): string {
  const d = new Date(ms * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

function monthKey(ms: number): string {
  const d = new Date(ms * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}`;
}

function dayLabel(ms: number): { main: string; sub: string } {
  const d = new Date(ms * 1000);
  const today = new Date();
  const same = (a: Date, b: Date) =>
    a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
  const yest = new Date(today.getTime() - 86400000);
  const week = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
  const main = same(d, today)
    ? "今天"
    : same(d, yest)
      ? "昨天"
      : `${d.getFullYear()}年${d.getMonth() + 1}月${d.getDate()}日`;
  return { main, sub: week[d.getDay()] };
}

function monthLabel(key: string): string {
  const [y, m] = key.split("-");
  return `${y}年${Number(m)}月`;
}

/* ------------------------------------------------------------------ *
 * 行列表构建（日期标题行 + 照片行）
 * ------------------------------------------------------------------ */
let months: { key: string; y: number }[] = [];

function computeCols() {
  const w = grid.clientWidth || window.innerWidth;
  cols = Math.max(1, Math.floor((w - GAP - 6) / pitch));
}

function buildRows() {
  rows = [];
  months = [];
  assetRow = new Int32Array(assets.length);
  let y = 0;
  let i = 0;

  while (i < assets.length) {
    const a0 = assets[i];
    const key = dayKey(a0.mtime);
    const lab = dayLabel(a0.mtime);
    const mk = monthKey(a0.mtime);
    if (!months.length || months[months.length - 1].key !== mk) {
      months.push({ key: mk, y });
    }

    rows.push({ kind: "header", y, h: HEADER_H, label: lab.main, sub: lab.sub, month: mk });
    y += HEADER_H;

    let j = i;
    while (j < assets.length && dayKey(assets[j].mtime) === key) j++;
    const n = j - i;

    for (let off = 0; off < n; off += cols) {
      const count = Math.min(cols, n - off);
      const first = i + off;
      rows.push({ kind: "photos", y, h: pitch, first, count });
      for (let c = 0; c < count; c++) assetRow[first + c] = rows.length - 1;
      y += pitch;
    }
    i = j;
  }

  totalH = y;
  spacer.style.height = `${totalH}px`;
}

function rowAtY(y: number): number {
  let lo = 0;
  let hi = rows.length - 1;
  let ans = 0;
  while (lo <= hi) {
    const m = (lo + hi) >> 1;
    if (rows[m].y <= y) {
      ans = m;
      lo = m + 1;
    } else hi = m - 1;
  }
  return ans;
}

function yOfAsset(ai: number): number {
  const r = assetRow[ai];
  return r >= 0 && r < rows.length ? rows[r].y : 0;
}

function assetAtY(y: number, x: number): number | null {
  if (!rows.length) return null;
  const ri = rowAtY(y);
  const r = rows[ri];
  if (r.kind !== "photos" || r.first === undefined) return null;
  const c = Math.min((r.count ?? 1) - 1, Math.max(0, Math.floor(x / pitch)));
  return r.first + c;
}

/* ------------------------------------------------------------------ *
 * 渲染
 * ------------------------------------------------------------------ */
function relayoutAll() {
  for (const el of mounted.values()) el.remove();
  mounted.clear();
  for (const el of mountedHeads.values()) el.remove();
  mountedHeads.clear();
  win.replaceChildren();
  computeCols();
  buildRows();
  renderAll();
  drawTimeline();
}

function ensureCell(ai: number, r: Row, c: number) {
  let el = mounted.get(ai);
  if (!el) {
    const a = assets[ai];
    el = document.createElement("div");
    el.className = "cell";
    el.style.width = `${cellSize}px`;
    el.style.height = `${cellSize}px`;
    el.dataset.id = String(a.id);

    const img = document.createElement("img");
    img.decoding = "async";
    img.draggable = false;
    img.dataset.key = a.key;
    el.appendChild(img);

    if (a.live) {
      const b = document.createElement("span");
      b.className = "badge";
      b.textContent = "LIVE";
      el.appendChild(b);
    }
    if (a.ph) {
      const b = document.createElement("span");
      b.className = "badge cloud";
      b.textContent = "☁";
      el.appendChild(b);
    }

    el.addEventListener("pointerdown", () => diag.pointerdown++);
    el.addEventListener("click", (ev) => {
      ev.stopPropagation();
      diag.cellClick++;
      diag.lastCellClickId = a.id;
      note(`cell click id=${a.id}`);
      select(a.id);
    });
    // 双击打开全屏查看器（单击是选中，和 Apple 相册一致）
    el.addEventListener("dblclick", (ev) => {
      ev.stopPropagation();
      diag.dblClick++;
      note(`dblclick id=${a.id}`);
      void openViewer(a.id);
    });
    // 悬停预览：只有 Live Photo 才有视频轨
    if (a.live) {
      const cell = el;
      cell.addEventListener("mouseenter", () => startHover(ai, cell));
      cell.addEventListener("mouseleave", () => stopHover());
    }

    win.appendChild(el);
    mounted.set(ai, el);
    if (ready.has(a.key)) attach(el, a.key);
  }
  el.style.transform = `translate3d(${GAP + c * pitch}px, ${r.y + GAP}px, 0)`;
}

function ensureHead(ri: number) {
  if (mountedHeads.has(ri)) return;
  const r = rows[ri];
  const el = document.createElement("div");
  el.className = "dayHead";
  el.innerHTML = `${r.label ?? ""}<span class="sub">${r.sub ?? ""}</span>`;
  el.style.transform = `translate3d(12px, ${r.y + 8}px, 0)`;
  win.appendChild(el);
  mountedHeads.set(ri, el);
}

function renderAll() {
  if (!assets.length) return;
  const top = grid.scrollTop;
  const h = grid.clientHeight;
  const from = top - pitch * 4;
  const to = top + h + pitch * 4;

  let lo = 0;
  let hi = rows.length - 1;
  let start = rows.length;
  while (lo <= hi) {
    const m = (lo + hi) >> 1;
    if (rows[m].y + rows[m].h >= from) {
      start = m;
      hi = m - 1;
    } else lo = m + 1;
  }

  const keepCells = new Set<number>();
  const keepHeads = new Set<number>();
  const visibleIds: number[] = [];
  const xMid = (grid.clientWidth || 800) / 2;

  for (let i = start; i < rows.length && rows[i].y <= to; i++) {
    const r = rows[i];
    if (r.kind === "header") {
      keepHeads.add(i);
      ensureHead(i);
    } else {
      for (let c = 0; c < (r.count ?? 0); c++) {
        const ai = (r.first ?? 0) + c;
        keepCells.add(ai);
        ensureCell(ai, r, c);
        visibleIds.push(assets[ai].id);
      }
    }
  }

  for (const [ai, el] of mounted) {
    if (!keepCells.has(ai)) {
      el.remove();
      mounted.delete(ai);
    }
  }
  for (const [ri, el] of mountedHeads) {
    if (!keepHeads.has(ri)) {
      el.remove();
      mountedHeads.delete(ri);
    }
  }

  // 悬浮日期标题：取"顶端之上最后一个标题行"
  const topRow = rows[rowAtY(top + 2)];
  if (topRow && topRow.kind === "header") {
    stickyEl.textContent = `${topRow.label ?? ""} ${topRow.sub ?? ""}`.trim();
    stickyEl.classList.add("on");
  } else {
    stickyEl.classList.remove("on");
  }

  diag.maxScrollTop = Math.max(diag.maxScrollTop, grid.scrollTop);
  updateTimelineActive();
  scheduleVisible(visibleIds);
  void xMid;
}

function attach(el: HTMLDivElement, key: string) {
  const img = el.querySelector("img");
  if (!img || img.dataset.loaded === "1") return;
  img.dataset.loaded = "1";
  img.onload = () => img.classList.add("ready");
  img.onerror = () => {
    diag.lastError = `图片加载失败 key=${key.slice(0, 8)}`;
    note(diag.lastError);
  };
  img.src = `${THUMB_BASE}/${TIER}/${key}`;
}

function select(id: number) {
  win.querySelector<HTMLDivElement>(".cell.selected")?.classList.remove("selected");
  selectedId = id;
  win.querySelector<HTMLDivElement>(`.cell[data-id="${id}"]`)?.classList.add("selected");
  void showDetail(id);
}

/* ------------------------------------------------------------------ *
 * 全屏查看器
 *
 * 两级图源：先用已经缓存的 512px 网格图顶上（零等待），
 * 同时向工作池请求 2048px 屏幕层级，算好后无缝替换。
 * 这样"打开大图"永远不会出现空白等待 —— 与 M0 的结论一致：
 * HEIC 解码约 0.9s/张，绝不能让用户等它。
 * ------------------------------------------------------------------ */
const viewerEl = $<HTMLDivElement>("viewer");
const vImg = $<HTMLImageElement>("vImg");
const vTitle = $<HTMLElement>("vTitle");
const vMeta = $<HTMLElement>("vMeta");
const vQuality = $<HTMLElement>("vQuality");

let viewerId: number | null = null;
let viewerKey: string | null = null;
/** 已请求过的全屏键：id → key（用于切换时直接命中，不必再问后端） */
const screenKeys = new Map<number, string>();
/**
 * 已知**已经算好**的全屏键。
 *
 * 有了它，切到已预加载的图时可以直接挂大图、完全不经过小图 ——
 * 否则会先显示网格层几十毫秒再被换掉，看起来就是"闪一下"（用户反馈）。
 * 两个来源：胶片条每 500ms 的 `screen_states` 查询，以及 `thumb-ready` 事件。
 */
const screenCached = new Set<string>();
/** 全屏层的延迟样本（毫秒）：从请求到可显示 */
const screenLatency: number[] = [];
let screenWaitSince = 0;
// 预加载半径见文件顶部的 PRELOAD 常量（同时决定请求数与胶片条跨度）
/** 记录全屏层实际产出速度，用来给出"能以多快速度翻页"的诚实答复 */
let screenReadyCount = 0;
const screenThroughput: Array<{ t: number; n: number }> = [];

async function openViewer(id: number) {
  if (id < 0 || id >= assets.length) return;
  const a = assets[id];

  viewerId = id;
  viewerEl.classList.remove("hidden");
  document.body.classList.add("viewer-open");
  // 换图立刻复位缩放：否则新图会带着上一张的缩放/平移显示
  vZoom = 1;
  vTx = 0;
  vTy = 0;

  // **关键：已经算好就直接挂大图，不经过小图。**
  // 无条件先挂网格层会导致"已是 2048px 的图"也先闪一下 1024px（用户反馈）。
  const knownKey = screenKeys.get(id);
  const preCached = knownKey !== undefined && screenCached.has(knownKey);
  diag.lastOpenPrecached = { id, preCached, key: knownKey?.slice(0, 8) ?? null };
  if (preCached && knownKey) {
    viewerKey = knownKey;
    vImg.src = `${THUMB_BASE}/${TIER_SCREEN}/${knownKey}`;
    vQuality.textContent = "清晰图 2048px";
  } else {
    vImg.src = `${THUMB_BASE}/${TIER}/${a.key}`;
    vQuality.textContent = "缩略图 1024px";
  }

  const d = await invoke<any>("asset_detail", { id }).catch(() => null);
  if (d) {
    vTitle.textContent = d.stem;
    const bits = [d.kind === "live" ? "Live Photo" : d.kind];
    if (d.width) bits.push(`${d.width}×${d.height}`);
    if (d.video_duration_ms) bits.push(`${(d.video_duration_ms / 1000).toFixed(1)}s`);
    bits.push(new Date(d.mtime * 1000).toLocaleString("zh-CN"));
    vMeta.textContent = bits.join(" · ");
    // 播放入口：**只有真正的 Live Photo 才自动播放**。
    // 普通视频（有些是几百 MB 的 4K 片子）绝不自动播 —— 那既不是用户意图，
    // 也会在大文件上瞬间拉满带宽/IO。
    if (d.kind === "live") {
      vLive.classList.remove("hidden");
      vMute.classList.remove("hidden");
      vLive.textContent = "LIVE ▶";
      applyMute();
      window.setTimeout(() => {
        if (viewerId === id) void playLive();
      }, 260);
    } else if (d.video_path) {
      vLive.classList.remove("hidden");
      vMute.classList.remove("hidden");
      vLive.textContent = "▶ 播放";
      applyMute();
      // 不自动播放，等用户点
    } else {
      vLive.classList.add("hidden");
      vMute.classList.add("hidden");
    }
  }

  // 批量请求：当前这张 + 前后各 PRELOAD 张。
  // 切换时相邻那张大概率已经算好，延迟接近 0（用户实测的"先小图后大图"就是这么消掉的）。
  const ids: number[] = [];
  for (let k = -PRELOAD; k <= PRELOAD; k++) {
    const t = id + k;
    if (t >= 0 && t < assets.length) ids.push(t);
  }
  try {
    const keys = await invoke<string[]>("request_screens", { ids, center: id });
    for (let i = 0; i < ids.length; i++) screenKeys.set(ids[i], keys[i]);
    const key = screenKeys.get(id);
    if (key) {
      viewerKey = key;
      // 走到这里说明进来时还不知道它是好的 —— 立刻查一次，若其实已缓存就马上换上，
      // 同时把状态记下来供下一次切换使用（下一次就不会闪了）。
      if (!preCached) {
        screenWaitSince = performance.now();
        try {
          const st = await invoke<string[]>("screen_states", { keys: [key] });
          if (st[0] === "cached") {
            screenCached.add(key);
            tryScreen();
          }
        } catch {
          /* 查不到就等事件 */
        }
      }
    }
  } catch (e) {
    note(`request_screens 失败 ${e}`);
  }

  diag.viewerOpen++;
  note(`viewer open id=${id}`);
  startStripPolling();
  void renderStrip();
  void flushDiag("viewer");
}

/** 全屏那层的图算好了就换上 */
function tryScreen() {
  if (!viewerKey) return;
  const key = viewerKey;
  const url = `${THUMB_BASE}/${TIER_SCREEN}/${key}`;
  const probe = new Image();
  probe.onload = () => {
    if (viewerKey !== key) return; // 用户已经翻到别的图了
    vImg.src = url;
    diag.screenLoaded++;
    vQuality.textContent = "清晰图 2048px";
    // 记录延迟：从发起请求到可显示
    if (screenWaitSince > 0) {
      screenLatency.push(performance.now() - screenWaitSince);
      screenWaitSince = 0;
    }
    renderStrip();
  };
  probe.onerror = () => {
    /* 还没算好，等事件通知 */
  };
  probe.src = url;
}

/* ------------------------------------------------------------------ *
 * 底部胶片条：显示前后几张的预加载状态
 *
 * 动机（用户提出）：全屏层单张 HEIC 约 1050ms、产出速度约 4.7 张/秒，
 * 切快了必然要等。与其让界面看起来"卡住"，不如把"哪几张已经就绪"直接摆出来 ——
 * 等待就从不可预期变成可预期，而且胶片条本身还能点着跳。
 * ------------------------------------------------------------------ */
const vStrip = $<HTMLDivElement>("vStrip");
const vStage = $<HTMLDivElement>("vStage");
const vZoomLabel = $<HTMLElement>("vZoom");

/* ------------------------------------------------------------------ *
 * 大图缩放与平移
 *
 * 做法：JS 按"适应窗口"算出尺寸与居中位置并显式设给元素，
 * 然后用 transform（origin 固定在左上角）做缩放与平移。
 * 这样光标锚点缩放的数学很直接：
 *   光标下的图像坐标 imgX = (clientX - (baseLeft + tx)) / z
 *   缩放后要保持它不动 → tx' = clientX - baseLeft - imgX * z'
 * ------------------------------------------------------------------ */
let vZoom = 1;
let vTx = 0;
let vTy = 0;
let vFitW = 0;
let vFitH = 0;
/** 大图最大放大倍数（与网格缩略图的 ZOOM_MAX 是两回事，故单独命名） */
const VIEW_ZOOM_MAX = 8;

function fitBox() {
  const st = vStage.getBoundingClientRect();
  const nw = vImg.naturalWidth || 1;
  const nh = vImg.naturalHeight || 1;
  const s = Math.min(st.width / nw, st.height / nh);
  return { st, w: nw * s, h: nh * s };
}

function applyTransform() {
  const t = `translate(${vTx}px, ${vTy}px) scale(${vZoom})`;
  vImg.style.transform = t;
  vVid.style.transform = t;
  vZoomLabel.textContent = vZoom > 1.01 ? `${Math.round(vZoom * 100)}%` : "适应窗口";
  vStage.classList.toggle("zoomed", vZoom > 1.01);
}

function layoutViewerImage() {
  if (!vImg.naturalWidth) return;
  const { st, w, h } = fitBox();
  vFitW = w;
  vFitH = h;
  const left = (st.width - w) / 2;
  const top = (st.height - h) / 2;
  for (const el of [vImg, vVid]) {
    el.style.width = `${w}px`;
    el.style.height = `${h}px`;
    el.style.left = `${left}px`;
    el.style.top = `${top}px`;
  }
  applyTransform();
}

function resetZoom() {
  vZoom = 1;
  vTx = 0;
  vTy = 0;
  layoutViewerImage();
}

/** 把平移限制在合理范围：放大后不留空白，缩小时保持居中 */
function clampPan() {
  const st = vStage.getBoundingClientRect();
  const baseL = (st.width - vFitW) / 2;
  const baseT = (st.height - vFitH) / 2;
  const sw = vFitW * vZoom;
  const sh = vFitH * vZoom;

  if (sw <= st.width) {
    vTx = (st.width - sw) / 2 - baseL;
  } else {
    vTx = Math.min(-baseL, Math.max(st.width - baseL - sw, vTx));
  }
  if (sh <= st.height) {
    vTy = (st.height - sh) / 2 - baseT;
  } else {
    vTy = Math.min(-baseT, Math.max(st.height - baseT - sh, vTy));
  }
}

function zoomAt(clientX: number, clientY: number, factor: number) {
  if (!vFitW) layoutViewerImage();
  const st = vStage.getBoundingClientRect();
  const baseL = st.left + (st.width - vFitW) / 2;
  const baseT = st.top + (st.height - vFitH) / 2;
  const nz = Math.max(1, Math.min(VIEW_ZOOM_MAX, vZoom * factor));
  if (Math.abs(nz - vZoom) < 0.001) return;
  const imgX = (clientX - (baseL + vTx)) / vZoom;
  const imgY = (clientY - (baseT + vTy)) / vZoom;
  vTx = clientX - baseL - imgX * nz;
  vTy = clientY - baseT - imgY * nz;
  vZoom = nz;
  clampPan();
  applyTransform();
}

// 滚轮缩放（以大图光标位置为锚点）
vStage.addEventListener(
  "wheel",
  (e) => {
    if (viewerId === null) return;
    e.preventDefault();
    diag.viewerZoom++;
    zoomAt(e.clientX, e.clientY, e.deltaY < 0 ? 1.15 : 1 / 1.15);
  },
  { passive: false },
);

// 拖动平移（放大后才有意义）
let dragging = false;
let dragStartX = 0;
let dragStartY = 0;
let dragTx = 0;
let dragTy = 0;
/** 本次按下之后指针是否真的移动过（≥3px）。原地点击不该被当成拖拽。 */
let dragMoved = false;
/** 拖拽后紧跟的 click 要忽略（否则平移一下就把查看器关了） */
let suppressNextClick = false;

vStage.addEventListener("pointerdown", (e) => {
  if (viewerId === null || vZoom <= 1.01) return;
  dragging = true;
  dragMoved = false;
  dragStartX = e.clientX;
  dragStartY = e.clientY;
  dragTx = vTx;
  dragTy = vTy;
  vStage.classList.add("dragging");
  // 合成事件（自检）没有活动指针，setPointerCapture 会抛异常；真实输入不会。
  try {
    vStage.setPointerCapture(e.pointerId);
  } catch {
    /* 忽略 */
  }
});

vStage.addEventListener("pointermove", (e) => {
  if (!dragging) return;
  if (Math.abs(e.clientX - dragStartX) > 3 || Math.abs(e.clientY - dragStartY) > 3) {
    dragMoved = true;
  }
  vTx = dragTx + (e.clientX - dragStartX);
  vTy = dragTy + (e.clientY - dragStartY);
  clampPan();
  applyTransform();
});

const endDrag = () => {
  if (!dragging) return;
  dragging = false;
  vStage.classList.remove("dragging");
  // 真拖过才抑制后面的 click；原地点击必须保留（否则"放大后点一下"会变成什么都不发生）
  if (dragMoved) {
    suppressNextClick = true;
    window.setTimeout(() => {
      suppressNextClick = false;
    }, 0);
  }
};
vStage.addEventListener("pointerup", endDrag);
vStage.addEventListener("pointercancel", endDrag);

/**
 * 光标是否落在大图（缩放/平移之后）的可视矩形内。
 *
 * 为什么不能只看 `e.target`：放大状态下一按下就 `setPointerCapture(vStage)`，
 * 于是随后的 click **总是**以 `vStage` 为 target —— 看起来"点在照片上"，
 * 其实 target 是底背景，旧逻辑就会把查看器关掉（用户反馈的
 * "放大后鼠标点一下就退回小图"）。所以这里用几何判断。
 */
function pointInsideImage(clientX: number, clientY: number): boolean {
  const st = vStage.getBoundingClientRect();
  const baseL = st.left + (st.width - vFitW) / 2 + vTx;
  const baseT = st.top + (st.height - vFitH) / 2 + vTy;
  const w = vFitW * vZoom;
  const h = vFitH * vZoom;
  return clientX >= baseL && clientX <= baseL + w && clientY >= baseT && clientY <= baseT + h;
}

// 双击复位
vStage.addEventListener("dblclick", () => {
  if (viewerId === null) return;
  resetZoom();
});

// 每次换图都重新按"适应窗口"排版 —— 用 load 事件统一覆盖所有 src 赋值点
vImg.addEventListener("load", () => {
  resetZoom();
});
const STRIP_SPAN = PRELOAD; // 与预加载半径同源，保证不会出现"显示了但没请求"
/** 胶片项按 id 复用，避免每次轮询重建 <img>（那会让小图闪烁/消失） */
const stripMap = new Map<number, HTMLDivElement>();
const stripSep = (() => {
  const d = document.createElement("div");
  d.className = "strip-sep";
  return d;
})();

async function renderStrip() {
  if (viewerId === null) {
    vStrip.replaceChildren();
    return;
  }
  const cur = viewerId;
  const ids: number[] = [];
  for (let k = -STRIP_SPAN; k <= STRIP_SPAN; k++) {
    const t = cur + k;
    if (t >= 0 && t < assets.length) ids.push(t);
  }

  // 向后端问每张的状态：cached / busy / queued / idle
  let states: string[] = ids.map(() => "idle");
  const keys = ids.map((i) => screenKeys.get(i) ?? "");
  const valid = keys.every((k) => k.length > 0);
  if (valid) {
    try {
      states = await invoke<string[]>("screen_states", { keys });
      // 顺手把"已算好"的记下来：下次切到它时就能直接挂大图、不闪
      for (let i = 0; i < keys.length; i++) {
        if (states[i] === "cached") screenCached.add(keys[i]);
        else screenCached.delete(keys[i]);
      }
    } catch {
      /* 忽略：状态显示失败不该影响看图 */
    }
  }
  if (viewerId !== cur) return; // 期间又翻页了

  // **DOM 复用，绝不重建 <img>。**
  // 之前每次轮询都 replaceChildren 重建全部胶片项，等于每 500ms 重新请求 10 张图；
  // 快速翻页时再叠加重建，浏览器图片缓存被冲掉、<img> 来不及解码，
  // 表现就是"快速滑动后小图直接不显示"。
  const wanted = new Set(ids);
  for (const [id, el] of stripMap) {
    if (!wanted.has(id)) {
      el.remove();
      stripMap.delete(id);
    }
  }

  const frag = document.createDocumentFragment();
  const missingThumbs: number[] = [];
  for (let i = 0; i < ids.length; i++) {
    const id = ids[i];
    const st = states[i] ?? "idle";

    if (i === STRIP_SPAN) frag.appendChild(stripSep);

    let el = stripMap.get(id);
    if (!el) {
      el = document.createElement("div");
      el.className = "strip-item";
      el.dataset.id = String(id);
      const img = document.createElement("img");
      img.decoding = "async";
      img.draggable = false;
      el.appendChild(img);
      // 用元素自身记录当前 src 对应的键，避免重复赋值触发重新请求
      el.dataset.key = "";
      el.addEventListener("click", () => {
        const target = Number(el!.dataset.id);
        if (target !== viewerId) void openViewer(target);
      });
      stripMap.set(id, el);
    }

    // **优先复用已经存在的图，绝不为此新触发一次解码。**
    // 因为解码成本与尺寸无关（1024 缩略图和 2048 大图都是约 900ms），
    // 为 56px 的胶片项单独解一张是极大的浪费。
    // 顺序：全屏层已缓存 → 网格层已缓存 → 占位并排队。
    const gridKey = assets[id].key;
    const scrKey = screenKeys.get(id);
    let src = "";
    let tag = "";
    if (scrKey && screenCached.has(scrKey)) {
      src = `${THUMB_BASE}/${TIER_SCREEN}/${scrKey}`;
      tag = `s:${scrKey}`;
    } else if (ready.has(gridKey)) {
      src = `${THUMB_BASE}/${TIER}/${gridKey}`;
      tag = `g:${gridKey}`;
    }

    const img = el.querySelector("img") as HTMLImageElement;
    if (src) {
      if (el.dataset.key !== tag) {
        el.dataset.key = tag;
        img.src = src;
      }
      el.classList.remove("noimg");
    } else {
      if (el.dataset.key !== "") {
        el.dataset.key = "";
        img.removeAttribute("src");
      }
      el.classList.add("noimg");
      const off = id - cur;
      el.dataset.off = off === 0 ? "•" : off > 0 ? `+${off}` : `${off}`;
      missingThumbs.push(id);
    }

    const cls = `strip-item ${st}${id === cur ? " current" : ""}`;
    if (el.className !== cls) el.className = cls;
    const tip =
      `${id === cur ? "当前" : `${i - STRIP_SPAN > 0 ? "后" : "前"} ${Math.abs(i - STRIP_SPAN)} 张`}` +
      ` · ${st === "cached" ? "已就绪" : st === "busy" ? "正在准备" : st === "queued" ? "排队中" : "未开始"}`;
    if (el.title !== tip) el.title = tip;
    frag.appendChild(el); // 移动已有节点，保留已加载的图
  }
  vStrip.replaceChildren(frag);
  // 主动为缺失的胶片缩略图排队（协议不会自动生成，必须显式请求）
  if (missingThumbs.length) {
    diag.stripMissingRequests += missingThumbs.length;
    invoke("request_grid", { ids: missingThumbs }).catch(() => {});
  }
  // 自证：胶片条确实渲染了，且状态不是一片 idle
  diag.strip = {
    items: vStrip.childElementCount,
    span: STRIP_SPAN,
    states,
    readyCount: states.filter((s) => s === "cached").length,
    current: cur,
  };
  // 状态随时间的变化序列：用来验证"确实是从中心向两侧扩散"而不是先左后右
  if (diag.stripHistory.length === 0 || diag.stripHistory[diag.stripHistory.length - 1].s !== states.join(",")) {
    diag.stripHistory.push({
      t: Math.round(performance.now()),
      cur,
      s: states.join(","),
      ids,
    });
    if (diag.stripHistory.length > 60) diag.stripHistory.shift();
  }
}

/** 胶片条状态轮询：只在查看器打开时跑 */
let stripTimer: number | null = null;
function startStripPolling() {
  stopStripPolling();
  stripTimer = window.setInterval(() => {
    if (viewerId === null) return;
    void renderStrip();
  }, 500);
}
function stopStripPolling() {
  if (stripTimer !== null) {
    window.clearInterval(stripTimer);
    stripTimer = null;
  }
}

function closeViewer() {
  if (viewerId === null) return;
  stopLive();
  stopStripPolling();
  // 清空胶片项（下次打开会按需重建）
  for (const el of stripMap.values()) el.remove();
  stripMap.clear();
  vStrip.replaceChildren();
  // 清掉胶片条钉住的网格层任务（否则它们会一直留在队列里）
  invoke("release_grid").catch(() => {});
  viewerId = null;
  viewerKey = null;
  viewerEl.classList.add("hidden");
  document.body.classList.remove("viewer-open");
  diag.viewerClose++;
}

/* ------------------------------------------------------------------ *
 * Live Photo 播放
 *
 * 实测结论（ui-media-selftest）：**WebView2 能直接播原始 HEVC MOV** ——
 * `canPlayType('codecs="hvc1"')` 返回空字符串是误导，真实加载能拿 metadata、
 * 能 play、readyState 到 4、时长 1.965s 与 MOV 元数据一致。
 * 所以这里不需要转 720p H.264 缓存（省掉约 2GB），声音也是原生的。
 * 唯一前提是自定义协议要支持 HTTP Range（播放器 seek 需要），已实现。
 * ------------------------------------------------------------------ */
const vVid = $<HTMLVideoElement>("vVid");
const vLive = $<HTMLButtonElement>("vLive");
const vMute = $<HTMLButtonElement>("vMute");
let livePlaying = false;

/**
 * 静音偏好（记住用户选择）。
 *
 * 默认**有声** —— 对齐 iOS 相册"滑到 Live Photo 自动播放并出声"的行为。
 * 注意之前这里写成了 `!== "0"`，于是"没存过偏好"也被当成静音（`null !== "0"` 为真），
 * 自动播放全程没声音。
 */
let liveMuted = localStorage.getItem("lp.liveMuted") === "1";

function applyMute() {
  vVid.muted = liveMuted;
  vMute.textContent = liveMuted ? "🔇" : "🔊";
  vMute.title = liveMuted ? "已静音（点击取消）" : "有声（点击静音）";
}

function stopLive() {
  if (!livePlaying) return;
  livePlaying = false;
  vVid.pause();
  vVid.classList.add("hidden");
  vImg.classList.remove("hidden");
  vLive.classList.remove("playing");
  vLive.textContent = "LIVE ▶";
}

async function playLive() {
  if (viewerId === null || livePlaying) return;
  const a = assets[viewerId];
  if (!a || a.kind === "still") return;

  const url = `${MEDIA_BASE}/v/${viewerId}`;
  // 用 id 而不是整段 URL 比对，避免不同 id 的 URL 互相误判
  if (vVid.dataset.aid !== String(viewerId)) {
    vVid.dataset.aid = String(viewerId);
    vVid.src = url;
  }
  livePlaying = true;
  vLive.textContent = "LIVE ■";
  vLive.classList.add("playing");
  try {
    vVid.currentTime = 0;
    applyMute();
    await vVid.play();
    vImg.classList.add("hidden");
    vVid.classList.remove("hidden");
    diag.livePlays++;
    note(`live play id=${viewerId}`);
  } catch (e) {
    // 自检里 play()/pause() 竞争会产生 AbortError，属噪声，不记为产品错误
    if (!String(e).includes("AbortError")) {
      diag.lastError = `播放失败: ${e}`;
      note(diag.lastError);
    }
    livePlaying = false;
    vLive.textContent = "LIVE ▶";
    vLive.classList.remove("playing");
  }
}

vVid.addEventListener("ended", () => stopLive());
vVid.addEventListener("error", () => {
  const err = vVid.error;
  diag.lastError = `视频错误 code=${err?.code} ${err?.message ?? ""}`;
  note(diag.lastError);
  stopLive();
});

// **点一下播放、再点一下停止**（用户要求）。
// 之前是"按下播、松开停"，导致只有长按才能播完；这里去掉松开即停。
vLive.addEventListener("click", () => {
  if (livePlaying) stopLive();
  else void playLive();
});

vMute.addEventListener("click", () => {
  liveMuted = !liveMuted;
  localStorage.setItem("lp.liveMuted", liveMuted ? "1" : "0");
  applyMute();
});

/* ---------------- 网格悬停预览 ---------------- */
const hoverVid = $<HTMLVideoElement>("hoverVid");
let hoverTimer: number | null = null;
let hoverAsset: number | null = null;

function stopHover() {
  if (hoverTimer !== null) {
    window.clearTimeout(hoverTimer);
    hoverTimer = null;
  }
  hoverAsset = null;
  hoverVid.classList.remove("shown");
  hoverVid.pause();
  hoverVid.removeAttribute("src");
}

function startHover(ai: number, el: HTMLDivElement) {
  const a = assets[ai];
  if (!a || a.kind === "still") return;
  hoverAsset = ai;
  hoverTimer = window.setTimeout(() => {
    hoverTimer = null;
    if (hoverAsset !== ai) return;
    const r = el.getBoundingClientRect();
    hoverVid.style.left = `${r.left}px`;
    hoverVid.style.top = `${r.top}px`;
    hoverVid.style.width = `${r.width}px`;
    hoverVid.style.height = `${r.height}px`;
    hoverVid.muted = true; // 悬停预览一律静音
    hoverVid.loop = true;
    hoverVid.classList.remove("shown");

    // **首帧解出来之后才淡入。**
    // 以前是设完 src 立刻显示元素，此时还没有任何画面，于是先看到一块黑底
    //（用户反馈的"悬停播放会先黑一下"）。现在用 loadeddata 作为闸门，
    // 并且在它之前元素是透明的、背景也是透明的，底下的缩略图自然透出来。
    const showOnce = () => {
      if (hoverAsset === ai) hoverVid.classList.add("shown");
    };
    hoverVid.addEventListener("loadeddata", showOnce, { once: true });

    hoverVid.src = `${MEDIA_BASE}/v/${ai}`;
    hoverVid.play().then(
      () => {
        // 若 loadeddata 已经错过（缓存命中时可能早于监听），补一次
        if (hoverVid.readyState >= 2) showOnce();
        diag.hoverPlays++;
      },
      () => stopHover(),
    );
  }, 380);
}

function viewerStep(delta: number) {
  if (viewerId === null) return;
  const next = viewerId + delta;
  if (next < 0 || next >= assets.length) return;
  void openViewer(next);
}

$<HTMLButtonElement>("vClose").addEventListener("click", () => closeViewer());
viewerEl.addEventListener("click", (e) => {
  // 拖拽后紧跟的 click 忽略掉
  if (suppressNextClick) return;
  const t = e.target as HTMLElement;
  // **点图片/视频本身绝不关闭** —— 放大后拖动或点一下看图是常规操作，
  // 之前这里只要目标是 stage 就关，导致"放大后点一下就退回小图"（用户反馈）。
  if (t === vImg || t === vVid) return;
  // 放大时指针被 capture 到 vStage，target 已经不可信，改用几何判断
  if (vZoom > 1.01 && pointInsideImage(e.clientX, e.clientY)) return;
  if (t === viewerEl || t.id === "vStage") closeViewer();
});

document.addEventListener("keydown", (e) => {
  if (viewerId === null) return;
  if (e.key === "Escape") {
    closeViewer();
    e.preventDefault();
  } else if (e.key === " " || e.code === "Space") {
    // **空格播放/停止**（用户要求）
    e.preventDefault();
    e.stopPropagation();
    if (livePlaying) stopLive();
    else void playLive();
  } else if (e.key === "ArrowRight" || e.key === "ArrowDown") {
    viewerStep(1);
    e.preventDefault();
  } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
    viewerStep(-1);
    e.preventDefault();
  } else if (e.key === "0") {
    resetZoom();
    e.preventDefault();
  } else if (e.key === "=" || e.key === "+") {
    const st = vStage.getBoundingClientRect();
    zoomAt(st.left + st.width / 2, st.top + st.height / 2, 1.25);
    e.preventDefault();
  } else if (e.key === "-" || e.key === "_") {
    const st = vStage.getBoundingClientRect();
    zoomAt(st.left + st.width / 2, st.top + st.height / 2, 1 / 1.25);
    e.preventDefault();
  }
});

/* ------------------------------------------------------------------ *
 * 时间轴
 * ------------------------------------------------------------------ */
let tlTicks: HTMLDivElement[] = [];
let tlThumb: HTMLDivElement | null = null;

function drawTimeline() {
  timelineEl.replaceChildren();
  tlTicks = [];
  tlThumb = null;
  if (!months.length || totalH <= 0) return;
  const track = timelineEl.clientHeight || grid.clientHeight - 12;

  // 轨道与滑块：之前只写了 CSS、忘了创建元素，导致只有浮动文字、
  // 旁边还留着原生滚动条，看起来像两条重叠的滚动指示器。
  const trackEl = document.createElement("div");
  trackEl.className = "track";
  timelineEl.appendChild(trackEl);

  tlThumb = document.createElement("div");
  tlThumb.className = "thumb";
  timelineEl.appendChild(tlThumb);

  let lastTop = -999;
  for (const m of months) {
    const frac = m.y / totalH;
    const top = frac * track;
    if (top - lastTop < 15) continue; // 太密就跳过，避免文字重叠
    lastTop = top;
    const t = document.createElement("div");
    t.className = "tick";
    t.textContent = monthLabel(m.key);
    t.style.top = `${top + 10}px`;
    t.dataset.y = String(m.y);
    t.dataset.key = m.key;
    timelineEl.appendChild(t);
    tlTicks.push(t);
  }
  updateTimelineThumb();
}

/** 滑块位置反映当前滚动位置（时间轴要当唯一的滚动指示器） */
function updateTimelineThumb() {
  if (!tlThumb) return;
  const track = timelineEl.clientHeight || 1;
  const max = Math.max(1, totalH - grid.clientHeight);
  const frac = Math.min(1, Math.max(0, grid.scrollTop / max));
  // 滑块长度按"一屏占全部内容的比例"估，最小 24px
  const h = Math.max(24, (grid.clientHeight / Math.max(1, totalH)) * track);
  tlThumb.style.height = `${h}px`;
  tlThumb.style.top = `${frac * (track - h) + h / 2}px`;
}

function updateTimelineActive() {
  updateTimelineThumb();
  if (!tlTicks.length) return;
  const cur = rows.length ? rows[rowAtY(grid.scrollTop + 2)] : null;
  const curKey = cur?.month ?? null;
  for (const t of tlTicks) t.classList.toggle("active", t.dataset.key === curKey);
}

function timelineScroll(clientY: number) {
  const rect = timelineEl.getBoundingClientRect();
  const track = rect.height || 1;
  const frac = Math.min(1, Math.max(0, (clientY - rect.top) / track));
  grid.scrollTop = frac * Math.max(0, totalH - grid.clientHeight);
}

timelineEl.addEventListener("pointerdown", (e) => {
  diag.tlClick++;
  timelineEl.setPointerCapture(e.pointerId);
  timelineScroll(e.clientY);
  const move = (ev: PointerEvent) => timelineScroll(ev.clientY);
  const up = () => {
    timelineEl.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
  };
  timelineEl.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up);
});

/* ------------------------------------------------------------------ *
 * 缩放
 * ------------------------------------------------------------------ */
function zoomBy(delta: number, anchorClientY?: number) {
  const old = cellSize;
  cellSize = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, cellSize + delta));
  if (cellSize === old) return;

  const anchorViewY = anchorClientY ?? grid.clientHeight / 2;
  const anchorY = grid.scrollTop + anchorViewY;
  const anchorAsset = assetAtY(anchorY, (grid.clientWidth || 800) / 2);

  pitch = cellSize + GAP;
  relayoutAll();

  if (anchorAsset !== null && anchorAsset >= 0) {
    grid.scrollTop = Math.max(0, yOfAsset(anchorAsset) - anchorViewY + GAP);
  }
  diag.zoom++;
  note(`zoom -> ${cellSize}px (cols=${cols})`);
  renderAll();
  drawTimeline();
}

/* ------------------------------------------------------------------ *
 * 需求登记
 * ------------------------------------------------------------------ */
let pendingIds: number[] | null = null;
let visibleTimer: number | null = null;

function scheduleVisible(ids: number[]) {
  pendingIds = ids;
  if (visibleTimer !== null) return;
  visibleTimer = window.setTimeout(() => {
    visibleTimer = null;
    const batch = pendingIds;
    pendingIds = null;
    if (batch && batch.length) {
      invoke("set_visible", { ids: batch }).catch((e) => note(`set_visible 失败 ${e}`));
    }
  }, 120);
}

/* ------------------------------------------------------------------ *
 * 事件
 * ------------------------------------------------------------------ */
let rafPending = false;
grid.addEventListener(
  "scroll",
  () => {
    diag.scroll++;
    if (rafPending) return;
    rafPending = true;
    requestAnimationFrame(() => {
      rafPending = false;
      renderAll();
    });
  },
  { passive: true },
);

grid.addEventListener(
  "wheel",
  (e) => {
    diag.wheel++;
    diag.lastWheelDelta = Math.round(e.deltaY);

    // Ctrl + 滚轮 = 缩放缩略图（用户要求的）。也接受触控板的 pinch（ctrlKey 由浏览器合成）。
    if (e.ctrlKey || e.metaKey) {
      diag.ctrlWheel++;
      e.preventDefault();
      zoomBy(e.deltaY < 0 ? 16 : -16, e.clientY - grid.getBoundingClientRect().top);
      return;
    }

    // 自己接管滚轮：实测 WebView2 里真实滚轮事件到达页面却不触发原生滚动，
    // 而滚动器本身正常（overflow-y=scroll、scrollHeight 充足、赋值 scrollTop 生效）。
    e.preventDefault();
    const dy = e.deltaMode === 1 ? e.deltaY * pitch : e.deltaY;
    grid.scrollTop += dy;
  },
  { passive: false },
);

grid.addEventListener("pointerdown", () => diag.pointerdown++);

grid.addEventListener("click", (e) => {
  const t = e.target as HTMLElement;
  if (t === grid || t.id === "spacer" || t.id === "window") {
    win.querySelector<HTMLDivElement>(".cell.selected")?.classList.remove("selected");
    selectedId = null;
  }
});

grid.addEventListener("keydown", (e) => {
  diag.key++;
  const page = grid.clientHeight * 0.9;
  const jump = (to: number) => {
    grid.scrollTop = to;
    e.preventDefault();
  };
  if (e.key === "Escape") {
    detailEl.classList.add("hidden");
    win.querySelector<HTMLDivElement>(".cell.selected")?.classList.remove("selected");
    selectedId = null;
  } else if (e.key === "ArrowDown") jump(grid.scrollTop + pitch);
  else if (e.key === "ArrowUp") jump(grid.scrollTop - pitch);
  else if (e.key === "PageDown" || e.key === " ") jump(grid.scrollTop + page);
  else if (e.key === "PageUp") jump(grid.scrollTop - page);
  else if (e.key === "Home") jump(0);
  else if (e.key === "End") jump(totalH);
  else if (e.key === "=" || e.key === "+") zoomBy(24);
  else if (e.key === "-") zoomBy(-24);
});

let resizeTimer: number | null = null;
window.addEventListener("resize", () => {
  diag.resize++;
  if (resizeTimer !== null) window.clearTimeout(resizeTimer);
  resizeTimer = window.setTimeout(() => {
    resizeTimer = null;
    note(`resize -> ${window.innerWidth}x${window.innerHeight}`);
    if (assets.length) relayoutAll();
  }, 60);
});

/* ------------------------------------------------------------------ *
 * 后端交互
 * ------------------------------------------------------------------ */
function setStatus(text: string) {
  statusEl.textContent = text;
}

async function initEvents() {
  await listen<{ key: string; ok: boolean }>("thumb-ready", (ev) => {
    const { key, ok } = ev.payload;
    if (!ok) return;
    ready.add(key);
    for (const el of mounted.values()) {
      const img = el.querySelector("img");
      if (img && img.dataset.key === key) attach(el, key);
    }
    // 统计全屏层的产出速度：决定"用户能以多快的速度翻页而不看到模糊图"
    if (isScreenKey(key)) {
      screenCached.add(key);
      screenReadyCount++;
      screenThroughput.push({ t: performance.now(), n: screenReadyCount });
      if (screenThroughput.length > 400) screenThroughput.shift();
    }
    // 全屏那层的图算好了就换上
    if (viewerKey && key === viewerKey) tryScreen();
  });
}

function isScreenKey(key: string): boolean {
  for (const k of screenKeys.values()) if (k === key) return true;
  return false;
}

async function refreshStats() {
  try {
    const [p, c, w] = await Promise.all([
      invoke<any>("pool_stats"),
      invoke<any>("cache_stats"),
      invoke<any>("worker_count_cmd"),
    ]);
    const avg = p.done_ok > 0 ? (p.decode_ms_total / p.done_ok).toFixed(0) : "-";
    statsBody.textContent = [
      `工作线程     ${w}`,
      `可见格子     ${mounted.size}   已出图 ${ready.size}`,
      `资产总数     ${assets.length}   日期组 ${months.length}`,
      `每行 ${cols} 格   格子 ${cellSize}px`,
      ``,
      `队列待办     ${p.queued}`,
      `正在解码     ${p.in_flight}`,
      `完成 / 失败  ${p.done_ok} / ${p.done_err}`,
      `缓存命中     ${p.cache_hits}`,
      `平均解码     ${avg} ms   最慢 ${p.decode_ms_max.toFixed(0)} ms`,
      ``,
      `缓存文件     ${c.files} 个 / ${(c.bytes / 1048576).toFixed(1)} MB`,
      ``,
      `wheel ${diag.wheel}（含 ctrl ${diag.ctrlWheel}）scroll ${diag.scroll}`,
      `click ${diag.cellClick} zoom ${diag.zoom} 时间轴 ${diag.tlClick} 侧栏 ${diag.folderClick}`,
      `scrollTop ${Math.round(grid.scrollTop)} / ${Math.round(Math.max(0, totalH - grid.clientHeight))}`,
      p.recent_errors.length ? `\n最近错误:\n${p.recent_errors.slice(0, 4).join("\n")}` : "",
    ].join("\n");
  } catch (e) {
    statsBody.textContent = `统计获取失败: ${e}`;
  }
}

async function showDetail(id: number) {
  try {
    const d = await invoke<any>("asset_detail", { id });
    dTitle.textContent = d.stem;
    dBody.textContent = [
      `类型         ${d.kind}${d.live ? "（Live Photo）" : ""}`,
      `配对置信度   ${d.confidence}`,
      `拍摄/修改    ${new Date(d.mtime * 1000).toLocaleString("zh-CN")}`,
      d.width ? `分辨率       ${d.width} × ${d.height}` : "",
      d.video_duration_ms ? `视频时长     ${(d.video_duration_ms / 1000).toFixed(2)} s` : "",
      d.video_codec ? `视频编码     ${d.video_codec}` : "",
      ``,
      d.still_path ? `剧照 ${(d.still_size / 1048576).toFixed(2)} MB\n${d.still_path}` : "",
      d.video_path ? `视频 ${(d.video_size / 1048576).toFixed(2)} MB\n${d.video_path}` : "",
      d.placeholder ? `\n（云端占位：内容不在本地，需先下载）` : "",
      ``,
      `看大图与播放 Live Photo 是下一步要做的。`,
    ]
      .filter(Boolean)
      .join("\n");
    detailEl.classList.remove("hidden");
  } catch (e) {
    diag.lastError = `asset_detail 失败: ${e}`;
    setStatus(diag.lastError);
  }
}

/* ------------------------------------------------------------------ *
 * 目录树
 * ------------------------------------------------------------------ */
async function loadTree(root: string) {
  try {
    const folders = await invoke<FolderDto[]>("list_folders");
    treeEl.replaceChildren();
    for (const f of folders) {
      const el = document.createElement("div");
      el.className = "folder";
      el.style.paddingLeft = `${12 + f.depth * 12}px`;
      el.title = f.path;
      el.dataset.path = f.path;
      el.innerHTML = `<span class="nm">${f.name}</span><span class="cnt">${f.count}</span>`;
      if (currentFolder === f.path || (currentFolder === null && f.path.toLowerCase() === root.toLowerCase())) {
        el.classList.add("active");
      }
      el.addEventListener("click", () => {
        diag.folderClick++;
        currentFolder = f.path;
        note(`folder -> ${f.path}`);
        treeEl.querySelectorAll(".folder.active").forEach((x) => x.classList.remove("active"));
        el.classList.add("active");
        void reloadAssets();
      });
      treeEl.appendChild(el);
    }
  } catch (e) {
    note(`list_folders 失败 ${e}`);
  }
}

/* ------------------------------------------------------------------ *
 * 打开 / 过滤
 * ------------------------------------------------------------------ */
async function reloadAssets() {
  assets = await invoke<AssetDto[]>("list_assets", { folder: currentFolder });
  ready = new Set();
  selectedId = null;
  // **必须一起清掉**：screenKeys/screenCached 是按"视图 id"索引的，
  // 换目录后同一个 id 会指向完全不同的照片。之前漏了这一步，于是打开大图时
  // 用了上一批照片的缓存键 —— 显示的照片与播放的视频来自两张不同的图
  //（用户反馈的"大图里播放和照片不一致"）。
  screenKeys.clear();
  screenCached.clear();
  mounted.clear();
  mountedHeads.clear();
  win.replaceChildren();
  detailEl.classList.add("hidden");
  grid.scrollTop = 0;
  relayoutAll();
  grid.focus();
}

async function open(path: string) {
  setStatus(`正在扫描 ${path} …`);
  const t0 = performance.now();
  try {
    const s = await invoke<any>("open_folder", { path });
    currentFolder = null;
    await reloadAssets();
    emptyEl.classList.add("hidden");
    await loadTree(path);

    const ms = (performance.now() - t0).toFixed(0);
    setStatus(
      `${path} · ${s.assets} 项（Live ${s.live} / 剧照 ${s.stills} / 视频 ${s.videos}）` +
        ` · 扫描 ${s.scan_ms.toFixed(0)}ms · 配对 ${s.pair_ms.toFixed(0)}ms · 每行 ${cols} 格 · ${s.workers} 线程 · 端到端 ${ms}ms`,
    );
    note(`open ${path} assets=${s.assets} cols=${cols} cell=${cellSize}`);
    void refreshStats();
    void flushDiag("opened");
  } catch (e) {
    diag.lastError = `打开失败: ${e}`;
    setStatus(diag.lastError);
    void flushDiag("error");
  }
}

/* ------------------------------------------------------------------ *
 * 工具栏
 * ------------------------------------------------------------------ */
$<HTMLButtonElement>("open").addEventListener("click", () => {
  void open($<HTMLInputElement>("path").value.trim());
});

$<HTMLInputElement>("path").addEventListener("keydown", (e) => {
  if (e.key === "Enter") void open($<HTMLInputElement>("path").value.trim());
});

$<HTMLButtonElement>("refresh").addEventListener("click", () => {
  statsEl.classList.toggle("hidden");
  void refreshStats();
});

$<HTMLButtonElement>("zoomIn").addEventListener("click", () => zoomBy(24));
$<HTMLButtonElement>("zoomOut").addEventListener("click", () => zoomBy(-24));

$<HTMLButtonElement>("toggleSide").addEventListener("click", () => {
  document.body.classList.toggle("side-hidden");
  window.setTimeout(() => relayoutAll(), 80);
});

/** 查看器自检：在真实格子上合成 dblclick，验证"打开 → 请求 2048px → 换上"整条链路。 */
function viewerSelfTest() {
  const cell = win.querySelector<HTMLDivElement>(".cell");
  if (!cell) return;
  cell.dispatchEvent(new MouseEvent("dblclick", { bubbles: true, cancelable: true }));
  // 2048px 需要解码，给它时间；2.5s 后再落一次盘
  window.setTimeout(() => {
    // 大图缩放缓验：在舞台中心合成一次滚轮，看 vZoom 是否变化
    let zoomBefore = 0;
    let zoomAfter = 0;
    let clickTest: Record<string, unknown> | null = null;
    try {
      const st = vStage.getBoundingClientRect();
      zoomBefore = vZoom;
      vStage.dispatchEvent(
        new WheelEvent("wheel", {
          deltaY: -120,
          clientX: st.left + st.width / 2,
          clientY: st.top + st.height / 2,
          bubbles: true,
          cancelable: true,
        }),
      );
      zoomAfter = vZoom;
      clickTest = viewerClickSelfTest(st);
      resetZoom();
    } catch (e) {
      clickTest = { error: String(e) };
    }
    diag.viewerTest = {
      opened: diag.viewerOpen,
      closed: diag.viewerClose,
      screenLoaded: diag.screenLoaded,
      viewerVisible: !viewerEl.classList.contains("hidden"),
      qualityText: vQuality.textContent,
      imgSrcTail: vImg.src.slice(-24),
      zoomBefore,
      zoomAfter,
      zoomWorks: zoomAfter > zoomBefore,
      fitW: Math.round(vFitW),
      fitH: Math.round(vFitH),
      clickTest,
    };
    closeViewer();
    void flushDiag("viewer-selftest");
  }, 2500);
}

/**
 * "放大之后点一下会不会退回小图"的自检。
 *
 * 用户反馈的原始现象：放大后鼠标点一下，直接退回网格。
 * 根因是 WebView2 里 `setPointerCapture` 会把随后的 click 的 target 改成捕获元素
 * （也就是 `vStage`），于是"点在照片上"在事件里看起来和"点底背景"完全一样，
 * 旧逻辑就把查看器关了。
 *
 * 这里合成两次点击，把两种意图分开验证：
 *   1. 放大状态下点在**照片范围内** → 查看器必须还开着（修复点）
 *   2. 缩放复位后点在**舞台角落**（照片外）→ 查看器必须关闭（保留"点空白退出"）
 */
function viewerClickSelfTest(st: DOMRect): Record<string, unknown> {
  const cx = st.left + st.width / 2;
  const cy = st.top + st.height / 2;
  const hitInside = pointInsideImage(cx, cy);
  // 放大状态下按一下再抬起（模拟真实的"点一下"，不是拖拽）
  vStage.dispatchEvent(
    new PointerEvent("pointerdown", { pointerId: 1, clientX: cx, clientY: cy, bubbles: true }),
  );
  vStage.dispatchEvent(
    new PointerEvent("pointerup", { pointerId: 1, clientX: cx, clientY: cy, bubbles: true }),
  );
  const idBefore = viewerId;
  vStage.dispatchEvent(
    new MouseEvent("click", { clientX: cx, clientY: cy, bubbles: true, cancelable: true }),
  );
  const stayedAfterImageClick = viewerId === idBefore;

  // 场景 2：复位缩放，点照片之外的角落
  resetZoom();
  const cornerX = st.left + 2;
  const cornerY = st.top + 2;
  const hitCorner = pointInsideImage(cornerX, cornerY);
  vStage.dispatchEvent(
    new MouseEvent("click", {
      clientX: cornerX,
      clientY: cornerY,
      bubbles: true,
      cancelable: true,
    }),
  );
  const closedOnBackdropClick = viewerId === null;

  return {
    zoomAtTest: vZoom,
    hitInside,
    stayedAfterImageClick,
    hitCorner,
    closedOnBackdropClick,
    pass: hitInside && stayedAfterImageClick && !hitCorner && closedOnBackdropClick,
  };
}

/**
 * M4 前置实验：WebView2 能不能直接播原始 HEVC MOV？
 *
 * 这是整个播放方案的分叉点：
 *   - 能播 → 用 `<video>` 直接指向原文件，零转码、零缓存、声音原生，M4 立刻变简单；
 *   - 不能播 → 必须走"后台转 720p H.264 预览片段"或"原生叠加窗口"，代价完全不同。
 * 所以我先测，不猜。
 */
async function mediaSelfTest() {
  const probe = document.createElement("video");
  const support = {
    hvc1: probe.canPlayType('video/mp4; codecs="hvc1"'),
    hev1: probe.canPlayType('video/mp4; codecs="hev1"'),
    avc1: probe.canPlayType('video/mp4; codecs="avc1.42E01E"'),
    quicktime: probe.canPlayType("video/quicktime"),
    mp4: probe.canPlayType("video/mp4"),
    mov_as_mp4: probe.canPlayType('video/mp4; codecs="hvc1.1.6.L93.B0"'),
  };

  const liveIdx = assets.findIndex((a) => a.kind === "live");
  if (liveIdx < 0) {
    diag.mediaTest = { support, error: "库里没有 Live Photo 可测" };
    return;
  }

  const url = `${MEDIA_BASE}/v/${liveIdx}`;
  const v = document.createElement("video");
  v.muted = true;
  v.preload = "auto";
  v.src = url;

  const result: Record<string, unknown> = {
    support,
    liveIdx,
    src: url,
    key: assets[liveIdx].key.slice(0, 12),
  };

  const done = new Promise<void>((resolve) => {
    const finish = () => resolve();
    const timer = window.setTimeout(() => {
      result.timedOut = true;
      finish();
    }, 6000);
    v.addEventListener("loadedmetadata", () => {
      result.loadedMetadata = true;
      result.videoWidth = v.videoWidth;
      result.videoHeight = v.videoHeight;
      result.duration = v.duration;
    });
    v.addEventListener("canplay", () => {
      result.canPlay = true;
      window.clearTimeout(timer);
      // 再试一下真的播起来（桌面 WebView 通常允许无手势播放）
      v.play()
        .then(() => {
          result.playResolved = true;
          result.currentTime = v.currentTime;
          setTimeout(() => {
            result.playedTo = v.currentTime;
            result.readyState = v.readyState;
            v.pause();
            finish();
          }, 700);
        })
        .catch((e) => {
          result.playRejected = String(e);
          finish();
        });
    });
    v.addEventListener("error", () => {
      const err = v.error;
      result.videoError = err ? { code: err.code, message: err.message } : "unknown";
      window.clearTimeout(timer);
      finish();
    });
  });

  await done;
  result.readyState = v.readyState;
  result.networkState = v.networkState;
  diag.mediaTest = result;
  note(`mediaTest canPlay=${!!result.canPlay} err=${JSON.stringify(result.videoError ?? null)}`);
  void flushDiag("media-selftest");
}

/** Live Photo 播放自检：打开第一张 Live Photo，按播放，确认时间在前进。 */
function liveSelfTest() {
  const idx = assets.findIndex((a) => a.kind === "live");
  if (idx < 0) {
    diag.liveTest = { error: "库里没有 Live Photo" };
    void flushDiag("live-selftest");
    return;
  }
  void openViewer(idx).then(() => {
    setTimeout(() => {
      void playLive().then(() => {
        setTimeout(() => {
          diag.liveTest = {
            idx,
            livePlays: diag.livePlays,
            playing: livePlaying,
            currentTime: Number(vVid.currentTime.toFixed(3)),
            duration: Number((vVid.duration || 0).toFixed(3)),
            videoW: vVid.videoWidth,
            videoH: vVid.videoHeight,
            readyState: vVid.readyState,
            muted: vVid.muted,
            liveButtonVisible: !vLive.classList.contains("hidden"),
            error: vVid.error ? { code: vVid.error.code, message: vVid.error.message } : null,
          };
          stopLive();
          closeViewer();
          void flushDiag("live-selftest");
        }, 900);
      });
    }, 1200);
  });
}

/**
 * 全屏层延迟自检：对比"冷启动单张"与"切换到已预加载的相邻张"。
 *
 * 用户实测的问题是"左右切时先显示小图、过一会才出大图"。
 * 这里用数字验证两点：① 冷启动到底要多久；② 预加载后切换是否接近 0。
 */
function waitFor(cond: () => boolean, timeoutMs: number): Promise<boolean> {
  return new Promise((resolve) => {
    const t0 = performance.now();
    const tick = () => {
      if (cond()) return resolve(true);
      if (performance.now() - t0 > timeoutMs) return resolve(false);
      window.setTimeout(tick, 60);
    };
    tick();
  });
}

async function screenSelfTest() {
  closeViewer();
  // 关键：必须用**从没被请求过**的位置，否则量到的是缓存命中而不是冷启动
  // （前面 viewerSelfTest 已经在第 0 张上算过一次，第一次跑就踩了这个坑）。
  const base = Math.min(500, Math.max(0, assets.length - 3));
  const before = screenLatency.length;

  await openViewer(base);
  const coldOk = await waitFor(() => screenLatency.length > before, 15000);
  const coldMs = coldOk ? Math.round(screenLatency[before]) : null;

  // 给预加载留出时间：真实使用里用户看图也会停留，所以等 2.5s 再切是合理场景
  await new Promise((r) => setTimeout(r, 2500));

  // 切到相邻张：它应当已经被预加载算好了
  const n1 = screenLatency.length;
  const t0 = performance.now();
  await openViewer(base + 1);
  const warmOk = await waitFor(() => screenLatency.length > n1, 15000);
  const warmMs = warmOk ? Math.round(screenLatency[n1]) : null;
  const switchElapsed = Math.round(performance.now() - t0);

  // 再切一张更远的（base+3，超出预加载半径 2）：应当退化成冷启动
  const n2 = screenLatency.length;
  await openViewer(base + 3);
  const farOk = await waitFor(() => screenLatency.length > n2, 15000);
  const farMs = farOk ? Math.round(screenLatency[n2]) : null;

  diag.screenTest = {
    base,
    coldMs,
    warmMs,
    farMs,
    switchElapsedMs: switchElapsed,
    coldTimedOut: !coldOk,
    warmTimedOut: !warmOk,
    farTimedOut: !farOk,
    preloadRadius: PRELOAD,
    latencySamples: screenLatency.map((x) => Math.round(x)),
  };
  note(`screenSelfTest base=${base} cold=${coldMs}ms warm=${warmMs}ms far=${farMs}ms`);
  closeViewer();
  void flushDiag("screen-selftest");
}

/* ------------------------------------------------------------------ *
 * 视频扫描自检：定位"有些视频文件点开直接卡死"
 *
 * 动机（用户实测反馈）：某些视频点开就卡死。这句话里有两种完全不同的故障，
 * 必须用数字把它们区分开，不能靠猜：
 *   a) 主线程被同步工作堵住 → 页面自己的定时器都会停摆（能测出"停顿"）
 *   b) 单个文件解码/请求失败  → <video> 会给 error / 一直 timeout（能测出 error）
 *
 * 做法：逐个真加载 `<video>`（隐藏元素，走的就是产品里完全相同的
 * `media://` 协议 + Range 路径），同时跑一个 50ms 心跳记录主线程最大停顿。
 * **每测完一个文件就写盘一次** —— 万一真的把应用卡死，最后落盘的 JSON
 * 就精确指出了是哪个文件、哪个字节数。
 *
 * 触发方式：环境变量 `LIVEPHOTO_SWEEP=<样本数>`（默认关闭，普通用户零开销）。
 * ------------------------------------------------------------------ */
type StallWatch = { timer: number; max: number; count: number; last: number; samples: number[] };

function startStallWatch(): StallWatch {
  const w: StallWatch = { timer: 0, max: 0, count: 0, last: performance.now(), samples: [] };
  w.timer = window.setInterval(() => {
    const now = performance.now();
    const gap = now - w.last;
    w.last = now;
    if (gap > 120) {
      w.count++;
      w.samples.push(Math.round(gap));
      if (gap > w.max) w.max = gap;
    }
  }, 50);
  return w;
}

async function probeOneVideo(id: number, timeoutMs: number) {
  const url = `${MEDIA_BASE}/v/${id}`;
  let detail: any = null;
  try {
    detail = await invoke<any>("asset_detail", { id });
  } catch {
    /* detail 只是补充信息，拿不到也能测 */
  }

  // 协议层快照：这个文件到底拉了多少分片、多少字节、单次读盘最慢多久
  const snap = async () => {
    try {
      return await invoke<any>("media_stats");
    } catch {
      return null;
    }
  };
  const before = await snap();

  const v = document.createElement("video");
  v.muted = true;
  v.preload = "auto";
  v.style.cssText = "position:absolute;left:-9999px;top:0;width:2px;height:2px;opacity:0";
  document.body.appendChild(v);

  const t0 = performance.now();
  const marks: Record<string, number> = {};
  for (const n of [
    "loadstart",
    "loadedmetadata",
    "loadeddata",
    "canplay",
    "canplaythrough",
    "stalled",
    "suspend",
    "error",
    "abort",
  ]) {
    v.addEventListener(n, () => {
      if (marks[n] === undefined) marks[n] = Math.round(performance.now() - t0);
    });
  }

  const watch = startStallWatch();
  v.src = url;

  const outcome = await new Promise<string>((resolve) => {
    const timer = window.setTimeout(() => resolve("timeout"), timeoutMs);
    v.addEventListener(
      "canplay",
      () => {
        window.clearTimeout(timer);
        resolve("canplay");
      },
      { once: true },
    );
    v.addEventListener(
      "error",
      () => {
        window.clearTimeout(timer);
        resolve("error");
      },
      { once: true },
    );
  });

  // "卡死"的另一种表现是 play() 永远不 resolve —— 这里也量一下
  let playError: string | null = null;
  let playedTo = 0;
  if (outcome === "canplay") {
    try {
      await v.play();
      await new Promise((r) => window.setTimeout(r, 400));
      playedTo = v.currentTime;
      v.pause();
    } catch (e) {
      playError = String(e);
    }
  }

  const err = v.error;
  const after = await snap();
  const out = {
    id,
    kind: assets[id]?.kind ?? null,
    videoPath: detail?.video_path ?? null,
    videoBytes: detail?.video_size ?? null,
    stillBytes: detail?.still_size ?? null,
    w: v.videoWidth,
    h: v.videoHeight,
    elementDurationMs: v.duration ? Math.round(v.duration * 1000) : null,
    metaDurationMs: detail?.video_duration_ms ?? null,
    outcome,
    marks,
    readyState: v.readyState,
    networkState: v.networkState,
    playedTo: Number(playedTo.toFixed(3)),
    playError,
    error: err ? { code: err.code, message: err.message } : null,
    totalMs: Math.round(performance.now() - t0),
    mainThreadMaxStallMs: watch.max,
    mainThreadStalls: watch.count,
    stallSamples: watch.samples,
    protocol: before && after ? {
      requests: after.requests - before.requests,
      bytes: after.bytes - before.bytes,
      readMsMax: after.readMsMax,
      capped: after.capped - before.capped,
      chunkLimit: after.chunkLimit,
      // 媒体请求到达时缩略图队列里压着多少个 —— 用来证明"播放不会排在缩略图后面"
      behindThumbMax: after.behindThumbMax,
    } : null,
  };

  window.clearInterval(watch.timer);
  v.pause();
  v.removeAttribute("src");
  v.load();
  v.remove();
  return out;
}

async function videoSweepSelfTest(limit: number, timeoutMs: number) {
  const cands: number[] = [];
  for (let i = 0; i < assets.length; i++) if (assets[i].kind !== "still") cands.push(i);
  if (!cands.length) {
    diag.videoSweep = { error: "库里没有视频资产" };
    void flushDiag("video-sweep");
    return;
  }

  // 先问一遍字节数：卡死与"一次读多大"高度相关，所以**从最大的开始测**。
  const sized: Array<{ id: number; bytes: number; kind: string }> = [];
  for (const id of cands) {
    let bytes = 0;
    try {
      const d = await invoke<any>("asset_detail", { id });
      bytes = d?.video_size ?? 0;
    } catch {
      /* 忽略 */
    }
    sized.push({ id, bytes, kind: assets[id].kind });
  }
  sized.sort((a, b) => b.bytes - a.bytes);

  const half = Math.max(1, Math.floor(limit / 2));
  const picked: Array<{ id: number; bytes: number; kind: string }> = sized.slice(0, half);
  const rest = sized.slice(half);
  const stride = Math.max(1, Math.floor(rest.length / Math.max(1, limit - picked.length)));
  for (let i = 0; i < rest.length && picked.length < limit; i += stride) picked.push(rest[i]);

  const results: unknown[] = [];
  let worst: { id: number; stallMs: number } = { id: -1, stallMs: 0 };

  // **先落盘候选清单再开测**：万一第一个文件就把进程干掉（实测老代码真的会崩），
  // 至少能知道"凶手在名单里、按体积从大到小第一个"。
  diag.videoSweep = {
    limit,
    timeoutMs,
    total: assets.length,
    videoAssets: cands.length,
    tested: 0,
    pickedCount: picked.length,
    picked: picked.map((p) => ({ id: p.id, bytes: p.bytes, kind: p.kind })),
    results,
  };
  await flushDiag("video-sweep");

  for (let k = 0; k < picked.length; k++) {
    const p = picked[k];
    const r = await probeOneVideo(p.id, timeoutMs);
    results.push(r);
    if (r.mainThreadMaxStallMs > worst.stallMs) {
      worst = { id: p.id, stallMs: r.mainThreadMaxStallMs };
    }
    note(
      `sweep ${k + 1}/${picked.length} id=${p.id} ${p.bytes} B → ${r.outcome} ${r.totalMs}ms stall=${r.mainThreadMaxStallMs}ms ` +
        `req=${(r as any).protocol?.requests ?? "?"} bytes=${(r as any).protocol?.bytes ?? "?"}`,
    );
    // 每测一个就落盘：真卡死了，这一行就是证据
    diag.videoSweep = {
      limit,
      timeoutMs,
      total: assets.length,
      videoAssets: cands.length,
      tested: k + 1,
      pickedCount: picked.length,
      worstStall: worst,
      results,
    };
    await flushDiag("video-sweep");
  }

  const bad = results.filter((r: any) => r.outcome !== "canplay");
  diag.videoSweep = {
    limit,
    timeoutMs,
    total: assets.length,
    videoAssets: cands.length,
    tested: picked.length,
    pickedCount: picked.length,
    worstStall: worst,
    badCount: bad.length,
    bad: bad.map((r: any) => ({
      id: r.id,
      bytes: r.videoBytes,
      outcome: r.outcome,
      error: r.error,
    })),
    results,
  };
  note(`videoSweep 完成 测试 ${picked.length} 个 失败 ${bad.length} 个 最差停顿 ${worst.stallMs}ms`);
  void flushDiag("video-sweep");
}

void (async () => {
  await initEvents();
  window.setInterval(() => {
    if (!statsEl.classList.contains("hidden")) void refreshStats();
  }, 1000);
  window.setInterval(() => void flushDiag("tick"), 4000);
  setStatus("就绪 · 只读浏览，不会移动或修改任何照片");
  note("boot");

  try {
    const initial = await invoke<string | null>("initial_folder");
    if (initial) {
      $<HTMLInputElement>("path").value = initial;
      await open(initial);
      diag.scrollTest = scrollSelfTest();
      const st = diag.scrollTest as { assignable: boolean; wheelHandlerWorks: boolean };
      note(`selfTest assignable=${st.assignable} wheelHandler=${st.wheelHandlerWorks}`);
      void flushDiag("selftest");
      viewerSelfTest();
      window.setTimeout(() => void mediaSelfTest(), 3200);
      window.setTimeout(() => liveSelfTest(), 9000);
      window.setTimeout(() => void screenSelfTest(), 15000);
      // 视频扫描：由环境变量 LIVEPHOTO_SWEEP 打开（默认关闭）
      try {
        const plan = await invoke<{ enabled: boolean; limit: number; timeoutMs: number }>(
          "probe_plan",
        );
        if (plan?.enabled) {
          window.setTimeout(() => void videoSweepSelfTest(plan.limit, plan.timeoutMs), 22000);
        }
      } catch {
        /* 命令不存在（旧版后端）就跳过 */
      }
    }
  } catch (e) {
    diag.lastError = `自动打开失败: ${e}`;
    setStatus(diag.lastError);
  }
})();
