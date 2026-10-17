# LivePhoto 浏览器

### Windows 上最好用的 iPhone Live Photo 浏览器

打开即可浏览，点按即可播放，声音保持原样。

Windows 自带的「照片」把 Live Photo 呈现为一张静态 HEIC；资源管理器把 `IMG_1234.HEIC`
与 `IMG_1234.MOV` 显示成两个互不关联的文件。Live Photo 的核心是按下瞬间的画面与声音。

本工具的目标只有一个：在 Windows 上正确、流畅、完整地还原这一体验。

---

## 功能

### 相册式浏览

- 选定文件夹即打开相册；退出后不留痕迹，原始文件保持不变，不做任何复制
- 17,159 个文件的目录扫描耗时 **257 ms**（66,769 文件/秒）
- 左侧目录树（按子树计数、点击过滤）、日期分组、右侧月份时间轴

索引阶段仅读取目录项，不打开文件内容。因此 iCloud / OneDrive 中数据尚未下载到本地的
占位文件不会触发下载，云端库同样可以立即浏览。

### Live Photo 完整回放

| 能力 | 实现方式 |
|---|---|
| **缩略图悬停预览** | 悬停 0.4 秒后以静音循环方式预览，移出即停止 |
| **点按播放** | 全屏界面点击 `LIVE ▶` 或按空格，以原声播放一次 |
| **切换自动播放** | 切换至 Live Photo 时自动播放，行为与 iOS 相册一致 |
| **播放链路** | 直接播放系统 HEVC 扩展解码的原始 MOV，不转码、不生成预览缓存 |
| **配对准确率** | 同目录同主文件名配对，真实库 311 对全部命中 |

### 性能（均为实测）

| 环节 | 实测数据 |
|---|---|
| 扫描 17,159 个文件 | **257 ms** |
| 已缓存缩略图读取 | **14,896 张/秒** |
| 网格滚动（5000 格、真实照片） | **58.5 fps**，p50 **16.7 ms**，满帧运行于 60 Hz vsync |
| 7832 个资产同时存在时的 DOM 节点 | **54 个** |
| 全屏切换（已预加载） | p50 **35 ms** |
| 打开 2.08 GB 的 4K 视频 | **274 ms** 起播，界面停顿 **0 ms**，内存峰值 **28 MB** |

以上性能来自四项设计选择：

1. **索引只读取目录元数据**，不打开文件内容，因此扫描速度与文件大小无关。
2. **仅解码当前可见区域**，滚出视野即取消任务。清空缓存后打开大目录，只生成 111 张缩略图
   （等于可见数量）即停止，其余 7800 多个资产不产生任何解码。
3. **多级图源**：网格层先行显示，全屏层完成后无缝替换；已预加载的图片直接显示清晰版本。
4. **缓存为纯派生数据**：位于 `%LOCALAPPDATA%\LivePhoto\cache`，上限 4 GB，按最旧优先淘汰；
   删除缓存仅影响下次缩略图生成耗时。

### 大文件播放

手机导出的视频常有数 GB。本机库中有一个 **2.08 GB / 4K / 325 秒** 的视频，实测
`canplay` **274 ms**、界面停顿 **0 ms**、宿主进程内存峰值 **28 MB**、协议实际传输 **28.6 MB**
—— 播放器开始播放所需的仅为文件头与起始样本。

16 个最大的视频（最大 2.08 GB）连续测试：**16/16 正常播放，0 失败，最差界面停顿 0 ms**。

实现方式：自定义协议按 **4 MB 分片**响应（HTTP Range）；读盘在独立 IO 线程池执行，
界面线程仅负责 UI；播放请求在 IO 池中优先于缩略图请求。

### 快捷键

| 操作 | 作用 |
|---|---|
| 双击缩略图 | 打开全屏 |
| `←` `→` | 全屏切换上一张 / 下一张 |
| **空格** | 播放 / 停止 Live Photo |
| **滚轮**（全屏内） | 以光标为锚点缩放（1× ~ 8×） |
| 拖动（放大后） | 平移；点击照片本身保持全屏，点击空白处或按 `Esc` 退出 |
| 双击（全屏内） | 缩放复位 |
| `Ctrl` + 滚轮 | 缩放网格缩略图（80 ~ 320 px） |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | 网格滚动 |
| 侧栏目录 / 时间轴 | 过滤目录 / 拖动跳转月份 |

---

## 下载与运行

前往 [Releases](../../releases) 下载 `LivePhotoBrowser.exe`，双击即可运行：

- 单文件 **4.6 MB**，免安装，无需 Node 或 Vite
- 启动后填入文件夹路径，或设置环境变量 `LIVEPHOTO_OPEN` 直接进入

```powershell
$env:LIVEPHOTO_OPEN="F:\DCIM"; .\LivePhotoBrowser.exe
```

### 环境要求

- Windows 10 1809 及以上 / Windows 11
- **WebView2 运行时**（Windows 11 已内置）
- **HEIC / HEVC 解码**：微软 *HEIF 图像扩展* 与 *HEVC 视频扩展*。多数品牌机已预装；
  安装后 HEIC 缩略图与 Live Photo 播放均可使用。

---

## 构建

```powershell
cd app; pnpm install; pnpm build                      # 前端 → app/dist
cd ..; cargo build --release -p app --features custom-protocol
# 产物：target/release/app.exe（约 4.6 MB）
```

> ⚠️ **`--features custom-protocol` 为必需项。**
> `tauri` 的 build.rs 用 `let dev = !custom_protocol;` 判断 dev/prod。
> 缺少该 feature 时，release 二进制会连接 `devUrl`（localhost:1420），窗口白屏且没有任何报错。
> 使用官方 CLI（`pnpm tauri build`）会自动带上。

