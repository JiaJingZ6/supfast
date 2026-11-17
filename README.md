# supfast

把 ASS / SSA 字幕按「画面是否变化」切成段，只渲染必要的帧，输出 PGS（`.sup`）与 BDN（`.xml`）。

给压制组、字幕组和 DIY 蓝光用的：一条 1 小时 44 分的片子，只要 2.4% 的帧真的在动，
剩下 97.6% 的画面是静止的 —— 静止段只需要渲染段首那一帧。

## 它怎么工作

1. **解析** ASS：取每条 Dialogue 的起止时间、层号、样式、文字，换算成帧号。
2. **切段**：相邻事件的可见内容完全一致就并成一段；带 `\move` `\t()` `\fad` `\k` 这类动态标签的段
   标记为「动态」，其余是「静态」。
3. **渲染**：每段生成一个只含该时段的 AviSynth 脚本，交给 `avs2bdnxml` 出 PNG。
   静态段只渲首帧（尾部若有需要再补一帧），动态段逐帧。
4. **收尾**：把各段 XML 按真实时间码重写回一条，重映射 SUP 的 PTS，拼接成单个 `.sup`。

## 界面

五个标签页：

| 标签 | 内容 |
|---|---|
| **转换** | 文件行（拖入 `.ass` 或从「最近」选）、帧率 / VideoFormat / 语言码 / 轨道名 / 并行度、切段计划、输出目录、高级选项 |
| **切段计划** | 全片时间轴（灰＝静态段、橙＝动态段、空档＝无字幕），四个统计数字，段列表带「段条」 |
| **历史** | 每次成功转换一行：时间 / 字幕 / PNG 数 / SUP 体积 / 耗时 / 较原版加速比 |
| **环境** | `avs2bdnxml` / VSFilterMod / AviSynth / 内置引擎 五项自检 |
| **日志** | 百分比 + 进度条 + 实时日志，可取消、可打开输出目录 |

- **预设**一键套用输出规格：蓝光 23.976/1080p、PAL 高清 25/1080p、DVD 25/480p、电视 29.97/1080i。
- **高级选项**（折叠）里是分辨率覆盖、mod 目录和七个合成开关。

| 开关 | 传给 avs2bdnxml | 作用 |
|---|---|---|
| 8bit 调色板 PNG | `-p 1` | 输出 8 位索引色 PNG。PGS 每帧最多 256 色，提前量化省体积，写 SUP 时也不用再转 |
| 自动裁剪 | `-a 1` | 逐帧裁掉四周空白，偏移写进 XML 的 `<Graphic>` |
| PG 缓冲优化 | `-b 1` | 切图复用，降低 PGS 的 object buffer 占用（硬件播放器不易溢出） |
| 严格模式 -z | `-z 1` | SUP 写入阶段更严格的检查，削弱缓冲优化但提升兼容性，花屏时再开 |
| 强制偶数 Y | `-e 1` | Y 坐标取偶数，配合 4:2:0 色度采样，避免半像素错位 |
| 不生成 SUP | 不传第二个 `-o` | 只出 BDN XML + PNG，自己 mux 时用 |
| 只看计划 | — | 只解析、切段、算工作量，不调用 avs2bdnxml |

写死的三个参数：`-m 3`（切分后最短 3 帧）、`-u 0`（不允许丑陋切图）、`-n 0`（不允许空 XML）。

主题三态（跟随系统 / 浅色 / 深色），选择与设置、最近、历史一起存在
`%APPDATA%\supfast-desktop\state.json`。

## 输出

```
<输出目录>\
├─ <字幕名>.xml      BDN 字幕（PNG 序列的时序与坐标）
├─ <字幕名>.sup      PGS 流，可直接 mux 进 m2ts / mkv
└─ <帧号>.png        按需渲染的图形
```

整片实测（1831 事件 / 1832 段 / 1 小时 44 分）：**1839 张 PNG，102 秒**，8 路并发。
同一份字幕交给 `avs2bdnxml` 原版单跑要 484 秒；逐帧比对 1839 张全部一致
（3 个起始帧差 1 帧，原因是 ASS 时间码只有两位小数，取整会顶到下一帧）。

## 命令行（不进 GUI）

release 版是 windows 子系统、没有 stdout，所以结果写文件：

```bat
supfast.exe --engine analyze --ass test.ass --json out.json
supfast.exe --engine convert --ass test.ass --out out_dir --jobs 12 --json out.json
```

- `analyze` 输出 events / frames / segs / sta / dyn / work / ratio / w / h，以及 `bands`（全片时间轴）与 `timeline`。
  `warnings` 只放会影响结果的问题（ASS 的 Timer 与帧率不符、切段数超上限）；
  `notes` 是纯信息（全片静态、多少帧含动态标签），界面上记一次日志，不弹窗。
