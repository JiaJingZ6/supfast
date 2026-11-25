# supfast 架构与接手说明

面向需要快速接手本项目的人：讲清楚它由哪些部分组成、各自负责什么、彼此如何协作，以及哪些地方有“看起来奇怪但必须这样写”的约束。

- 项目：`supfast`，把 ASS / SSA 字幕按「画面是否变化」切段，只渲染必要的帧，产出 PGS（`.sup`）与 BDN（`.xml`）。
- 技术栈：Tauri 2 + Rust（后端全部逻辑）+ 单文件 HTML/CSS/JS（前端，无打包器、无 node_modules）。
- 平台：Windows（大量 Win32 调用，见第 8 节）。

---

## 1. 一张图看懂

```
┌───────────────────────────── ui/index.html ─────────────────────────────┐
│  单文件：<style> 设计令牌/布局 + <section> 五个 pane + <script> 全部逻辑   │
│                                                                          │
│  DOM 表单 ──readForm()──► 参数对象 ──invoke('api',{path,body})──┐          │
│     ▲                                                          │          │
│  applySettings() ◄── /api/boot、/api/settings 的返回            │          │
│     │                                                          ▼          │
│  渲染：时间轴条 / 段表 / 历史 / 环境 / 日志      ┌──────── Rust 进程 ─────┐ │
│     ▲                                          │ main.rs               │ │
│     └──── /api/status 轮询（380ms，since=seq）──│  route() 路由表        │ │
│                                                │  Shared 内存状态       │ │
│                                                │      │                │ │
│                                                │ engine.rs             │ │
│                                                │  analyze / convert    │ │
│                                                └──────┼────────────────┘ │
└───────────────────────────────────────────────────────┼──────────────────┘
                                                        │ 子进程（每段一次）
                                  avs2bdnxml.exe + csri\VSFilterMod.dll
                                                        │
                                        PNG / 段 XML / 段 SUP → 合并产物
```

产物：`<目录>\<字幕名>\<字幕名>.xml`、`<字幕名>.sup`、`<帧号>_<序号>.png`。

---

## 2. 目录总览

