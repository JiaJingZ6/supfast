# supfast · Tauri 版

同一份界面（`ui/index.html` 与 Electron 版逐字节同源），后端换成 **Rust + Tauri 2**。
Electron 版 **361 MB / 150–250 MB 内存**，这版 **7.3 MB 目录 / 35–40 MB 内存**：

```
dist\  （7.3 MB 整个目录，解压即用）
├─ supfast.exe            3.8 MB
├─ WebView2Loader.dll     157 KB   gnu 目标是动态加载 WebView2，必须带
└─ mod\                   3.4 MB   avs2bdnxml.exe + csri\VSFilterMod.dll
```

```
tauri\
├─ Cargo.toml
├─ build.rs                    tauri-build（生成图标/版本资源）
├─ tauri.conf.json             窗口 1320×940、无边框、withGlobalTauri
├─ capabilities\default.json   主窗口权限（core:default + dialog:default）
├─ icons\icon.ico              4 根竖条，最后一根是强调橙（make_icon.py 生成）
├─ src\engine.rs               引擎：ASS 解析 → 切段 → 并行渲染 → 重写 XML / SUP
├─ src\main.rs                 IPC 路由 + 状态持久化 + 引擎自检
├─ ui\index.html               界面（和 Electron 版同一份）
├─ tools\gcc_shim.rs           windres 用的最小 gcc -E 垫片（见下）
└─ build.bat                   一键编译 + 组装 dist\
```

## 和 Electron 版的差别

| | Electron | Tauri |
|---|---|---|
| 体积 | 361 MB | **7.3 MB**（含 mod 与 WebView2Loader） |
| 内存 | 150–250 MB | **35–40 MB** |
| 冷启动 | ~400 ms | **~120 ms** |
| 引擎 | `engine/supfast.js`（Node） | `src/engine.rs`（纯 Rust，零 crate） |
| 渲染 | Chromium 自带 | 系统 **WebView2**（Win10 1803+/Win11 自带） |
| 状态文件 | `%APPDATA%\supfast-desktop\state.json` | **同一个文件**（历史/最近直接继承） |

引擎是逐函数从 JS 版移植的，并用 `--engine` 自检模式和 JS 版做过逐字节对拍：

| 样本 | PNG | 对拍结果 |
|---|---|---|
| `test2.ass`（43 事件 / 45 段） | 195 | 195 张**全部字节相同**，XML 完全相同，SUP 完全相同 |
| `drama.ass`（1831 事件 / 1832 段，整片 1h44m） | 1839 | 抽样 400 张全部相同，XML 仅 Title/Language 两个可配字段不同，SUP 42,267,261 字节一致 |

整片耗时 102.6 秒（JS 版 121 秒，原版 avs2bdnxml 484 秒）。

## 编译

前置条件都在这台机器上现成：

1. **便携 Rust**：`H:\DIY\rea_sup\rust-toolchain\root`（1.100，`x86_64-pc-windows-gnu`）
2. **C 运行时不用装**：gnu 目标自带 mingw 的 `libkernel32.a` 等导入库

直接跑 `build.bat`。它做的事：

```bat
set PATH=H:\DIY\rea_sup\rust-toolchain\root\bin;<self-contained 目录>;%PATH%
cargo build --release
copy target\release\supfast.exe dist\
xcopy mod  dist\mod\
```

`<self-contained 目录>` = `...\root\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained`。

### 三个工具链坑（都已就地修好，换机器要重做）