- `convert` 输出 pngs / secs / speed / epochs / failed / sup_size。

## 编译

需要 Rust（`x86_64-pc-windows-gnu` 目标）、Python（生成图标）和一份 `avs2bdnxml` + `VSFilterMod` 的 `mod\` 目录。
直接跑 `build.bat`：设 PATH → `cargo build --release` → 把 exe、`WebView2Loader.dll`、`mod\` 组装进 `dist\`。

发布目录 7.3 MB，绿色解压即用：

```
dist\
├─ supfast.exe            3.8 MB
├─ WebView2Loader.dll     157 KB   gnu 目标是动态加载 WebView2，必须带
└─ mod\                   3.4 MB   avs2bdnxml.exe + csri\VSFilterMod.dll
```

工程结构：

```
├─ Cargo.toml / Cargo.lock
├─ build.rs                    tauri-build（图标 / 版本资源）
├─ tauri.conf.json             窗口 1320×940、无边框、withGlobalTauri
├─ capabilities\default.json   主窗口权限（core:default、dialog、窗口控制）
├─ icons\icon.ico              4 根竖条，最后一根是强调橙（make_icon.py 生成）
├─ src\engine.rs               引擎：解析 → 切段 → 并行渲染 → 重写 XML / SUP
├─ src\main.rs                 IPC 路由 + 状态持久化 + 命令行入口
├─ ui\index.html               界面（单文件 HTML + CSS + JS）
├─ tools\gcc_shim.rs           windres 用的最小 gcc -E 垫片（见下）
├─ tools\winbox.rs             开发用：从外部改窗口尺寸，方便截图核对排版
└─ build.bat                   一键编译 + 组装 dist\
```

### 工具链的三个坑（已就地解决，换机器要重做）

| 症状 | 原因 | 处理 |
|---|---|---|
| `dlltool: program not found` | 便携工具链只有 `dlltool.exe`，而它生成导入库时要调 `<同目录>/as` | 从 msys2 的 `mingw-w64-x86_64-binutils` 包里取 `as.exe` 放进 self-contained |
| `windres: preprocessing failed` | `tauri-winres` 编译 `.rc` 时固定调 `gcc -E`，自带的 gcc 只是链接器壳子 | 用 `tools/gcc_shim.rs` 编出一个 `gcc.exe`（把 `#pragma` 行滤掉原样吐出）放进 self-contained |
| `icon.ico not found` | tauri-build 要往 exe 里塞图标资源 | `python make_icon.py` 生成（手写 BMP DIB + ICO 头，无第三方库） |

注意 `gcc.exe` 这个垫片**不能**改名成 `x86_64-w64-mingw32-gcc.exe` —— rustc 用那个名字做链接。

### 性能上的一个坑

`avs2bdnxml` 是控制台程序，而 GUI 进程没有控制台。如果不给 `CreateProcess` 加 `CREATE_NO_WINDOW`，
Windows 会为**每一段的每个子进程**分配 conhost —— 整片 1832 段下来从 102 秒拖到 270 秒，还会闪窗。
`engine.rs` 的 `run_seg()` 里已经加了。

## 调试钩子

```bat
set SUPFAST_AUTOSTART=1    :: 启动后自动加载「最近」第一条并开跑
set SUPFAST_PANE=timeline  :: 直接切到指定标签页（convert/timeline/history/env/log）
set SUPFAST_OPEN_ADV=1     :: 展开「高级选项」
set SUPFAST_EVAL="..."     :: 界面加载后跑一次 JS，把结果画到页面上再截图
```

配合 `tools\winbox.rs` 从外部摆窗口：

```bat
rustc -O --edition 2021 tools\winbox.rs -o tools\winbox.exe
tools\winbox.exe resize 1980 1410       :: 物理像素；1.5 倍缩放屏上 1980×1410 ≈ 1320×940
tools\winbox.exe state                  :: 查最小化 / 最大化状态
tools\winbox.exe min "Google Chrome"    :: 截图前把挡在前面的窗口最小化
```

## 已知限制

- **没有安装包**：只产出 `exe` + `mod` 目录，绿色解压即用。
- 无边框窗口用 `data-tauri-drag-region` 拖动，**双击标题栏不会最大化**，用右上角按钮。
- 目标机器需要 WebView2 运行时（Win10 1803+ 与 Win11 自带）。
- 右键菜单是关掉的：WebView2 默认会弹「重新加载 / 另存为 / 打印 / 检查」，
  这里用 `SetAreDefaultContextMenusEnabled(false)` 关掉，页面再兜一层 `contextmenu`。
  代价是输入框里右键粘贴没了，`Ctrl+V` 照常可用。