| 路径 | 角色 | 是否需要改 |
|---|---|---|
| [src/main.rs](file:///H:/DIY/rea_sup/tauri/src/main.rs) | 应用入口、IPC 路由、共享状态、持久化、CLI | 业务编排改这里 |
| [src/engine.rs](file:///H:/DIY/rea_sup/tauri/src/engine.rs) | 纯 Rust 引擎：解析 → 切段 → 并行渲染 → 重写 XML/SUP | 算法改这里 |
| [ui/index.html](file:///H:/DIY/rea_sup/tauri/ui/index.html) | 全部界面与前端逻辑（无构建步骤） | 界面/交互改这里 |
| [tauri.conf.json](file:///H:/DIY/rea_sup/tauri/tauri.conf.json) | 窗口尺寸、无边框、`frontendDist: ui`、`withGlobalTauri` | 少改 |
| [capabilities/default.json](file:///H:/DIY/rea_sup/tauri/capabilities/default.json) | 主窗口权限白名单（dialog、窗口控制） | 加新 API 时才改 |
| [build.rs](file:///H:/DIY/rea_sup/tauri/build.rs) | 只有 `tauri_build::build()`：图标/版本资源 | 基本不动 |
| [build.bat](file:///H:/DIY/rea_sup/tauri/build.bat) | 一键编译并组装 `dist\` | 换机器要改路径 |
| [pack-release.bat](file:///H:/DIY/rea_sup/tauri/pack-release.bat) | `dist\` + README → `release\*.zip` | 发版改版本号 |
| [.github/workflows/release.yml](file:///H:/DIY/rea_sup/tauri/.github/workflows/release.yml) | 打 `v*` tag 时把仓库里的 zip 发成 Release | 少改 |
| [src/verify.rs](file:///H:/DIY/rea_sup/tauri/src/verify.rs) | SUP 合规自检（零依赖，含 palette update / 多对象检查） | 加检查项时 |
| [src/pgsdec.rs](file:///H:/DIY/rea_sup/tauri/src/pgsdec.rs) | PGS 软件解码器：WDS 裁剪 / cropped / DTS 缓冲 / present_pts | 改解码语义时 |
| [src/baseline.rs](file:///H:/DIY/rea_sup/tauri/src/baseline.rs) | 对拍基准：内容轨 + 名义/有效双时间轴 + decode_risk、黄金帧清单、参考 PNG | 改判据时 |
| [src/pngutil.rs](file:///H:/DIY/rea_sup/tauri/src/pngutil.rs) | 极简 PNG 读写（8bit RGB/RGBA，flate2），基准与真机对照共用 | 改参考画面格式时 |
| [src/compare.rs](file:///H:/DIY/rea_sup/tauri/src/compare.rs) | 真机录屏对照：ffmpeg 抽帧 → bbox 裁剪 → 逐像素比对 + 差异热图 | 改真机判据时 |
| [src/superenc.rs](file:///H:/DIY/rea_sup/tauri/src/superenc.rs) | 可选 SUPer 外部调用 | 换后端时 |
| [tools/baseline.ps1](file:///H:/DIY/rea_sup/tauri/tools/baseline.ps1) | 一键跑四组样本的对拍基准，产出 JSON + Markdown 摘要 | 加样本时 |
| [tools/compare_recording.ps1](file:///H:/DIY/rea_sup/tauri/tools/compare_recording.ps1) | 真机录屏对照一键脚本，产出 JSON + 摘要 + 截图 | 加真机样本时 |
| [tools/gcc_shim.rs](file:///H:/DIY/rea_sup/tauri/tools/gcc_shim.rs) | 编译期工具链垫片（windres 需要 `gcc -E`） | 见 7.3 |
| [tools/winbox.rs](file:///H:/DIY/rea_sup/tauri/tools/winbox.rs) | 开发期从外部改窗口尺寸/最小化别的窗口，便于截图 | 调试用 |
| [make_icon.py](file:///H:/DIY/rea_sup/tauri/make_icon.py) | 零依赖生成 `icons/icon.ico` | 换图标时 |
| `gen/schemas/` | `tauri-build` 自动生成的 ACL/schema 清单 | 生成物，勿手改 |
| `dist/`、`release/`、`target/` | 构建产物 | 生成物，勿手改 |
| [docs/ui.png](file:///H:/DIY/rea_sup/tauri/docs/ui.png) | README 里的界面截图 | — |
| [README.md](file:///H:/DIY/rea_sup/tauri/README.md) | 功能、用法、工具链坑、发版流程 | 已有大量细节，建议先读 |

`dist\` 的运行时布局（发布包内容）：

```
dist\
├─ supfast.exe            主程序（release 为 windows 子系统，无控制台）
├─ WebView2Loader.dll     gnu 目标动态加载 WebView2，必须与 exe 同级
└─ mod\
   ├─ avs2bdnxml.exe      渲染引擎
   └─ csri\VSFilterMod.dll 字幕滤镜（\move \t \fad \k 等）
```

---

## 3. 应用入口与启动流程

入口在 [main.rs 的 main()](file:///H:/DIY/rea_sup/tauri/src/main.rs#L680-L800)，顺序很关键：

1. **CLI 分流**：`argv[1] == "--engine"` 时不启动 GUI，直接进 [`cli_engine()`](file:///H:/DIY/rea_sup/tauri/src/main.rs#L680-L960)（`analyze` / `convert` / `verify` / `pgs` / `baseline` / `compare` 六种模式）。release 版没有 stdout，所以结果写 `--json <file>`；GUI 子系统 exe 在一键脚本里要用 `Start-Process -Wait -PassThru` 明确等待。这是一条独立于界面的对拍/回归/真机对照通道。
2. **单实例闸门**：[`single::acquire()`](file:///H:/DIY/rea_sup/tauri/src/main.rs#L617-L678) 用命名互斥体 `supfast_single_instance_v1`。拿不到锁就 `EnumWindows` 找到标题为 `supfast` 的窗口并恢复前台，然后直接返回。原因：WebView2 用户数据目录独占，第二个进程会以 `HRESULT 0x800700AA` 失败，而 GUI 进程没有控制台，用户只看到“双击没反应”。
3. **Tauri Builder**：
   - 注册 `tauri_plugin_dialog`（文件/目录选择）。
   - `setup` 里构造 [`Shared`](file:///H:/DIY/rea_sup/tauri/src/main.rs#L36-L129) 并 `app.manage(...)`：状态文件固定为 `%APPDATA%\supfast-desktop\state.json`（沿用 Electron 版路径，历史可直接继承；取不到 APPDATA 才退回 Tauri 的 `app_data_dir`）。
   - 注册 `CloseRequested` 钩子：先置取消 token，再最多等 6 秒让转换线程收尾。否则线程随进程被杀，`convert()` 末尾的临时目录清理不执行，`%TEMP%` 会攒下几 GB PNG。
   - 用 `with_webview` + `SetAreDefaultContextMenusEnabled(false)` 关掉 WebView2 原生右键菜单（页面里再拦一层 `contextmenu` 只是兜底）。
   - 调试钩子 `SUPFAST_EVAL`：延迟 1.8s 在页面里执行一段 JS。
   - 按主显示器尺寸收缩窗口（1320×940 在 1280×720 屏上会出界）。
4. **`invoke_handler` 只注册一个命令 `api`**，然后 `run(generate_context!())`。

启动后前端 [`boot()`](file:///H:/DIY/rea_sup/tauri/ui/index.html#L1438-L1477) 调 `/api/boot` 拿设置/最近/历史/环境，再查一次 `/api/status`，如果后端有任务在跑（例如窗口重开）就继续轮询。

---

## 4. Rust 后端：main.rs

main.rs 的职责是“编排 + 状态”，不含渲染算法。可以分成四层：

### 4.1 共享状态 `Shared`

[`Shared`](file:///H:/DIY/rea_sup/tauri/src/main.rs#L19-L129) 是一个可克隆的句柄集合，全部 `Arc<Mutex<..>>`，由 Tauri 托管：

| 字段 | 内容 | 说明 |
|---|---|---|
| `settings` | `serde_json::Value` | 与 `DEFAULT_SETTINGS` 合并后的设置 |
| `recent` | `Vec<Value>` | 最近字幕，按规范化路径去重，最多 12 条 |
| `history` | `Vec<Value>` | 成功转换记录，最多 24 条（启动读取时截 40） |
| `job` | `Job` | 当前任务快照：running/stage/pct/done/total/pngs/result/error/plan/started/token |
| `log` | `Vec<Value>` | `{seq,line,kind}` 环形日志；超 3000 条从头部裁到 2000 |
| `seq` | `AtomicUsize` | 全局递增日志序号，前端增量拉取用 |
| `file` | `PathBuf` | `state.json` 路径 |

两个设计点值得注意：

- [`lock()`](file:///H:/DIY/rea_sup/tauri/src/main.rs#L47-L51) 对锁中毒做 `unwrap_or_else(|e| e.into_inner())`。直接 `unwrap` 会让一次 panic 之后**所有** IPC 调用都失败，界面表现为“点了全没反应”。
- `Job.token` 是**每次任务新建**的 `Arc<AtomicBool>`，取消只置当前这个。共用一个 bool 会出现“新任务重置标志，把旧线程复活”的竞态。

### 4.2 统一路由 `route()`

[`route()`](file:///H:/DIY/rea_sup/tauri/src/main.rs#L214-L500) 是后端唯一的业务入口，按 `path` 分发（SQL 式“一个命令走天下”，方便和 Electron 版共用前端）：

| 路由 | 作用 | 谁调 |
|---|---|---|
| `/api/boot` | 设置 + 最近 + 历史 + 环境自检 + 版本 + 调试开关 | `boot()` |
| `/api/settings` | 合并设置、落盘、重跑 `probe_env` | `saveSettings()` |
| `/api/state` | 只读返回 recent/history/settings | `refreshHistory()` |
| `/api/analyze` | 解析切段出计划；成功则记入 recent 并落盘 | `analyze()` |
| `/api/start` | 置 running、落盘、清日志、起后台线程跑 `convert` | `start()` |
| `/api/cancel` | 置当前任务 token | `cancel()` |
| `/api/status?since=N` | 任务快照 + seq > N 的增量日志 | `poll()` |
| `/api/forget` | 删一条历史 / 清空历史 / 删一条最近 | 历史页 |
| `/api/open` | 用 `explorer.exe` 打开/定位路径（绝对路径防 PATH 劫持） | 打开目录 |

错误一律 `Result::Err(String)`。

### 4.3 IPC 命令 `api`

```rust
#[tauri::command]
fn api(sh: State<'_, Shared>, path: String, body: Value) -> Result<Value, String> {
    route(&sh, &path, &body)
}
```

前端把 `{path, body}` 打成一个对象传进来。`path` 里可以带查询串（`/api/status?since=..`），`route()` 自己 `split('?')` 解析。

### 4.4 转换任务的线程模型

`/api/start` 做三件事，顺序有讲究：

1. **校验 + 占位在同一把锁内**：`if j.running { return Err }` 和 `*j = Job{running:true,...}` 必须原子完成，否则两个并发 `invoke` 都能通过检查，起两条线程往同一个 `outdir` 写，产物互相覆盖。
2. 合并设置、记 recent、落盘、`clear_log()`。
3. `std::thread::spawn` 跑 `engine::convert`，并构造一个 [`engine::Emit`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L709-L709) 闭包把引擎事件翻译成 `Shared` 里的进度/日志：

| 引擎事件 | 处理 |
|---|---|
| `{"t":"stage","name":"plan"/"render"/"write"}` | 改 `job.stage`；render 记 total 并把 pct 置 3，write 置 96 |
| `{"t":"seg","done","total","pngs"}` | 更新计数，`pct = 3 + 92*done/total`，每 50 段或最后一段打日志 |
| `{"t":"warn","msg"}` | 以 `err` 类型写日志 |

结束时写 `job.result` / `job.error`、插入 history（截 24）、`save()`。取消时看 token 判断是 `cancel` 还是 `error`。

注意：这里**没有使用 Tauri 的 event 系统**。进度是“后端写内存 → 前端定时拉”，好处是窗口重开、前端刷新都能无缝恢复；代价是 380ms 一次轮询。

### 4.5 CLI 与工具函数

- `cli_engine()`：六种模式 `analyze / convert / verify / pgs / baseline / compare`。
  前四种解析 `--ass/--out/--fps/--jobs/--json` 等，analyze 只输出摘要字段
  （`bands_len` / `timeline_len` 代替整条数组）；`baseline` 额外接
  `--ref/--cand/--xml/--tol/--color-tol/--ratio-tol/--frames/--png-dir/--max-png`，
  调 `baseline::run`；`compare` 接 `--frames/--recording/--ffmpeg/--offset-ms/
  --cmp-tol/--cmp-ratio-tol/--shrink/--min-alpha/--nominal/--max-frames/--shots-dir`，
  调 `compare::run`。`--frames` / `--png-dir` / `--json` 两个模式共用变量。
- `fmt_n`（千分位）、`file_name`、`remember`（按 `\`→`/` 小写去重）。

---

## 5. Rust 后端：engine.rs

engine.rs 是纯函数式引擎，不依赖 `tauri::*`，只依赖 `serde_json` 和 std，因此能被 CLI 直接复用、也方便和旧 JS 版对拍。文件头的注释写明了三条必须遵守的 `avs2bdnxml` 约束，接手前务必读。

### 5.1 模块分层

| 区块 | 主要符号 | 职责 |
|---|---|---|
| 常量 | `MIN_STATIC_FRAMES=2`、`MAX_SEGMENTS=20000`、`PGS_HZ=90000`、`DYNAMIC_TAGS` | 静态段至少 2 帧（1 帧会输出空事件）；段数上限；PGS 时间基；动态标签表 |
| 数据结构 | `Event{start,end,layer,style,text,dynamic}`、`Segment{start,end,dynamic}` | 事件与切段 |
| 编码/时间码 | `read_text`、`decode_ansi`、`parse_timecode`、`frames_to_tc`、`tc_to_frames` | UTF-8(BOM) → UTF-16LE → 系统 ANSI(GBK) 三级解码；ASS 时间码向上取整到帧 |
| ASS 解析 | [`parse_ass()`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L261-L333) | 读 `[Script Info]` 的 PlayResX/Y、Timer；按 `Format:` 动态定位 Dialogue 字段；`split_n` 保证 Text 里的逗号不被切开 |
| 切段 | [`build_segments()`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L357-L425) | 见 5.2 |
| AVS 生成 | [`avs_for_segment()`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L435-L468) | 每段一个脚本：`LoadPlugin` + `MaskSubMod` + `LanczosResize`，再按静态/动态 `Trim` |
| SUP 重定时 | [`sup_retime()`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L506-L665) | 改写 PGS 段头 PTS/DTS 为合规模板，见 5.4 |
| 路径 | `strip_verbatim`、`resolve_outdir` | 去掉 `\\?\` 前缀；统一 `<父目录>\<字幕名>\` 布局 |
| 工具定位 | `find_exe`、[`locate_exe()`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L838-L858) | 优先级：设置里的 mod_root → `SUPFAST_MOD` → exe 同级/`mod\` → 当前目录 → 两个硬编码开发路径 |
| 参数归一 | `pick_f64/pick_u32/pick_flag/pick_str`、`read_opts` | 前端传字符串或数字都能吃；fps 限制 1~240，jobs 限制 1~64，分辨率上限 8192 |
| 调度 | [`analyze()`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L864-L970)、[`convert()`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L1152-L1528) | 对外的两个入口 |
| 环境自检 | [`probe_env()`](file:///H:/DIY/rea_sup/tauri/src/engine.rs#L1538-L1589) | 检查 avs2bdnxml、VSFilterMod、AviSynth、WebView2、内置引擎五项 |

### 5.2 切段算法（核心）

`build_segments` 把时间轴离散成“所有事件的起点/终点”这些边界，用**扫描线 + 活跃集**求每个区间的可见事件集合：

1. 所有 `start/end` 排序去重成边界数组。
2. 事件按 `start` 排序；边界从左到右推进时增量维护 `active`，并剔除 `end <= 当前边界` 的。
3. `active` 为空 → 无字幕空档，跳过。
4. 只要有任一事件带动态标签，整段标为 `dynamic`。
5. 计算**签名**：排序后的 `(layer, style, text)` 列表；`last.end == a && last_sig == key` 才把两段合并。

这里的签名是关键：字幕常常“上一句结束 = 下一句开始”，如果只按 `dynamic` 判断，整串台词会被并成一段、只渲第一句，后面全部丢失。同理，编码若把中文都解成 `U+FFFD`，不同台词签名会碰撞——这就是 `read_text` 必须用 `MultiByteToWideChar` 真解码的原因。

复杂度是 O(n log n)，早期 O(n²) 写法在 2 万事件时会把 `analyze` 卡死。

### 5.3 并行渲染

`convert()` 的流水线：

| 阶段 | 动作 |
|---|---|
| 准备 | `parse_ass` → `build_segments`；`dry_run` 在这里直接返回；建**独占随机名**临时目录（防预置 junction 重定向） |
| 建工作区 | 每段一个 `w{idx}` 目录，写入 `seg.avs`；记录 `contig`（下一段是否从本段结尾开始，供 `sup_retime` 判断是否丢尾部清屏） |
| 串参数 | `-t/-l/-v/-f`，固定 `-m 3 -u 0 -n 0`，开关映射 `-e/-z/-p/-b/-a`，`-o seg.xml`，可选 `-o seg.sup`，最后 `seg.avs` |
| 并行渲染 | `jobs` 条线程共享 `Mutex<Vec<Job>>` 队列；每段最多 `SEG_ATTEMPTS=3` 次，单次 `SEG_TIMEOUT_SECS=120`；失败清半成品再重试；每段 `emit` 进度 |
| 收集 | 按 `seg.start` 排序；静态段只取 XML 里第 1 个事件；动态段用子 XML 的 `InTC/OTC` 反算真实帧号；PNG 复制成 `{帧号}_{序号}.png`；每段 `sup_retime` |
| 写 XML | 重新生成完整 BDN（Description/Format/Events），时间码用真实帧号 |
| 拼 SUP | 逐段 append 到 `<name>.sup`，不把整条 42MB 读进内存 |

`run_seg()` 与 `kill_tree()` 放在同一区块，负责超时/取消时杀**进程树**，以及给 `CreateProcess` 加 `CREATE_NO_WINDOW`。不加这个标志，Windows 会为每个子进程分配 conhost，整片从 102 秒拖到 270 秒且闪窗。

### 5.4 PGS 时间戳重映射（时序合规模板）

每段的 `seg.sup` 是在“从 0 开始”的局部时间轴上生成的，必须改回真实时间。2026-11 起
`sup_retime` 与 `src/pgs.rs` 用**同一套合规模板**（见 5.6 末段），默认拼段流不再
沿用 `avs2bdnxml` 的 `DTS=0` / PDS 哨兵 PTS 惯例：

| 段 | PTS | DTS |
|---|---|---|
| `PCS(0x16)` / `WDS(0x17)` / `ODS(0x15)` | 显示集呈现时刻 `ds_pts` | `base = ds_pts − lead` |
| `PDS(0x14)` / `END(0x80)` | `base` | `base`（规范强制 `dts == pts`） |

- PGS 段头固定 13 字节：`PG` + PTS(4) + DTS(4) + type(1) + size(2)，90 kHz。
- **呈现时刻取原始 PCS 的 PTS**：`avs2bdnxml` 把「段内帧偏移 × frame_ticks」直接写进
  PCS（0 / 198949 / 360360 … 与子 XML 的 `InTC` 同源），`start_ticks + raw_pts` 即全局
  呈现时刻。旧实现用 `real_start + PCS 序号` 推时间，但分段输出会去重、序号 ≠ 真实帧号，
  曾让部分显示集比 BDN `InTC` 早 **1.21 秒**。静态段仍渲 2 帧、尾部清屏落在
  `real_frames` 对应的 `end_ticks`。
- **lead 按分辨率算**：`wipe = 90000 · width · height / Rc`（Rc = 32e6 px/s），
  `lead = wipe + frame_ticks + 1`，1080p/23.976 为 `5832 + 3754 + 1 = 9587` ticks，
  落在 `(wipe, 2·wipe]` 内且 < 1 秒：既满足 Epoch Start 的 `PTS−DTS > wipe`，也不踩
  WDS 的 `2·wipe` 红线。旧实现取「清屏 + 整屏解码 + 合成」最坏上界 21250 ticks，
  反而越线。
- **跨段单调化**：`floor` 记住上一段最后一个 PCS 的 PTS，新段首个显示集必须满足
  `first − lead > floor`（新 epoch 的 DTS 要晚于上一 epoch 最后呈现），否则整段推到
  `floor + lead + 1`（保住段内相对间隔）；其余段边界空档远大于 lead，保持 `InTC` 原值。
  段内 PCS 严格递增，同刻「显示 + 清屏」推后一帧，消掉 `PTS_DUP` 与 `wds_gap > pts_delta`
  在 gap=0 处的失败。这条硬约束是源头 PTS 仍有约 0.195 s 残留平移的原因（旧实现 1.21 s）。
- 静态段只渲了 2 帧，尾部会带一个清屏 epoch；若下一段紧接其后（`contig`），丢掉这个
  尾部 epoch，保证 PCS 数和原版一致。时间换算 `ticks = frame · 90000 / fps`。
- 写入时 `PDS` 的 `0xFFFF…` 哨兵 PTS 与 `DTS=0`、`END` 的 `DTS=0` 全部被覆盖；
  SUPer 会把哨兵读成负时间戳，令整条流的 DTS 序列崩掉。

比“整段重编码”省内存：只记录区间索引，再逐段拷贝并改写 8 个字节。实测 SUPer
`check_pts_dts_sanity` 对 test2 / drama 默认流都由 `False` 变为 `True`；`is_compliant`
对 test2 仍为 `(False, 0)`，但原因已定位为 `avs2bdnxml` 分段输出固有的
`Window change mid-epoch` / `Object-slot dimensions mismatch` 结构性错误，改前改后
日志行数同为 48，与时序无关（要消除需换用原生整流流）。

### 5.5 环境自检

`probe_env` 复用 `locate_exe`，另查 `System32/SysWOW64` 的 `avisynth.dll`，返回 `{mod_dir, items:[{key,name,ok,detail}]}`。前端“环境”页和状态栏芯片都吃这一个结构。

### 5.6 原生 PGS 后端（src/pgs.rs）

`RunOpts.native_pgs` / `palette_fade` / `object_split`（转换收尾的可选分支）与 CLI
`--engine pgs [--no-palette-fade] [--no-object-split]` 共用唯一入口
`pgs::encode(xml, out, vw, vh, fade, split)`。它不碰 L1/L2 渲染层，只吃主 BDN XML + 同目录的
8bit 索引 PNG，自己写 PCS/WDS/PDS/ODS/END 与 PGS RLE，输出 `<名>.native.sup`。
输入 PNG 已由 `avs2bdnxml -p 1` 量化好，所以不需要量化器；唯一第三方依赖是
纯 Rust 的 `flate2`（zlib 解压 IDAT）。

流水线：`parse_bdn` → `mono_times`（与 `super.xml` 同一套单调化）→ `group_epochs`
（按对象面积 3 MiB / 60 个 id / 2 秒空档切 epoch，且只允许在「显示集级空档 > lead」
处切）→ 逐 epoch `find_fade_chains` 规划调色板动画链 → `find_split_chains`
规划帧间拆分链 → 解码 PNG 规划普通显示集 → PTS 严格递增修正 → 段时序合规化
（踩线的 Acquisition 省略 WDS 并降级 Normal Case）→ 写段。对象复用表按 epoch
维护，key 是「调色板 + RLE」，命中就走 Normal Case 不重传 ODS。两类链互斥：
渐变只换调色板、更省，优先占用帧。

**调色板渐变（`FadeChain`）**：与 SUPer `PaletteSequenceEffect` 同理但零近似。
`find_fade_chains` 在同一 epoch 内找「单 Graphic、尺寸与 X/Y 相同、时间首尾相接」
的连续事件（≤512 帧），`png_rgba` 把每帧反量化成 RGBA，`build_fade_chain` 以
「每像素跨 N 帧的 RGBA 序列」为特征聚类：序列相同共用同一 palette entry，全透明
序列固定占 entry 0；序列种类 >255 就返回 `None`，**整条链回退逐帧 ODS**。
链首发 Epoch Start / Acquisition（WDS + PDS + ODS），后续每帧是
`ST_NORMAL` + `pal_upd=true` 的 palette update 显示集：PCS 写
`palette_update_flag=0x80`、不带 WDS/ODS、`palette_diff` 只发变化条目、
composition objects 与链首逐字节相同（SUPer verifier 硬约束）。PDS version
在 Epoch Start / Acquisition 重置为 0，其余逐次 +1。

**帧间对象拆分（`SplitChain`）**：SUPer 的 `ObjectDetector` 用 SSIM 阈值（约 0.986）
把「与累积合成图足够相似」的连续帧并进一个 object，只传一次 ODS —— 那是**有损
近似**；这里做零画质损失的等价版。`find_split_chains` 找「单 Graphic、尺寸与 X/Y
相同、时间首尾相接」（≤128 帧）的连续事件且避开渐变链；`best_split` 在宽/高的
1/4、1/3、1/2、2/3、3/4 共十条候选线里，按「唯一块数 × 段头 + 去重 RLE 字节
+ 每帧多一个 composition object」估算净收益，取 ≥64 B 且最大的一条。`split_rle`
把每帧索引矩阵切成两个矩形各自 RLE（不重新量化，拼回去逐像素一致），每块用
`object_key` 查 epoch 对象表，完全一致才复用 `object_id`，只重传变化的那一块。
`MAX_OBJ_IDS` / `OBJ_BUDGET` 预算不足或无净收益就整链回退普通单对象路径。

**关键规范坑**：拆分链后续帧必须 `ST_NORMAL`，不能是 `ST_ACQ`。SUPer verifier
在 acquisition 时执行 `ods_filled.clear()`，把复用旧对象判成 `Using expired memory`；
Normal Case 不清空对象缓冲、且允许同集定义新对象，才能合法复用链首的恒定半区。
改这里务必用 SUPer verifier 对拍。

**改这里前必须知道的几个坑**：

- `find_tag()` 必须排除 `<Events .../>` 与 `<Events>` 容器。BDN 的 Description 里
  `<Events FirstEventInTC=... LastEventOutTC=.../>` 会被朴素 `find("<Event")` 当成事件，
  `attr()` 又会把 `FirstEventInTC` 里的 `InTC` 当成属性名 —— 结果是第一个伪事件
  横跨全片，`mono_times` 把后面所有事件级联平移到片尾。**结构自检仍然 0 错 0 警，
  只有逐像素 + PTS 对照 XML 才能发现**。已有回归测试 `bdn_tag_boundaries`。
- PGS 的 RLE 把索引 0 当透明行程；`decode_png` 必须保证 `tRNS[0] == 0`，否则
  把某个全透明索引与 0 交换。
- epoch 内 WDS 字节必须恒定（否则 `WDS_CHANGED`）；Epoch Start / Acquisition 发同一份
  WDS，Normal / 清屏不发。唯一例外是「相邻 PTS 间隔 ≤ 窗口擦除时间」的 Acquisition：
  省略 WDS 并降级 `ST_NORMAL`（SUPer `test_diplayset` 规定非 Normal Case 必须带 WDS，
  `check_pts_dts_sanity` 又要求带 WDS 的显示集间隔 > pts_delta，逐帧大窗口只能这样解）。
- **段时序按 SUPer `check_pts_dts_sanity` 排布**（时序分歧已解决）：同一显示集所有段
  共用一个 DTS = `base = PTS − lead`；PCS/WDS/ODS 的 PTS 取呈现时刻，PDS/END 的 PTS
  也取 `base`（其 `PDS.__init__` / `END.__init__` 强制 `dts=pts`），于是段 DTS 序列
  全程非递减。`lead = wipe + 一帧`（1080p 为 9587），同时满足 Epoch Start 的
  `PTS−DTS > wipe` 与 WDS 的 `PTS−DTS <= 2·wipe`；阶段一按「清屏+解码+合成最坏上界」
  取的 21251 会踩 WDS 红线。**不要**把 PDS/END 的 DTS 写回 0 —— 2026-11 起
  `sup_retime` 也改用同一模板（见 5.4），两条工具链统一；`src/verify.rs` 仍保留对
  `dts=0` / PDS 哨兵 PTS 的豁免，只为兼容历史流与外部导入的旧 `avs2bdnxml` 产物。
- `group_epochs` 的切分空档必须用**显示集级**口径：上一 epoch 最后一个显示集的 PTS
  在有空档时是清屏显示集（= prev_out），无清屏时是事件显示集（= prev_in）。
  早期用 `in − prev_out` 会偏小，误把合法边界合并成超 64 id / 4 MiB 的过大 epoch。
- 同一显示集所有段共用 PTS/DTS，总字节必须 < RX 1 MiB；超了直接报错，不产出坏流。
- palette update 显示集只能 Normal Case、不能带 WDS/ODS、必须带 PDS 且 `palette_id`
  匹配、composition objects 非空且与前一画面逐字节相同，否则 SUPer `test_diplayset` /
  `is_compliant` 会判不合规。`src/verify.rs` 的 3c 段把这七项都查了。
- 序列种类超过 255 时**不要**做近似聚类，`build_fade_chain` 返回 `None`、整链回退
  逐帧 ODS 才能保证零画质损失；`FADE_MAX_SEQ_BYTES`(32 MiB) / `FADE_MAX_FRAMES`(512)
  是内存护栏。
- PGS 要求 palette entry `0xFF` 全透明（SUPer `is_compliant` 第 292 行亦如此判）。
  writer 写 PDS 前用 `palette_has_opaque_ff` 兜底拦截，避免 256 色 PNG 产出坏流。
- 「有新 ODS 又要复用旧对象」只能用 Normal Case，见上文 SplitChain。改成
  Acquisition Point 会被 SUPer 判 expired memory；它还有个 `for w in seg.windows`
  变量遮蔽 bug，会顺带抛 `TypeError: int + WindowDefinition`。
- `uniq_stats` 必须逐字节精确比较，不能换成哈希判等：误判「块相同」会复用错位图，
  直接导致画面错误。`object_key` 含 PLTE/tRNS 也是同理 —— 保证被复用对象的索引
  仍能被当前帧的 PDS 正确解释。

验收：`cargo test --release --bin supfast pgs::tests`（12 项：RLE 往返、标签边界、
时间码、颜色范围、渐变链无损 / 超容量回退 / PDS 差分、切块无损、分割线选择、
无收益回退、唯一统计精确性、`0xFF` 保护）+ `--engine pgs` 对 test2 / drama 跑
`verify` 0 错 0 警（1,228,073 B / 42,168,260 B）+ 8 帧合成淡入样本（渐变开/关各
12,288 采样 0 误差，910 B / 3,101 B）+ 8 帧 320×64 局部变化合成样本（拆分开/关
各 163,840 采样 0 误差，104,332 B / 175,957 B）+ 四个样本全部通过 SUPer verifier
的 `is_compliant=(True, 0)` 与 `check_pts_dts_sanity=True`。合规自检只能验结构，
验不了时间轴语义；两套 verifier 对拍 + 逐像素解码比对缺一不可。

### 5.7 对拍基准（src/pgsdec.rs / src/baseline.rs）

真机硬解验证没法自动化，先把「播放器模型」下的等价性做成可复现的软基准（第二版
补上硬解缓冲模型），再把第三角做成可复现的录屏对照命令（见 5.8）。

三角关系：源 PNG（BDN ground truth）→ SUP 解码画面（本工具）→ 真机播放输出。
`baseline` 覆盖前两角，`compare` 覆盖第三角。

解码器模型（第二版）：

- 拆段 → PCS…END 显示集；Epoch Start 清对象 / 调色板 / 窗口 / 画面构成，
  Acquisition / Normal Case 保留 epoch 内对象（否则无法复用 `object_id`）；
- ODS 首 / 中 / 末片拼接后 RLE 解码；PDS 按 `(palette_id, entry_id)` 累积，
  因此差分 palette update 只需携带变化条目；
- 每个 END 提交一帧；composition objects 为空就是清屏帧；
- 画面只保留 `alpha > 0` 的像素，按 `(y*stride+x)` 排序，hash 为 FNV-1a 64。
  透明色的 RGB 不参与比较；
- **WDS 窗口裁剪**：cobj 引用的窗口有效（w/h>0）时，落在窗口矩形外的像素不显示；
  没有对应 WDS 或窗口为空时保持不裁剪。窗口定义按 id 覆盖更新；
- **cropped 对象**：PCS cobj 第 4 字节 bit7 = `object_cropped_flag`，带 8 字节
  `(cx,cy,cw,ch)` 时从对象内部取子矩形贴到 `(x,y)`；cobj 变长解析，bit6 是 forced；
- **DTS 缓冲 / PTS 呈现**：`need = clear(90000·plane/Rc) + decode(90000·obj_px/Rd)
  + compose(90000·plane/Rc)`，`plane` 取 WDS 窗口面积（无窗口则整屏）；
  `dts` 取 PCS 段头，为 0 时按 `pts - need` 兜底（不凭空造 late）；
  `present_pts = max(pts, dts + need)`，`late = present_pts - pts`；
- **buffer 峰值**：按 `(dts, +bytes)` / `(pts, -bytes)` 事件排序列出 RX buffer 峰值
  （上限 1 MiB）；epoch 内累积对象像素数（上限 4 MiB）；DTS 非单调单独计数。

双轨对拍 + 硬解轨：

| 轨 | 做法 | 回答 |
|---|---|---|
| 内容 | 可见帧按序号对齐，hash 不同的帧二次解码取像素逐通道比对 | 每张图有没有被编对 |
| 时间轴（名义） | 两流所有 PTS 的并集上采样，取各自「最后一个 `pts ≤ t`」的帧比 hash | 同一时刻挂着哪张画面 |
| 时间轴（有效） | 同上但改用 `present_pts`（late 帧会被推后） | 硬解真正来得及呈现的画面 |
| 解码可行性 | 任一流 `late_display_sets > 0` 或 RX / object buffer 超限 → `decode_risk` | 名义时间轴还能不能信 |

时间轴轨把 hash 不同再分三类：`presence`、`color_only`、`frame_mismatch`。只有真实
错位 / 存在性差异 / 按序号的 PTS 平移才计入「时间轴不一致」。综合判据优先级：
`divergent` > `decode_risk` > `content_ok_time_shift` > `equivalent` > `identical`。
二次解码只对 hash 不同的帧、按 256 帧分块，内存 O(一帧)；有效轨用按
`(present_pts, 帧号)` 预排序的索引二分，避免 O(n²)。

`--xml` 另做源头对拍：复用 `pgs.rs` 的 `parse_bdn` / `decode_png` / `palette_entries`，
把每个事件的源 PNG 组合成期望画面，按事件序号与各流的可见帧对齐。源头对拍按序号，
PTS 差只作参考 —— 默认拼段流的呈现时刻由 `sup_retime` 取分段原始 PCS PTS 重映射
（见 5.4），已贴近 BDN `InTC`，但受「新 epoch 的 DTS 必须晚于上一 epoch 最后呈现」
约束仍会有约 0.2 s 以内的残留平移，不是完全同一基准。

### 5.8 真机录屏对照（src/compare.rs / src/pngutil.rs）

`--engine compare` 读 `baseline` 导出的 `*.frames.json`（含 `png_dir` / `present_pts`
/ `bbox` / `png`），对每帧用 ffmpeg 在 `present_pts - offset_ms` 抽一帧、缩放到源
视频尺寸，裁到 bbox 与参考 RGBA PNG 逐像素比 RGB。`pngutil.rs` 是零第三方图像库的
8bit RGB/RGBA PNG 读写（含 filter 0..4 解码），基准导出与录屏对照共用。

判据 `identical` / `equivalent`（超容差占比 ≤ `--cmp-ratio-tol`）/ `divergent` /
`error`（ffmpeg 抽不到帧）；默认 `--cmp-tol 12` / 0.02 / `--shrink 1` /
`--min-alpha 200` 用来吸收播放器 YCbCr→RGB、缩放与抗锯齿噪声。每帧导出
`*.shot.png`（录屏裁图）与 `*.diff.png`（差异热图），一键脚本
`tools\compare_recording.bat` → `compare_recording.ps1` 落 `tools\compare-results\`。
`--nominal` 可切回名义 PTS 抽帧，用来区分时间轴标注口径与硬解呈现两类差异。

入口与产物：`main.rs` 的 `--engine baseline` / `--engine compare`；一键脚本
`tools\baseline.bat` → `baseline.ps1` 与 `tools\compare_recording.bat` →
`compare_recording.ps1`。结果分别落 `tools\baseline-results/` 与
`tools\compare-results/`。实测数据与判据见 README。

---

## 6. 前端：ui/index.html

单文件、零构建：`<style>`（设计令牌 + 布局）、`<section class="pane">` 五个页面、`<script>` 全部逻辑。因为 `tauri.conf.json` 里 `withGlobalTauri: true`，所以可以直接用 `window.__TAURI__`，不需要 npm 包或 `@tauri-apps/api`。

### 6.1 页面结构

| pane | 内容 |
|---|---|
| `convert` | 文件行、最近 chips、fps/vfmt/lang/title/jobs、切段时间轴、预设、输出目录、高级选项（分辨率覆盖、mod 目录、7 个开关） |
| `timeline` | 大时间轴 + 四个统计 + 段列表（前 240 段） |
| `history` | 历史表格，点行打开输出目录，可单删/清空 |
| `env` | 五项自检 + 原理说明 |
| `log` | 百分比、进度条、开始/取消/打开目录、实时日志 |

无边框窗口：标题栏 `<div class="titlebar" data-tauri-drag-region>`，右侧三个按钮走 `getCurrentWindow().minimize/toggleMaximize/close`。

### 6.2 与后端的三种通道

1. **命令（invoke）**：
   ```js
   const T = window.__TAURI__;
   const rpc = (path, body) => T.core.invoke('api', { path, body: body || {} });
   ```
   所有业务都走这一个 `api` 命令，和 main.rs 的 `route()` 一一对应。
2. **轮询**：[`poll()`](file:///H:/DIY/rea_sup/tauri/ui/index.html#L1280-L1311) 每 380ms 调 `/api/status?since=S.seq`，把增量日志 append 到 `<pre id="log">`，更新进度/阶段/统计；任务结束按 `stage` 弹 toast 并刷新历史。`S.seq` 只在拿到 `log_seq` 时前移。
3. **系统能力**：
   - 文件/目录选择：`T.dialog.open({...})`（`dialog:default` 权限）。
   - 拖放：`T.webview.getCurrentWebview().onDragDropEvent`。WebView 会吞掉 HTML5 拖放，拿不到真实路径，必须用 Tauri 的；只收 `.ass/.ssa`。

### 6.3 前端状态管理

没有框架，只有一个模块级对象 [`S`](file:///H:/DIY/rea_sup/tauri/ui/index.html#L902-L906) 和一套“表单即状态”的约定：

```
DOM 表单  ──readForm()──►  {ass, fps, vfmt, lang, title, jobs, outdir, mod_root, width, height, 7 个开关, theme}
                              │
                              ├─► saveSettings()（300ms 防抖）──► /api/settings ──► Shared.settings ──► state.json
                              ├─► analyze()  ──► /api/analyze ──► renderInfo/paintTimeline/renderSegTable
                              └─► start()    ──► /api/start   ──► 后台线程 + poll()
```

- `applySettings(st)` 是反向：后端设置 → 填表单。`outdir` 故意不回填（每次启动都回到默认落点）。
- 保存时 `delete o.outdir; delete o.ass`：这两个是“这一次的形式状态”，不写进 `state.json`，否则上次路径会一直黏在界面上。
- 最近文件只显示前 5 条（后端存 12），历史最多 24 条。
- 主题三态 `auto/light/dark` 存 `settings.theme`；首帧前用一个小内联脚本读 `location.hash` 的 `#t=dark` 防白闪（Rust 端目前没有注入 hash，实际主题在 `boot()` 后由 `applyTheme` 设定）。

### 6.4 渲染函数

`renderInfo`（文件行/统计/耗时估算/警告）、`paintTimeline`（`bands` 数组 → 百分比定位的色块，静态灰、动态橙）、`setTicks`、`renderSegTable`、`renderOutHint`（显示实际落点）、`renderHistory`、`renderEnv`、`renderRail`、`pushLog`、`setStage`、`setPct`。全部是“拿到后端 JSON → 重写 innerHTML”，没有虚拟 DOM。

---

## 7. 配置、构建与打包

### 7.1 tauri.conf.json

```json
{
  "productName": "supfast",
  "version": "1.0.1",
  "identifier": "com.supfast.desktop",
  "build": { "frontendDist": "ui" },
  "app": {
    "withGlobalTauri": true,
    "windows": [{ "title": "supfast", "width": 1320, "height": 940,
                  "minWidth": 1024, "minHeight": 680,
                  "decorations": false, "resizable": true, "center": true }],
    "security": { "csp": null }
  },
  "bundle": { "active": false, "targets": "all" }
}
```

要点：
- `frontendDist: "ui"`：没有 dev server、没有 `beforeDevCommand`，`ui/` 目录直接内嵌进二进制，改完 HTML 重新编译即可。
- `withGlobalTauri`：`window.__TAURI__` 的来源。
- `decorations: false`：自绘标题栏，拖动依赖 `data-tauri-drag-region`。
- `bundle.active: false`：不做 msi/nsis 安装包，发布物是手工组装的绿色 zip。
- `csp: null`（宿主侧关闭），页面自己用 `<meta http-equiv="Content-Security-Policy">` 限制：`default-src 'self'`、`connect-src 'self' ipc: http://ipc.localhost`。

### 7.2 capabilities/default.json

Tauri 2 的权限白名单，作用于 `main` 窗口：

- `core:default`：事件、路径、webview 基础能力。
- `dialog:default`：`pickFile` / `pickDir` 的 `open` 对话框。
- `core:window:allow-*`：`start-dragging`、`minimize`、`maximize/unmaximize/toggle-maximize`、`close`、`set-focus`、`center`、`unminimize`、`is-minimized/is-maximized`。

这些写操作不授权时 Promise 会直接拒绝。早期前端没写 `.catch`，表现就是“点了完全没反应也看不到原因”，现在统一用 `winAct()` 包一层 toast。

`gen/schemas/` 是 `tauri-build` 根据 conf + capabilities 生成的校验清单，不要手改。

### 7.3 build.rs + 工具链

[build.rs](file:///H:/DIY/rea_sup/tauri/build.rs) 只有 `tauri_build::build()`，负责把图标、版本资源塞进 exe。它隐含两个前置条件：

- `icons/icon.ico` 必须存在（[make_icon.py](file:///H:/DIY/rea_sup/tauri/make_icon.py) 零依赖手写 ICO）。
- `tauri-winres` 编译 `.rc` 时固定调 `gcc -E`，而便携工具链的 gcc 只是链接器壳子 → 用 [tools/gcc_shim.rs](file:///H:/DIY/rea_sup/tauri/tools/gcc_shim.rs) 编出 `gcc.exe` 放进 self-contained，只支持 `-E`，把 `#pragma`/`#include` 行滤掉原样输出。
- 另一坑：`dlltool.exe` 生成导入库时要调同目录的 `as.exe`，需从 msys2 binutils 取一份。

`tools/gcc.exe`、`tools/winbox.exe` 都是这些 `.rs` 编出来的现成二进制，源码保留是为了换机器能重建。

### 7.4 build.bat

[build.bat](file:///H:/DIY/rea_sup/tauri/build.bat) 步骤：

1. 设 `TC=H:\DIY\rea_sup\rust-toolchain\root`（**硬编码，换机器必改**），`SC=...\x86_64-pc-windows-gnu\bin\self-contained`。
2. 检查 `as.exe`、`gcc.exe`、`icon.ico`；缺图标就 `python make_icon.py`。
3. `PATH = TC\bin; SC; ...` 后 `cargo build --release`。
4. 复制 `target\release\supfast.exe` + `WebView2Loader.dll` 到 `dist\`；从 `..\mod` 或 `H:\DIY\mod` 复制 `avs2bdnxml.exe` 与 `csri\`。
5. 冒烟测试：`dist\supfast.exe --engine analyze --ass ..\test\test2.ass --json %TEMP%\...` 并打印结果。

### 7.5 pack-release.bat

[pack-release.bat](file:///H:/DIY/rea_sup/tauri/pack-release.bat)：

1. `VER=1.0.1`（发版改这里）→ 临时 staging 目录。
2. `xcopy dist\*` + `packaging\release-readme.txt` → `README.txt`。
3. **防呆**：staging 里若出现 `state.json` 直接失败（用户状态绝不能进包）。
4. `Compress-Archive` → `release\supfast-<ver>-win64.zip`。

因为用户数据在 `%APPDATA%\supfast-desktop\state.json`，包天然是“全新默认状态”。想模拟新用户：`set APPDATA=%TEMP%\supfast-fresh` 再启动。

### 7.6 发布流水线

[.github/workflows/release.yml](file:///H:/DIY/rea_sup/tauri/.github/workflows/release.yml)：push `v*` tag（或手动 `workflow_dispatch` 填 tag）→ 校验 `packaging/artifacts/` **正好一个** zip → 有 `packaging/release-notes-<ver>.md` 就用它当正文，否则自动生成 → `gh release upload --clobber`。

`mod\` 的二进制在 runner 上编不出来，所以 zip 随仓库走（每个版本一份，记得删旧的）。当前仓库里是 `packaging\artifacts\supfast-1.0.1-win64.zip`。

---

## 8. 必须知道的平台约束（改动前先看）

1. **`CREATE_NO_WINDOW`**：GUI 进程 spawn 控制台程序必须加，否则每段一个 conhost，性能腰斩 + 闪窗。
2. **每段独立调用 avs2bdnxml**：它会和**前一帧**比较去重，把多个静态段拼在一条时间轴上，第 2 段起会被判成“没变化”而整段丢失。每段还必须有自己的 cwd，因为 PNG 写到当前工作目录。
3. **静态段至少 2 帧**：`Trim(k,k)` 只有 1 帧时输出空事件；`MIN_STATIC_FRAMES=2` + `.Loop()`。
4. **`\\?\` 前缀**：`std::fs::canonicalize` 在 Windows 返回 verbatim 路径，AviSynth / VSFilterMod 不认，写进 AVS 会静默失败。所有进 AVS 的路径都要过 `strip_verbatim`。
5. **ASS 编码**：中文 GBK 必须真解码，否则签名碰撞、整串漏字幕（见 5.2）。
6. **单实例**：WebView2 用户数据目录独占，多开必炸，所以有互斥体。
7. **关窗等待**：不等待会留下几 GB `%TEMP%\supfast_*`。
8. **`gen/schemas`、`dist`、`release`、`target`** 都是生成物。

---

## 9. 一次转换的完整数据流

```
用户拖入 .ass
  └─ onDragDropEvent → selectFile(path)
       └─ analyze() → invoke api /api/analyze
            ├─ main::route → engine::analyze
            │    ├─ read_text（编码探测）→ parse_ass
            │    ├─ build_segments → 统计 sta/dyn/work/ratio
            │    └─ resolve_outdir + 估算耗时 + warnings/notes
            ├─ remember(recent) + save(state.json)
            └─ 返回 {events,frames,segs,work,ratio,timeline,bands,outdir,...}
       └─ renderInfo / paintTimeline / renderSegTable 更新 UI

点「开始转换」
  └─ start() → invoke api /api/start {opts}
       ├─ 锁内检查 running 并占位（新建 cancel token）
       ├─ 合并设置 / 记 recent / 落盘 / clear_log
       └─ spawn 线程 → engine::convert(opts, emit, token)
            ├─ plan：解析切段、建独占临时目录、每段写 seg.avs
            ├─ render：N 线程队列 → run_seg(avs2bdnxml, 独立 cwd, 超时/重试)
            │            └─ emit seg → main 更新 job.pct/log
            ├─ 收集：PNG 复制、sup_retime 重映射 PTS
            ├─ write：重写 BDN XML + 逐段 append SUP
            └─ 返回 {pngs,secs,speed,sup_size,failed,...}
  └─ poll() 每 380ms → /api/status?since=seq
       ├─ 增量日志 append
       ├─ stage=done → toast + refreshHistory()
       └─ stage=error/cancel → toast
```

---

## 10. 二次开发入口速查

| 想做的事 | 改哪里 |
|---|---|
| 加一个新参数/开关 | `ui` 表单 + `readForm` → `read_opts`/`RunOpts` → `avs_for_segment` 或 `args` 拼装 → `DEFAULT_SETTINGS` |
| 改切段规则 | `engine::build_segments` + `is_dynamic`（注意签名语义） |
| 改产物布局 | `engine::resolve_outdir`（前端只展示后端算出的 `outdir`） |
| 改 SUP 编码方式 | `src/pgs.rs`（原生整流 / 调色板渐变）或 `src/superenc.rs`（外部 SUPer）；两者都只吃 XML + PNG，产物并列不覆盖默认 `.sup` |
| 跑真机验证前的对拍基准 | `tools\baseline.bat` → `tools\baseline.ps1` → `--engine baseline`；改判据看 `src/baseline.rs`，改解码语义看 `src/pgsdec.rs` |
| 拿录屏核对真机输出 | `tools\compare_recording.bat -Frames <frames.json> -Recording <rec.mp4> -OffsetMs <ms>` → `--engine compare`；改判据看 `src/compare.rs` |
| 加一个 IPC 接口 | `main::route` 分支 + 前端 `rpc('/api/xxx')`；窗口/对话框等系统能力还要改 `capabilities/default.json` |
| 改进度/日志 UI | `ui` 的 `setStage/setPct/pushLog` + `poll()`；后端事件在 `engine::Emit` 的 `t` 字段 |
| 换图标 | `make_icon.py` 后删 `icons/icon.ico` 重跑 `build.bat` |
| 排查“转换没产物” | 先看“环境”页五项；再用 `supfast.exe --engine analyze --ass ... --json out.json` 脱离 GUI 复现 |
| 排查布局 | `SUPFAST_PANE` / `SUPFAST_OPEN_ADV` / `SUPFAST_EVAL` + `tools\winbox.exe resize` |

---

## 11. 当前已知的小坑（接手时优先确认）

- `/api/*` 的错误通过 `Err(String)` 返回时会**拒绝 Promise**，而前端部分调用点（如 `analyze()`、`start()`）只判断 `r.ok`、没有 `try/catch`，失败时可能只在控制台留下 unhandled rejection。新增调用建议加 catch + toast。
- `engine::Job` 与 `main::Job` 同名但各自模块作用域内，阅读时注意区分。
- `find_exe` 与 `build.bat` 里有 `H:\DIY\...` 硬编码开发路径，发布/换机器时要么改，要么靠 `mod_root` 设置覆盖。
- 没有安装包、无自动更新；双击标题栏不会最大化（用右上角按钮）；右键菜单被全局关闭（输入框右键粘贴也不可用，`Ctrl+V` 正常）。