开发模式（debug 加载 `devUrl`，需同时启动 Vite）：

```powershell
cd app; pnpm dev                       # 终端 1
$env:LIVEPHOTO_OPEN="F:\DCIM"; .\target\debug\app.exe   # 终端 2
```

### 构建产物体积（实测）

运行所需文件只有 `LivePhotoBrowser.exe`。`target/` 是编译器缓存，删除后程序照常运行，
下次构建重新生成。实测体积：

| 状态 | `target/` | 文件数 | 冷构建耗时 |
|---|---|---|---|
| 干净构建（快档 `LTO=false`，246 个 crate） | **1.77 GB** | 3,679 | **14 分 11 秒** |
| 删除 `*.pdb` 后（该文件仅调试器使用） | **1.52 GB** | 3,569 | — |
| 混用不同 profile 设置反复构建 | 3.94 GB | 8,421 | — |

第三行为真实踩坑记录：每次修改 `CARGO_PROFILE_RELEASE_LTO` / `OPT_LEVEL` 这类覆盖，
crate 的 hash 随之变化，同一批依赖被重新编译一份，旧副本留在 `target/deps` 中。
实测累积到 775 个 `.rlib`（干净构建为 360 个），多出 2.4 GB。统一使用 `Cargo.toml`
中的 profile，或在切换设置前执行 `cargo clean`，体积即可保持在干净构建水平。

### 图标

当前使用 Tauri 脚手架的默认图标（`app/src-tauri/icons/`，含 16–256 共 6 个尺寸）。

`python tools/gen-icon.py` 保留了一套自绘方案作为备选：4 个变体，各生成 **9 个尺寸**
（16–256）的 `icon.ico` 与各尺寸 png，切换只需 `--variant` 一个参数：

```powershell
python tools/gen-icon.py --variant badge    # 圆角方块 + 播放三角 + 实况照片记号
python tools/gen-icon.py --sheet icon-candidates.png   # 输出候选对比图
```

多尺寸版本为必需项：Windows 在任务栏、标题栏、Alt-Tab、资源管理器大图标处使用不同尺寸，
仅提供大图由系统缩放会导致小尺寸模糊。

修改图标后需更新 `app/src-tauri/tauri.conf.json` 的时间戳再构建：
`tauri-build` 的 `rerun-if-changed` 只登记了该配置文件，不包含图标文件
（详见 `docs/实施记录.md` 的 K18）。

---

## 技术栈与结构

**Rust 核心 + Tauri v2 + WebView2**。程序不打包 Chromium 与编解码器：
HEIC / HEVC 全部调用系统扩展，可执行文件因此仅有 4.6 MB。

```
core/                 # 核心层，不依赖 Tauri（bench 可秒级编译）
  files.rs            # 目录扫描：只读元数据 + 云占位识别
  pairing.rs          # Live Photo 三级配对
  movmeta.rs          # MOV box 解析：时长/编码/分辨率/still-image-time
  cache.rs            # 磁盘缓存 + LRU 淘汰
  thumbs.rs           # 缩略图工作池：常驻 STA 线程 + 优先级队列
  media/wic.rs        # WIC 图片解码
  media/mf.rs         # Media Foundation 视频抽帧
  src/bin/            # bench 与验收工具（全部性能结论可复跑）
app/                  # Tauri 应用：命令层 + 自定义协议 + 前端
docs/                 # 三份文档，分工明确
tools/                # 侦察/验证脚本 + 图标生成
```

## 文档

| 文档 | 内容 |
|---|---|
| [`docs/方案.md`](docs/方案.md) | 产品形态、交互验收清单、架构设计、里程碑 |
| [`docs/实施记录.md`](docs/实施记录.md) | **26 条决策记录**（背景、选项、决定、实测依据、后果）与 **18 条踩坑清单** |
| [`docs/M0-实测发现.md`](docs/M0-实测发现.md) | 全部性能数字的出处，每条均附复现命令 |

每项性能结论均附实测数据与复现方式。修改本项目时，建议从 `实施记录.md` 开始阅读。

复现「大视频可正常播放」结论：

```powershell
pwsh tools/video-sweep.ps1            # 当前实现：逐个大视频真实加载 + 界面停顿监测
pwsh tools/video-sweep.ps1 -Legacy    # 原实现（同步协议 + 不限分片）：内存会涨到 4 GB 以上
pwsh tools/boot-selftest.ps1 -Runs 4  # 首屏自检连跑 4 次，给出通过率
```

---

## 当前限制

1. **首次浏览大库时缩略图逐个生成**：HEIC 解码约 900 ms/张，系统解码器并行度很低
   （6 线程仅 1.4× 加速）。浏览过程不被阻塞，生成一次后永久命中缓存。
2. **全屏翻页上限约 2~4.7 张/秒**：翻页更快时，底部胶片条显示「正在准备」。
3. **日期取文件修改时间**，尚未读取 EXIF 拍摄时间。
4. **滚轮由前端接管**，因此没有原生惯性。
5. **只读浏览**，v1 不含编辑与导出功能。
6. **交付形式为绿色单文件**，安装包（MSI/NSIS）尚未提供。
7. **一次未能彻底归因的偶发问题**：首屏加载期间出现过 1 次「播放 6 秒未就绪」
   （同一文件在无并发时 274 ms 即就绪）。已把播放请求提升为 IO 池优先、读盘移出界面线程，
   连测 4 次全部通过；该次失败记录保留在 `docs/实施记录.md` 的 D24。

## License

[MIT](LICENSE)

本仓库不包含任何编解码器；HEIC / HEVC 解码全部调用系统扩展。
