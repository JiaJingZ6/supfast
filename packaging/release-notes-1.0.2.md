## supfast 1.0.2

把 ASS / SSA 字幕按「画面是否变化」切成段落，只为必要的帧渲染，输出可直接混流的 PGS `.sup` 与 BDN `.xml`。

这一版把两条最影响实际播放的链路做完了：**默认产出的 `.sup` 时序合规化**，以及**内置 Rust 原生 PGS 编码器**（严格合规整流 + 无损调色板渐变 + 无损帧间对象拆分）。同时移除了外部 SUPer 后端 —— 软件现在完全不需要 Python 或任何外部编码器。

### 下载

`supfast-1.0.2-win64.zip`（约 3.3 MB）— 绿色免安装，解压后双击 `supfast.exe`。
从 1.0.1 升级直接覆盖即可，`%APPDATA%\supfast-desktop\state.json` 里的设置与历史会保留；旧配置里残留的 SUPer 开关会在启动时自动清除。

- Windows 10 1803 以上 / Windows 11（64 位）
- 需要 WebView2 运行时：Win11 与新版 Win10 已自带；缺失时双击无窗口，装一次即可
- `mod\` 目录是字幕渲染引擎（avs2bdnxml + VSFilterMod），不能删

### 这一版改了什么

**默认 `.sup` 现在时序合规（最关键）**

默认拼段流的段头时间戳被整套重写，与内置原生编码器共用同一套模板：

- 呈现时刻改读分段原始 PCS 的 PTS，不再按「PCS 序号」推时间 —— `avs2bdnxml` 会去重，序号不等于帧号，旧做法让 `test2` 的源头 PTS 最大偏 **1.21 秒**，现在降到 **0.195 秒**。
- 解码提前量收紧为 `wipe + 一帧`（1080p 为 9587 tick），取代越过规范 `2·wipe` 红线的旧值 21250；所有段 `DTS = PTS − lead`，PDS / END 的 `PTS = DTS = base`，彻底清掉 `0xFFFF…` 哨兵 PTS 与 `DTS = 0`。
- 跨段只在会踩「新 epoch 的 DTS 必须晚于上一 epoch 最后呈现」时整体平移，段内 PTS 严格递增，`PTS_DUP` 消失。
- 实测：`test2` / `drama` 的 SUPer `check_pts_dts_sanity` 由 `False` 变 **`True`**，时序相关错误日志 221 / 4262 行降为 **0**；硬解模型下来不及解码的显示集从 **174 / 2430 降为 0**；内置自检 `dts_back` / `pts_back` / `tight_ds` 全为 0。

**内置 Rust 原生 PGS（高级选项里打开，推荐）**

不依赖 `avs2bdnxml` 的 SUP 输出，直接吃 BDN XML + PNG 重新编码，额外产出 `xxx.native.sup`，不覆盖默认产物：

- 严格合规整流：epoch 切分、Acquisition Point / Normal Case、WDS / PDS / ODS / END 全套段时序按 HDMV PGS 规范排布。
- **无损调色板渐变**：同一张 bitmap 用逐帧 PDS 做 palette update，不做有损近似。
- **无损帧间对象拆分**：逐块精确 RLE 比较代替 SUPer 的 SSIM 0.986 有损判定，复用 `object_id`。
- 四个合成 / 真实样本的 SUPer `is_compliant` 均为 `(True, 0)`、`check_pts_dts_sanity` 均为 `True`，内置自检 0 错 0 警，逐像素解码比对 0 误差。`test2` 的 `.sup` 从 3.37 MB 压到 **1.32 MB**。

**转换收尾与验证工具**

- 每次转换收尾自动做 SUP 合规自检，日志页也能手动「检查 SUP」；报告含段统计、时序回退、PU / 多对象等判据。
- 新增软件解码器模型的对拍基准：内容轨 + 名义 / 有效双时间轴、源头 PNG 对拍、黄金帧清单与参考画面导出、RX / object buffer 峰值与 `decode_risk` 判据。
- 新增 `--engine compare`：把真机播放录屏用 ffmpeg 按 `present_pts` 抽帧，裁到黄金帧 bbox 后逐像素比对并导出差异热图；`tools\compare_recording.bat` 可一键跑。**注意这是离线工具，真机实拍验证仍需自行录制播放器画面。**

**不再需要外部 SUPer**

- 删除外部进程调用后端：不再探测 `supercli.py` / 环境变量，不再产出 `.super.xml` / `.super.sup` / `.super.log`，GUI 里对应开关与 CLI 的 `--super` 参数一并移除。
- SUPer 现在只作为离线规范依据 / 校验工具，不是运行时依赖。主程序体积 4,282,368 → **4,264,448 B**。

### 已知限制

- 没有安装包，是绿色目录；升级直接覆盖
- 默认拼段流在 `test2` 这类「相邻段区间偶有重叠」的素材上仍会有 `WDS_CHANGED` 结构性警告（来自 `avs2bdnxml` 分段输出，不是时序问题）；要 0 错 0 警请勾「Rust 原生 PGS」
- drama 类素材为让每个 epoch 都来得及解码，源头 PTS 会有最多约 0.15 秒的整体平移，这是可解码性换绝对对齐的取舍
- 真机 / 硬解实拍录屏对照尚未完成，软基准结论不能替代真机验证
- 双击标题栏不会最大化（右上角按钮正常）；右键菜单被关闭，输入框里请用 `Ctrl+V` 粘贴