| 症状 | 原因 | 处理 |
|---|---|---|
| `dlltool: program not found` / `dlltool: CreateProcess` | 便携工具链只有 `dlltool.exe` 没有 `as.exe`，dlltool 生成导入库时要调 `<同目录>/as` | 从 msys2 的 `mingw-w64-x86_64-binutils` 包里取 `as.exe` 放进 self-contained |
| `windres: preprocessing failed` | `tauri-winres` 编译 `.rc` 时固定调 `gcc -E`，而自带的 gcc 只是链接器壳子 | 用 `tools/gcc_shim.rs` 编出一个 `gcc.exe`（只把 `#pragma` 行滤掉原样吐出）放进 self-contained |
| exe 双击无反应（dist 目录里） | gnu 目标下 `webview2-com-sys` 是**动态**链接 `WebView2Loader.dll`（msvc 才静态） | 把 `target\release\WebView2Loader.dll` 一起放进 dist |
| `icon.ico not found` | tauri-build 要往 exe 里塞图标资源 | `python make_icon.py` 生成（手写 BMP DIB + ICO 头，无第三方库） |

注意 `gcc.exe` 这个垫片**不能**改名成 `x86_64-w64-mingw32-gcc.exe` —— rustc 用那个名字做链接。

## 引擎自检（不进 GUI）

release 版是 windows 子系统、没有 stdout，所以结果写文件：

```bat
supfast.exe --engine analyze --ass test.ass --json out.json
supfast.exe --engine convert --ass test.ass --out out_dir --jobs 12 --json out.json
```

`analyze` 输出 events / frames / segs / sta / dyn / work / ratio / w / h 与 bands、timeline 长度。
`warnings` 只放会影响结果的问题（Timer 与帧率不符、切段数超上限）；`notes` 是纯信息（全静态、多少帧含动态标签），
界面上记一次日志，不弹窗 —— 分析会在每次改设置后重跑，弹窗会变成「按哪个按钮都是同一句」。
`convert` 输出 pngs / secs / speed / epochs / failed / sup_size。

## 冒烟测试钩子

```bat
set SUPFAST_AUTOSTART=1   :: 启动后自动加载「最近」第一条并开跑
set SUPFAST_PANE=timeline :: 直接切到指定标签页（convert/timeline/history/env/log）
set SUPFAST_OPEN_ADV=1    :: 展开「高级选项」，截图核对排版用
set SUPFAST_EVAL="..."    :: 界面加载后跑一次 JS（调试用，结果自己画到页面上再截图）
```

配合 `tools\winbox.rs` 从外部摆窗口（不用手拖）：

```bat
rustc -O --edition 2021 tools\winbox.rs -o tools\winbox.exe
tools\winbox.exe resize 1980 1410   :: 物理像素；当前屏幕 1.5 倍缩放，1980×1410 ≈ 1320×940
tools\winbox.exe min "Google Chrome" :: 截图前把挡在前面的窗口最小化
```

## 性能上的一个坑

`avs2bdnxml` 是控制台程序，而 GUI 进程没有控制台。如果不给 `CreateProcess` 加
`CREATE_NO_WINDOW`，Windows 会为**每一段的每个子进程**分配 conhost —— 整片 1832 段下来
从 102 秒拖到 270 秒，还会闪窗。`engine.rs` 的 `run_seg()` 里已经加了。

## 右键菜单是关掉的

WebView2 默认会弹自己的菜单（重新加载 / 另存为 / 打印 / 检查），对一个装好就用的桌面工具是多余的。
两层一起关：

1. `main.rs` 的 `setup` 里把 `ICoreWebView2Settings::SetAreDefaultContextMenusEnabled(false)` 关掉 ——
   这是权威开关，WebView2 的菜单是宿主进程画的，光靠页面拦不住。
2. `ui/index.html` 里 `addEventListener('contextmenu', e => e.preventDefault(), true)` 兜底。

代价：输入框里右键粘贴没了，`Ctrl+V` 照常可用。若这一步失败，会往 `%TEMP%\supfast-ctxmenu.log`
写一行原因（成功时什么都不写）。

## 已知差异

- **没有安装包**。Tauri 的 NSIS/WiX 打包器要额外工具链，这里只产出 exe + mod 目录，绿色解压即用。
- 无边框窗口的拖动改成了 Tauri 的 `data-tauri-drag-region`，双击标题栏最大化因此没有（用右上角按钮）。
- 目标机器需要 WebView2 运行时；Win10 1803 之后和 Win11 默认自带。
