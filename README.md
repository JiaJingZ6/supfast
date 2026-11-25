# supfast

把 ASS / SSA 字幕按「画面是否变化」切成段，只渲染必要的帧，输出 PGS（`.sup`）与 BDN（`.xml`）。
内置 Rust PGS 编码器可把同一份 BDN 一次性整流成严格合规的流（含无损调色板渐变），不需要 Python 或外部编码器。

给压制组、字幕组和 DIY 蓝光用的：一条 1 小时 44 分的片子，只要 2.4% 的帧真的在动，
剩下 97.6% 的画面是静止的 —— 静止段只需要渲染段首那一帧。

<img src="docs/ui.png" width="880" alt="supfast 主界面：转换页，尚未选择字幕 —— 上方是文件行，中间是切段计划区域，下方是预设与输出目录">

## 它怎么工作

1. **解析** ASS：取每条 Dialogue 的起止时间、层号、样式、文字，换算成帧号。
   编码依次尝试 UTF-8（含 BOM）→ UTF-16 LE → 系统 ANSI（中文即 GBK，走 `MultiByteToWideChar`，
   中文内容必须真解码 —— 否则不同台词会解成同一串替换字符，切段签名碰撞，时间相接的两句被并成一段）。
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
| **环境** | `avs2bdnxml` / VSFilterMod / AviSynth / 内置引擎 / SUPer（可选）六项自检；Rust 原生 PGS 是内置的，不需要额外环境 |
| **日志** | 百分比 + 进度条 + 实时日志，可取消、可打开输出目录，收尾自动做 SUP 合规自检；开启原生 PGS / SUPer 时另给多套流的自检对比 |

- **预设**一键套用输出规格：蓝光 23.976/1080p、PAL 高清 25/1080p、DVD 25/480p、电视 29.97/1080i。
- **高级选项**（折叠）里是分辨率覆盖、mod 目录、七个合成开关，以及可选的
  Rust 原生 PGS 整流（内含「调色板渐变」开关）与 SUPer 重编码。

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
<目录>\<字幕名>\
├─ <字幕名>.xml           BDN 字幕（PNG 序列的时序与坐标）
├─ <字幕名>.sup           PGS 流（默认拼段），可直接 mux 进 m2ts / mkv
├─ <字幕名>.native.sup    Rust 原生整流 PGS（开启「Rust 原生 PGS」时）
├─ <字幕名>.super.sup     SUPer 重编码 PGS（开启 SUPer 时）
└─ <帧号>.png             按需渲染的图形
```

无论填不填输出目录，产物**总是**落在 `<目录>\<字幕名>\` 这一层里：
留空时 `<目录>` 取字幕所在目录，即 `<字幕目录>\<字幕名>\`；填了或浏览选了某个目录，
就用它当父目录，产物进 `<所选目录>\<字幕名>\`。所以「留空」和「选字幕所在目录」结果一模一样。
选中的目录如果本身已经叫字幕名，不会再叠一层。框下那行小字会显示实际落点。

整片实测（1831 事件 / 1832 段 / 1 小时 44 分）：**1839 张 PNG，102 秒**，8 路并发。
同一份字幕交给 `avs2bdnxml` 原版单跑要 484 秒；逐帧比对 1839 张全部一致
（3 个起始帧差 1 帧，原因是 ASS 时间码只有两位小数，取整会顶到下一帧）。

## SUP 合规自检

`.sup` 是逐段 `avs2bdnxml` 的输出重映射时间戳后拼起来的。拼接时会按解码器模型推导
DTS、并保证 PCS 的 PTS 跨段不倒退；转换收尾再**只读**体检一次，结果按行写进「日志」页。
日志页的「检查 SUP」按钮也可以对任意已有的 `.sup` 单跑一次（不改动文件）。

检查项（依据 HDMV PGS 解码器模型）：

| 类别 | 检查 |
|---|---|
| 结构 | `PG` 段头对齐、负载长度、段类型、PCS…END 配对、首条 Epoch Start |
| 引用 | ODS 的 `data_length` 与分片序列、PCS 引用的 object_id 在 epoch 内已定义、palette_id 与同组 PDS 一致、window_id 与同组 WDS 一致 |
| 时序 | PCS PTS 递增、DTS 非递减且 ≤ PTS、`PTS−DTS` < 1 秒 |
| 缓冲 | 同一 DTS 取走的段 ≤ RX buffer 1 MiB；epoch 内对象面积 ≤ object buffer 4 MiB、唯一 object_id ≤ 64 |
| 带宽 / 余量 | 平均与瞬时码率峰值（统计展示）；按 Rd = 16e6 px/s、Rc = 32e6 px/s 估算的「提前量偏紧」组数（仅供参考） |

错误（红）是会导致画面错乱或播放失败的问题；警告（黄）是严格蓝光规范不合规、
但多数播放器能容忍的情况。`ok` 只看错误，`compliant` 要求连警告都没有。
自检不影响转换是否成功，也不改动 `.sup`。

`PTS==DTS` 与「解码余量不足」只作统计、**不判不合规**：H.222 在两者相等时 PES 里
只写 PTS，SUPer 的 `check_pts_dts_sanity` 更明确要求 PDS 段 `pts==dts`；而 SUPer 靠
display set buffering 提前解码，不能只看单个显示集的 `PTS−DTS`。早期版本把这两项
列为警告，会误伤 SUPer 自己判定 compliant 的流（下面的三方校准就是这么发现的）。

> 实测对照（三方校准，2026-11 默认流时序合规化后）：
> - 原版 `avs2bdnxml` 单跑产出的 `ref.sup`：0 错误 0 警告；
> - supfast 默认拼段流：`drama.ass`（1832 段 / 10961 个 PGS 段）**0 错误 0 警告、严格合规**；
>   `test2.ass` 0 错误 1 警告（`WDS_CHANGED` 22 处）—— 来自「每段独立 epoch、
>   相邻段窗口定义偶有变化」，属拼接链路固有现象，多数播放器容忍；`PTS_DUP` 已消除；
> - **SUPer 重新编码的 `.super.sup`：0 错误 0 警告、严格合规** —— 用权威编码器反向
>   验证这套规则没有误伤合规流。
> - **Rust 原生整流流 `.native.sup`：0 错误 0 警告、严格合规**，且首个 ODS 解码后
>   与源 PNG 逐像素一致 —— `test2.ass` 上顺带把体积从 3.37 MB 压到 1.32 MB。
>
> 早先会报的 `PTS_EQ_DTS` / `DTS_NOT_MONO` / `TIGHT_DECODE` / `PTS_BACK` / `PTS_DUP`
> 已在 `sup_retime` 里修掉：默认拼段流从 2026-11 起与原生整流流共用同一套段时序
> 模板（同一显示集所有段 DTS = `PTS − lead`，PDS / END 的 `PTS = DTS`，见
> [段时序合规](#段时序合规与-super-verifier-对拍)），SUPer `check_pts_dts_sanity`
> 对 test2 / drama 默认流均由 `False` 变为 `True`。要把剩下的 `WDS_CHANGED` 也消除，
> 打开下面的 Rust 原生 PGS（内置、推荐）或 SUPer 后端即可。

## Rust 原生 PGS（内置 · 推荐）

默认 `.sup` 是「每段一个 epoch、逐段拼接」的产物，快；2026-11 起已做段时序合规化
（SUPer `check_pts_dts_sanity` 通过），但相邻段各自带完整 WDS/PCS，`test2.ass` 上
仍会留 `WDS_CHANGED` 一个警告。想要**严格合规**又不想装 Python，
在「高级选项 → Rust 原生 PGS」打开开关：supfast 自带的编码器把主 `.xml` + 索引 PNG
一次性规划成一条流，额外产出 `<字幕名>.native.sup`，不覆盖默认产物、不改变 PNG。

它按 HDMV PGS 规范独立实现（不是 SUPer 的移植，也不分发任何 GPL 代码）：

| 环节 | 做法 |
|---|---|
| 输入 | 直接读 8bit 索引 PNG，用 `PLTE`/`tRNS` 生成 PDS（RGB→YCbCr，BT.709/601）；`avs2bdnxml -p 1` 已经量化好，不需要量化器 |
| RLE | 标准 PGS 行程编码；单个对象超过 65535 字节自动切 FIRST / 中间 / LAST 分片 |
| epoch | 按 object buffer（面积 3 MiB 预算、60 个对象上限）与 2 秒空档把事件串分组，epoch 内窗口定义固定 |
| 对象复用 | 同一 epoch 内「位图 + 调色板」完全相同就复用 `object_id`，走 Normal Case，不重传 ODS |
| 调色板渐变 | 连续同图、仅逐帧变色的事件压成「一张位图 + 逐帧调色板」，后续帧只发 palette update 显示集；序列种类超 255 时整链回退逐帧 ODS（零画质损失），详见下节 |
| 帧间对象拆分 | 连续单图局部变化时，按最优分割线把每帧切成 2 个对象，逐帧只重传变化的那一块；无净收益或超预算就整链回退，同样零画质损失，详见下节 |
| 显示集 | 组首 Epoch Start、新位图 Acquisition Point、空档补清屏显示集；Epoch Start / Acquisition 都带同一份 WDS（逐帧大窗口踩线的 Acquisition 除外，见下节） |
| 时序 | 同一显示集所有段共用一个 DTS = `PTS − lead`（`lead = wipe + 一帧`，1080p 为 9587）；PCS/WDS/ODS 的 PTS 取呈现时刻，PDS/END 的 PTS 也取该 DTS（规范强制 `dts == pts`）。段 DTS 全程非递减、生命周期 < 1 秒，SUPer `check_pts_dts_sanity` 全项通过，详见下节 |

PNG 重复度越高收益越大：`test2.ass` 的 195 张 PNG 只有 52 张唯一内容，复用 127 次，
SUP 从 3.37 MB 降到 1.23 MB；`drama.ass` 的图几乎张张不同，体积与原流基本持平。

| 样本 | 默认拼段流 | SUPer 重编码 | Rust 原生 PGS |
|---|---|---|---|
| `test2.ass` | 3.37 MB / 0 错 1 警 | 3.42 MB / 0 错 0 警 | **1.32 MB / 0 错 0 警** |
| `drama.ass` | 42.27 MB / 0 错 0 警 | 43.07 MB / 0 错 0 警 | **42.18 MB / 0 错 0 警** |

不需要 `avs2bdnxml` 的 SUP 输出、不需要 Python / SUPer；只多一个纯 Rust 的 `flate2`
（解压 PNG 的 zlib 数据），无 DLL、静态链接。两套 SUP 都会在日志页各跑一次合规自检。

### 调色板渐变（淡入淡出 / 卡拉OK）

默认开启，在「高级选项 → Rust 原生 PGS → 调色板渐变」可关；命令行用
`--no-palette-fade` 关闭（对 `--engine pgs` 和 `--engine convert --native-pgs` 都生效）。

原理与 SUPer 的 `PaletteSequenceEffect` 相同，但**不做有损聚类**：

| 步骤 | 做法 |
|---|---|
| 候选链 | 同一 epoch 内，连续事件都是单个 Graphic、尺寸与 X/Y 完全相同、时间首尾相接（淡入淡出 / 卡拉OK 逐帧变色的典型形态），最多 512 帧 |
| 反量化 | `png_rgba` 把每帧索引 PNG 还原成逐像素 RGBA，以「每个像素在 N 帧上的 RGBA 序列」为特征 |
| 共用位图 | 序列相同的像素共用同一个 palette entry，全透明序列固定占 entry 0；序列种类 ≤255 时生成一张共用 ODS 位图 |
| 逐帧调色板 | 链首发 Epoch Start / Acquisition（WDS + PDS + ODS），后续每帧只发一条 Normal Case 的 **palette update 显示集**：PCS 置 `palette_update_flag=0x80`、不带 WDS/ODS、`palette_diff` 只发与上一帧不同的条目、composition objects 与链首逐字节相同，PDS 版本逐次 +1 |
| 无损回退 | 序列种类 >255（超出 PGS 调色板容量）时**整条链回退**逐帧 ODS，不引入任何近似误差 |

因此渐变帧不重传位图，只换颜色/透明度。`verify` 新增 `PU_BAD_STATE` / `PU_WITH_WDS` /
`PU_WITH_ODS` / `PU_BAD_PDS` / `PU_NO_COBJ` / `PU_COBJ_CHANGED` / `PU_PDS_VERSION` 七项检查，
日志页在有渐变链时会多一行「调色板动画」。

实测（8 帧同一张 64×24 位图、alpha 0→255 淡入 + 255→0 淡出的合成样本）：

| 模式 | 体积 | 显示集 | ODS 组 | palette update | 逐像素比对 |
|---|---|---|---|---|---|
| 渐变开 | 910 B | 9 | 1 | 7 | 12,288 采样 0 误差 |
| 渐变关 | 3,101 B | 9 | 8 | 0 | 12,288 采样 0 误差 |

真实素材上 `test2.ass` 没有可用链（`fade_chains=0`），`drama.ass` 识别出 1 条 2 帧链。
当前版本产物：`test2` 1,228,073 B、`drama` 42,168,260 B（阶段一分别为
1,321,675 B / 42,176,509 B）。两者自检均 0 错 0 警、严格合规；用 SUPer 的
verifier 交叉验证，`is_compliant` 返回 `(True, 0)`、`check_pts_dts_sanity` 返回
`True`（详见 [已知限制](#已知限制)）。

### 帧间对象拆分（局部变化 / 滚动）

默认开启，在「高级选项 → Rust 原生 PGS → 帧间对象拆分」可关；命令行用
`--no-object-split` 关闭（对 `--engine pgs` 和 `--engine convert --native-pgs` 都生效）。

SUPer 的 `ObjectDetector` 用 SSIM 阈值（约 0.986）判断连续帧能否并进同一个 object，
只传一次 ODS —— 那是**有损近似**。supfast 做零画质损失的等价优化：

| 步骤 | 做法 |
|---|---|
| 候选链 | 同一 epoch 内，连续事件都是单个 Graphic、尺寸与 X/Y 完全相同、时间首尾相接（≤128 帧），且与调色板动画链互斥（渐变优先、更省） |
| 最优分割线 | 在宽/高的 1/4、1/3、1/2、2/3、3/4 共十条候选线里，按「唯一块数 × 段头 + 去重 RLE 字节 + 每帧多一个 composition object」估算净收益，取 ≥64 B 且最大的一条 |
| 切块 | `split_rle` 把每帧索引矩阵按该线切成两个矩形各自做 PGS RLE；不重新量化、PLTE 不变，两块拼回去与原图逐像素一致 |
| 逐块复用 | 每块用「PLTE + tRNS + RLE」查 epoch 内对象表，**完全一致才复用** `object_id`，只重传变化的那一块（PGS 单显示集最多 2 个 composition object） |
| 回退 | 无净收益、`object_id` / object buffer 预算不足、调色板不一致，都整链回退逐帧 ODS |

**一个容易踩的规范坑**：拆分链后续帧必须走 Normal Case，不能是 Acquisition Point。
SUPer verifier 在 acquisition 时会清空对象缓冲（`ods_filled.clear()`），把复用旧对象
判成 `Using expired memory`；Normal Case 不清空缓冲、且允许同集定义新对象，才能合法
复用链首的恒定半区。`verify` 另加 `COBJ_TOO_MANY` / `COBJ_DUP_ID` 两项检查，
`stats` 里有 `multi_obj_ds` / `max_cobjs`。

实测（8 帧 320×64 合成样本：左半静态噪点、右半逐帧变色，255 色；RGBA 序列种类
>255，调色板渐变回退、由拆分接管）：

| 模式 | 体积 | 显示集 | ODS 段 | 每帧 composition objects | 逐像素比对 |
|---|---|---|---|---|---|
| 拆分开（默认） | **104,332 B** | 9 | 9（恒定半区只传 1 次） | 2 | 163,840 采样 0 误差 |
| 全关 | 175,957 B | 9 | 8 | 1 | 163,840 采样 0 误差 |

省 71,625 B / 40.7%。只开拆分（`--no-palette-fade`）与默认产物逐字节一致；只关拆分
（`--no-object-split`）等于全关 —— 说明两条链确实互斥且渐变优先。SUPer verifier 的
`is_compliant=(True, 0)` 且 `check_pts_dts_sanity=True`。真实素材 `test2.ass` /
`drama.ass` 没有符合条件的单图局部变化链，`split_chains=0`。

### 段时序合规（与 SUPer verifier 对拍）

早期原生流把 PDS / END 的 DTS 写 0（沿用拼段工具链惯例），SUPer 的
`check_pts_dts_sanity` 会判 PTS-DTS out of spec。这**不是模型差异，是真不合规**：
avs2bdnxml 原版 `ref.sup` 在 SUPer 那 20 多条时序条件下全部通过，而我们的
`dts=0` 被 SUPer 的 `PDS.__init__` / `END.__init__` 强制改写成显示集 PTS，夹在
ODS（DTS 更早）之间就造成段 DTS 回退。现在按 `ref.sup` 的排布模板写段：

| 段 | PTS | DTS |
|---|---|---|
| PCS / WDS / ODS | 显示集呈现时刻 `pts` | `base = pts − lead` |
| PDS / END | `base` | `base`（规范强制 `dts == pts`） |

`lead = wipe + 一帧`（`wipe = 画面面积·90k/Rc`，1080p 为 9587 ticks），同时落在
`(wipe, 2·wipe]` 内，满足 Epoch Start 的 `PTS−DTS > wipe` 与 WDS 的
`PTS−DTS ≤ 2·wipe`。配套三条规划约束：epoch 只在「显示集级空档 > lead」处切分
（连续帧处切会让新 epoch 的 DTS 早于上一 epoch 的最后呈现，且口径要用显示集 PTS
而不是事件 out）；逐帧连续的大窗口 Acquisition 在「相邻 PTS 间隔 ≤ 窗口擦除时间」
时省略 WDS 并降级 Normal Case（`test_diplayset` 规定非 Normal Case 必须带 WDS，
而带 WDS 又要求更大的显示集间隔）；PTS 严格递增。

实测四个样本（合成淡入链 / 合成拆分链 / `test2` / `drama`）的 SUPer
`is_compliant` 均为 `(True, 0)`、`check_pts_dts_sanity` 均为 `True`，内置自检同样
0 错 0 警，逐像素解码比对 0 误差。

#### 默认拼段流也已时序合规化（2026-11）

默认 `<名>.sup` 是各段 `avs2bdnxml` 输出经 `sup_retime` 重映射 PTS 拼起来的。
早期它沿用 `avs2bdnxml` 惯例：PCS 的 `DTS = PTS`（lead=0）、PDS 写 `0xFFFF…`
哨兵 PTS、PDS / END 写 `DTS=0`，SUPer `check_pts_dts_sanity` 直接判 out of spec，
第二版硬解模型里 test2 174 个 / drama 2430 个显示集全部来不及解码。现在
`sup_retime` 与 `src/pgs.rs` 共用上表这套模板：

- 所有段 DTS 统一 `base = ds_pts − lead`，`lead = wipe + 一帧`（1080p 为 9587），
  PDS / END 的 `PTS = DTS = base`，彻底覆盖哨兵 PTS 与 `DTS=0`；
- 呈现时刻不再按 PCS 序号推（`avs2bdnxml` 会去重，序号 ≠ 真实帧号），改为读取
  分段原始 PCS 的 PTS 作为「段内帧偏移」再加段起始 ticks，与 BDN `InTC` 对齐；
- 跨段只在会踩「新 epoch 的 DTS 必须晚于上一 epoch 最后呈现」时整段推到
  `floor + lead + 1`；段内 PCS 严格递增，同刻「显示 + 清屏」推后一帧消掉 `PTS_DUP`。

实测（新二进制重新 `convert` 生成，产物与旧实现字节数相同）：

| 默认流 | 内置自检 | SUPer `check_pts_dts_sanity` | SUPer `is_compliant` | 源头 PTS 最大偏差 |
|---|---|---|---|---|
| `test2` | 0 错 1 警（`WDS_CHANGED` 22 处），`dts_back=0` / `pts_back=0` / `tight_ds=0`，`PTS_DUP` 已消除 | `False → True` | `(False, 0)` | 108847 → **17516 tick（1.21 s → 0.195 s）** |
| `drama` | 0 错 0 警、严格合规 | `False → True` | `(True, 0)` | 562 → 13595 tick（0.151 s，合规约束下的取舍） |

`test2` 的 `is_compliant` 仍为 `(False, 0)`，但诊断后确认与时序无关：失败项全部是
`avs2bdnxml` 分段输出固有的 `Window change mid-epoch` / `Object-slot dimensions
mismatch` / `Object ID … is reserved` 结构性错误，改前改后 verifier 日志行数同为
48，且时序相关日志从 221 行降为 0。要连这些结构错误一起消除，用
[Rust 原生 PGS](#rust-原生-pgs内置--推荐) 或 SUPer 后端重编码即可。

**暂不支持**（后续阶段）：窗口布局优化，以及 SUPer 那种带 SSIM 容差的帧间合成
（有损，本项目坚持零画质损失）。另外，单个显示集超过 1 MiB 会直接报错而不是产出
超 RX 缓冲的流 —— 常规字幕用不到，全屏大图需要注意。输入 PNG 的颜色数也应 ≤255：
PGS 要求 palette entry `0xFF` 是全透明保留项，若被用作可见色会直接报错而不是产出坏流。

## 真机播放验证前置：对拍基准

真机 / 硬解播放验证没法自动化，所以先把「播放器模型」下的等价性做成可复现的
**软基准**（第二版已补上硬解缓冲模型），再把第三角做成可复现的录屏对照命令。

| 三角 | 谁和谁比 | 由谁做 |
|---|---|---|
| ① 源头对拍 | 源 PNG（BDN ground truth） ↔ SUP 解码画面 | `--xml`，按事件序号对齐 |
| ② 跨流对拍 | 参考流 ↔ 任意多条候选流 | `--ref` + 多个 `--cand` |
| ③ 真机对拍 | 播放器录屏 ↔ 黄金帧清单 / 参考画面 | `--engine compare`，ffmpeg 抽帧逐像素比对 |

`src/pgsdec.rs` 按 HDMV PGS 解码器模型把任意 `.sup` 还原成「显示集 PTS → 屏幕画面」：
Epoch Start 清状态、ODS 分片 + RLE、PDS 累积 / palette update、多对象合成、清屏帧，
并按 **WDS 窗口裁剪** 与 `object_cropped_flag` 处理对象内裁剪（变长 cobj 解析）。
第二版再加一层硬解时序：段按 DTS 进 RX buffer，RLE 解码受 `Rd=16e6 px/s`、
清屏 / 窗口合成受 `Rc=32e6 px/s` 限制，得 `present_pts = max(pts, dts + need)`；
同时统计 RX buffer 1 MiB / object buffer 4 MiB 峰值。`src/baseline.rs` 做双轨对拍并
导出黄金帧清单 + 参考画面 PNG；`src/compare.rs` 拿录屏按 `present_pts` 抽帧比对。

跨流对拍分两条轨道，避免把「时间轴平移」误判成「画面编错」：

| 轨 | 做法 | 回答 |
|---|---|---|
| 内容 | 可见帧按序号对齐，hash 不同的帧二次解码逐像素比对 | 每张图有没有被编对 |
| 时间轴（名义） | 两流所有 PTS 的并集上采样，取各自「最后一个 `pts ≤ t`」的帧比 hash | 同一时刻挂着哪张画面 |
| 时间轴（有效） | 同上但改用 `present_pts`（late 帧会被推后） | 硬解真正来得及呈现的画面 |

时间轴轨把 hash 不同再分三类：`presence`（一方已清屏）、`color_only`（像素集合与
bbox 完全相同，只是 ±1 的 YCbCr 舍入）、`frame_mismatch`（真错位）。比对在 PGS 原生
`[Y, Cr, Cb, Alpha]` 域进行，不做 YCbCr→RGB 往返；透明像素的 RGB 不参与比较。
二次解码只对 hash 不同的帧、按 256 帧分块，内存 O(一帧)。

| 结论 | 含义 |
|---|---|
| `identical` | 内容轨与两条时间轴轨都完全一致，且解码可行 |
| `equivalent` | 内容轨只差 `--color-tol` 内（默认 2，工具链间 ±1 舍入），时间轴一致 |
| `content_ok_time_shift` | 每帧画面无损，但两流 PTS 有平移或清屏时机不同 —— **真机验证重点** |
| `decode_risk` | 任一流有 late 显示集（`dts + need > pts`）或 RX / object buffer 超限；名义时间轴已不足以描述真机，看 `timeline.decode` / `timeline.effective` |
| `divergent` | 内容帧有超过容差的实质差异，看 `content.worst` 定位 |

```bat
:: ①② 一键跑四组样本（不重渲染，直接用 test\ 下已有 SUP + XML + PNG）
tools\baseline.bat
:: 单独对拍两条已有 SUP
supfast.exe --engine baseline --ref test2.sup --cand native=test2.native.sup ^
  --cand ref=ref.sup --xml test2.xml --frames frames.json --png-dir frames ^
  --json baseline.json
:: ③ 拿录屏对照（frames.json 里已记录参考 PNG 目录）
tools\compare_recording.bat -Frames frames.json -Recording rec.mp4 -OffsetMs 1001
```

`--cand` 可重复，格式 `名字=路径`（省略名字就用文件名）。其余参数：`--fps`（默认
23.976）、`--tol`（时间轴容差帧数，默认 1）、`--color-tol`（逐通道容差，默认 2）、
`--ratio-tol`（超容差像素占比上限，默认 0.005）、`--frames`（黄金帧清单 JSON）、
`--png-dir` + `--max-png`（导出 bbox 裁出的 RGBA 参考画面）、`--json`（完整报告）。
一键脚本把 `test2` / `drama` 的参考流指向时序合规化后的新默认产物 `<名>.retime.sup`
（用 `--engine convert` 生成 `<名>\<名>.sup` 后重命名），并把合规化前的旧默认流
`<名>.sup` 作为 `default-old` 候选做 A/B；缺任一文件会打印 `[SKIP]` 而不是报错。
结果落在 `tools\baseline-results\`：`<时间戳>_<样本>.json`、
`<时间戳>_<样本>.frames.json`、`<时间戳>_<样本>.png\`，以及 `<时间戳>_summary.md`。

`compare` 的参数：`--frames`（帧清单）、`--recording`（录屏，ffmpeg 能读的任何格式）、
`--ffmpeg`（默认自动探测 PATH / `H:/ffmpeg/bin`）、`--offset-ms`（录屏起点相对 SUP
PTS 0 的毫秒偏移，用来对齐播放器起始时间）、`--cmp-tol`（逐通道容差，默认 12）、
`--cmp-ratio-tol`（超容差占比上限，默认 0.02）、`--shrink`（bbox 内缩像素，默认 1，
跳过抗锯齿边缘）、`--min-alpha`（默认 200，跳过半透明边缘）、`--nominal`（改用名义
PTS 抽帧）、`--max-frames`（0 = 全部）、`--shots-dir`（导出录屏裁图与差异热图）、
`--json`。结果落 `tools\compare-results\`，含 JSON 判定、Markdown 摘要、每帧
`*.shot.png` 与 `*.diff.png`（红越亮代表通道差越大）。

报告里 `reference` / `streams` 给每条流的体积、显示集数、帧数、epoch 数、合规自检
简报，以及 `clip`（窗口裁剪 / cropped 统计）与 `decode`（late 显示集、RX 峰值、
DTS 单调性）；`pairs[]` 分 `content` / `timeline` 两段，`timeline` 内另有 `decode`
与 `effective`（基于 `present_pts` 的有效时间轴）；`source.checks[]` 是源头对拍；
`golden.frames[]` 每帧含 PTS、`present_pts`、`late_ticks`、hash、bbox 与参考 PNG 名。

实测（2026-11-25，默认流时序合规化后重跑；四组样本 / 11 条候选，整轮约 136 秒。
参考流换成新默认产物 `*.retime.sup`，并额外把旧默认流作为 `default-old` 候选做 A/B）：

| 样本 | 参考 → 候选 | 内容轨 | 名义 / 有效时间轴 | late 显示集（ref / cand） | 综合 | 源头对拍 |
|---|---|---|---|---|---|---|
| fade | 渐变关 → 渐变开 / 最终 | 8/8 帧一致 | identical / identical | 0 / 0 | `identical` | 8/8 逐像素一致，Δ=0 |
| split | 拆分关 → 拆分开 / 最终 | 8/8 帧一致 | identical / identical | 0 / 0 | `identical` | 8/8 逐像素一致，Δ=0 |
| test2 | 新默认 → 原生最终 / 阶段一 | 129 帧：73 全一致 + 56 帧 Δ=1，**0 实质差异** | shifted / shifted | 0 / 0 | `content_ok_time_shift` | 原生 129/129 逐像素一致（Δ=0、PTS 差 0） |
| test2 | 新默认 → avs2bdnxml 原版 | 129/129 完全一致 | shifted / shifted | 0 / 64 | `decode_risk` | 73/129 完全一致（56 帧 Δ=1） |
| test2 | 新默认 → 旧默认 | 129/129 完全一致 | shifted / shifted | 0 / 174 | `decode_risk` | 73/129 完全一致（56 帧 Δ=1） |
| drama | 新默认 → 原生阶段一 | 1832 帧：3 全一致 + 1829 帧 Δ=1，**0 实质差异** | shifted / shifted | 0 / 0 | `content_ok_time_shift` | 原生 1832/1832 逐像素一致（Δ=0、PTS 差 0） |
| drama | 新默认 → 原生最终 | 同上 | shifted / shifted | 0 / 1 | `decode_risk` | 同上 |
| drama | 新默认 → 旧默认 | 1832/1832 完全一致 | shifted / shifted | 0 / 2430 | `decode_risk` | 3/1832 完全一致（1829 帧 Δ=1） |

四条结论：

- **画面无损**：原生整流流（含调色板渐变、帧间对象拆分）与源 PNG 逐像素一致；
  与拼段流的唯一差异是 ±1 的 RGB→YCbCr 舍入（两套矩阵 / 取整口径），PSNR 71–82 dB。
  fade / split 合成样本两条时间轴全 `identical`，说明两条优化链在解码器模型下与
  逐帧 ODS 完全等价 —— 不是「只拿源 PNG 自证」，而是跨编码策略的独立验证。
- **默认流的硬解风险已消除**：旧默认流所有 PCS 的 `DTS = PTS`（lead=0）、PDS 带
  `0xFFFF…` 哨兵 PTS、`DTS=0`，硬解模型下 test2 174 个 / drama 2430 个显示集全部
  late。新默认流统一 `lead = 9587`、PDS / END 写 `base = PTS − lead` 且
  `PTS = DTS`，**参考流自身 late = 0**：test2 与两条原生流对拍从 `decode_risk`
  降为 `content_ok_time_shift`，drama 与阶段一原生流同样降为 `content_ok_time_shift`。
  上表剩下的 `decode_risk` 都来自**候选流**（旧默认 / avs2bdnxml 原版 / 原生最终版
  那 1 个余量不足的显示集），不再是默认产物的问题。SUPer
  `check_pts_dts_sanity` 对 test2 / drama 默认流也都由 `False` 变 `True`。
- **时间轴口径不同，且合规化有代价**：`sup_retime` 呈现时刻现取分段原始 PCS PTS
  （见 [段时序合规](#段时序合规与-super-verifier-对拍)），原生编码器按 BDN `InTC`
  规划（`mono_times`）。源头对拍里原生流与源 PTS 差为 0；默认流 `test2` 最大偏
  **17516 tick / 0.195 s**（旧实现 108847 tick / 1.21 s，改善约 6 倍），但 `drama`
  从旧实现的 562 tick 升到 13595 tick / 0.151 s —— 这是 PGS 硬约束「新 epoch 的 DTS
  必须晚于上一 epoch 最后呈现」在密集连续段上必须整段推后造成的，是拿「可解码」
  换「绝对对齐」的取舍，不是画面编错。有效时间轴轨与名义轨结论一致（都
  `shifted`），说明这不是解码来不及造成的假象；`timeline.mismatch_intervals`
  逐段列出，就是真机上该重点复核的时间点。
- **第三角已可复现**：`--engine compare` + `tools\compare_recording.bat` 把录屏对照
  做成命令。用基线导出的参考画面合成一张「理想录屏」做端到端探针，1 帧 466 个
  参与比对的像素逐通道一致（`identical`），ffmpeg 抽帧 / PNG 读回 / bbox 裁剪 /
  差异热图全链路已验证。

边界与简化（第二版后剩下的部分）：

- 解码器仍不模拟 OS 级呈现（vsync 对齐、硬件 overlay 缩放、播放器色彩管理），
  这些由 `compare` 用真实录屏覆盖；`--nominal` 可用来区分「时间轴标注口径」与
  「硬解呈现」两类差异。
- `need` 按 WDS 窗口面积算合成耗时，比 `sup_retime` 的整屏保守上界更精确；
  两者口径不同，报告里的 `late` 是「按窗口面积」的判定，不是整屏上界判定。
- `compare` 的默认容差（`--cmp-tol 12` / 占比 0.02 / 内缩 1px / `min_alpha 200`）
  是为吸收播放器 YCbCr→RGB、缩放与抗锯齿噪声；系统性色偏应查播放器色彩设置，
  不应靠无限放宽容差掩盖。
- 源头对拍按事件序号对齐，PTS 差只作参考；导出的参考 PNG 是标准 BT.709/601
  反变换的 8bit RGBA，仅供录屏对照，不参与软基准判据。

## SUPer 高质量 PGS（可选）

`cubicibo/SUPer`（GPL-3.0-or-later）是完整的 HDMV PGS 编码器，吃 BDN XML + PNG，
生成**严格合规**、支持卡拉OK / 遮罩 / 淡入淡出 / 移动的 SUP。supfast 默认走
「逐段 avs2bdnxml + 拼接」，快，2026-11 起也做了段时序合规化；但分段输出仍会留下
窗口定义变化等结构性警告，SUPer 与内置 Rust 原生 PGS 才能产出零警告整流流。三者可以并存。

supfast **不随包分发 SUPer**：GPL 与约 100 MB 的 Python 运行时都不允许塞进这个
3 MB 的包。需要你自行安装，然后在「高级选项 → SUPer 高质量 PGS」打开开关、填路径。

### 安装（Windows 示例）

```bat
python -m venv H://DIY//SUPer-cli//venv
H://DIY//SUPer-cli//venv//Scripts//python.exe -m pip install "SUPer @ git+https://github.com/cubicibo/SUPer"
:: 再把仓库根目录的 supercli.py 放到 H://DIY//SUPer-cli//，同目录放一份 config.ini
:: （[SUPer] use_gpu=0 可避开没有 OpenCL 平台时的报错）
```

探测顺序：界面里填的路径 → `SUPFAST_SUPER_CMD` → exe 同目录 `SUPer\supercli.py` /
`supercli.py` → `%APPDATA%\supfast-desktop\supercli.py` → PATH。目标是 `.py` 时，
解释器按 `SUPFAST_SUPER_PY` → 脚本旁 `venv\Scripts\python.exe` → PATH 里的 python 找。
也可以直接填一个自己写的 `.bat` / `.cmd` 启动器，软件只负责把参数透传过去。

### 会产出什么

开启后收尾额外生成（**不覆盖**默认产物，PNG / `.xml` / `.sup` 保持原样）：

| 文件 | 说明 |
|---|---|
| `<字幕名>.super.xml` | 给 SUPer 的 BDN XML。拼段流相邻事件偶有 1 帧重叠，SUPer 要求严格单调，这份副本做了单调化平移；主 `.xml` 不动 |
| `<字幕名>.super.sup` | SUPer 编出的严格合规 PGS |
| `<字幕名>.super.log` | SUPer 的完整输出日志 |

两套 SUP 都会在日志页各跑一次合规自检，并给出一行对比。
实测 `test2.ass`：默认拼段流 0 错 1 警（时序已合规），**SUPer 那份 0 错 0 警、严格合规**。

### 已知限制

- Windows 上 SUPer 的多进程 worker 可能起不来（`Companion worker did not start.`），
  所以固定 `-t 1`；单线程编 129 个事件约 5 秒，够用。想试多线程设
  `SUPFAST_SUPER_THREADS`（1–10）。
- 默认用内置 Qtzr 量化器（`-q 0`），不依赖外部 pngquant / libimagequant。
- 编码耗时另计，不计入转换耗时统计；失败只写日志，不影响转换成功。

## 命令行（不进 GUI）

release 版是 windows 子系统、没有 stdout，所以结果写文件：

```bat
supfast.exe --engine analyze --ass test.ass --json out.json
supfast.exe --engine convert --ass test.ass --out out_dir --jobs 12 --json out.json
supfast.exe --engine verify  --sup test.sup --json verify.json
supfast.exe --engine convert --ass test.ass --native-pgs --json out.json
supfast.exe --engine convert --ass test.ass --super --super-cmd H://DIY//SUPer-cli//supercli.py --json out.json
:: 对已有的 BDN XML + 同目录 PNG 单独跑原生编码器（不重渲字幕）
supfast.exe --engine pgs --xml test2.xml --json pgs.json
supfast.exe --engine pgs --xml test2.xml --out test2.native.sup --vw 1920 --vh 1080 --json pgs.json
:: 关掉调色板渐变 / 帧间对象拆分做对照（--engine convert --native-pgs 同样支持）
supfast.exe --engine pgs --xml test2.xml --out test2.nofade.sup --no-palette-fade --json pgs.json
supfast.exe --engine pgs --xml test2.xml --out test2.nosplit.sup --no-object-split --json pgs.json
:: 对拍基准：参考流 vs 候选流 + 源 PNG 对拍 + 黄金帧清单（详见上一节）
supfast.exe --engine baseline --ref test2.sup --cand native=test2.native.sup --xml test2.xml --frames frames.json --json baseline.json
:: 或一键跑四组样本
tools\baseline.bat
```

- `analyze` 输出 events / frames / segs / sta / dyn / work / ratio / w / h，以及 `bands`（全片时间轴）与 `timeline`。
  `warnings` 只放会影响结果的问题（ASS 的 Timer 与帧率不符、切段数超上限）；
  `notes` 是纯信息（全片静态、多少帧含动态标签），界面上记一次日志，不弹窗。
- `convert` 输出 pngs / secs / speed / epochs / failed / sup_size / verify（自检报告）/
  super（SUPer 重编码报告）/ native（Rust 原生 PGS 报告），未开启的后端为 `null`。
- `verify` 输出 ok / compliant / errors / warnings / stats / lines；`lines` 就是日志页显示的那几行。
- `pgs` 输出 bytes / display_sets / epochs / pcs / wds / pds / ods / end / objects_reused /
  palette_updates / fade_chains / fade_frames / split_chains / split_frames /
  split_objects_reused / lead_ticks / decode_upper_ticks / wds_dropped，外加对产物的
  `verify` 自检报告。`lead_ticks` 是段 DTS 提前量（按 `wipe + 一帧` 取，1080p 为 9587）；
  `decode_upper_ticks` 是按 Rd 估算的理论最坏解码时间，可能大于前者 —— 这是 PGS 的
  display set buffering 模型。`wds_dropped` 是因「相邻 PTS 间隔不足窗口擦除时间」
  而省略 WDS 并降级为 Normal Case 的显示集数。
  `--vw`/`--vh` 省略时按 XML 的 VideoFormat 推断；`--no-palette-fade` 关闭调色板渐变、
  `--no-object-split` 关闭帧间对象拆分。`verify` 的 `stats` 里另有 `palette_updates` /
  `pu_cobj_changed` / `pu_pds_version_bad` / `multi_obj_ds` / `max_cobjs` / `cobj_dup` /
  `cobj_too_many`。
- `baseline` 输出 `reference` / `streams`（流级统计 + 合规简报）、`golden`（黄金帧清单）、
  `pairs[]`（`content` 内容轨 + `timeline` 时间轴轨）、`source.checks[]`（源头对拍）、
  `summary.lines`。`content.worst` 列差异最大的帧，`timeline.mismatch_intervals` 列
  真机该重点复核的时间段；`--cand` 可重复，格式 `名字=路径`。

## 编译

需要 Rust（`x86_64-pc-windows-gnu` 目标）、Python（生成图标）和一份 `avs2bdnxml` + `VSFilterMod` 的 `mod\` 目录。
直接跑 `build.bat`：设 PATH → `cargo build --release` → 把 exe、`WebView2Loader.dll`、`mod\` 组装进 `dist\`。
除 Tauri 外只多一个纯 Rust 的 `flate2`（`rust_backend` / miniz_oxide，用来解压 PNG 的 zlib 数据），
无 C 库、无 DLL，交叉编译不受影响。

发布目录约 7.7 MB，绿色解压即用：

```
dist\
├─ supfast.exe            4.3 MB
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
├─ src\pgs.rs                  原生 PGS 编码器：PNG/RLE → epoch 规划 → 调色板渐变 / 帧间对象拆分 → 段序列化
├─ src\pgsdec.rs              PGS 软件解码器（播放器模型）：WDS 裁剪 / cropped / DTS 缓冲 / present_pts
├─ src\baseline.rs            对拍基准：内容轨 + 名义/有效双时间轴 + decode_risk + 黄金帧清单
├─ src\pngutil.rs             极简 PNG 读写（8bit RGB/RGBA，flate2），基准与真机对照共用
├─ src\compare.rs             真机录屏对照：ffmpeg 抽帧 → bbox 裁剪 → 逐像素比对 + 差异热图
├─ src\verify.rs               SUP 合规自检（零依赖，含 palette update / 多对象显示集检查）
├─ src\superenc.rs             可选 SUPer 外部调用
├─ src\main.rs                 IPC 路由 + 状态持久化 + 命令行入口
├─ ui\index.html               界面（单文件 HTML + CSS + JS）
├─ tools\baseline.bat / .ps1   一键跑四组样本的对拍基准（结果进 tools\baseline-results\）
├─ tools\compare_recording.bat / .ps1  真机录屏对照一键脚本（结果进 tools\compare-results\）
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

## 打包发布

`pack-release.bat` 把 `dist\` 加上 `packaging\release-readme.txt` 打成一个 zip：

```bat
build.bat          :: 先编译出 dist\
pack-release.bat   :: 产出 release\supfast-1.0.1-win64.zip（约 3.1 MB）
```

解压后是一个自成一体的目录：`supfast.exe`、`WebView2Loader.dll`、`mod\`，以及给最终用户的 `README.txt`
（系统要求、首次使用、数据位置、目录说明）。

**这个包天然是「全新默认状态」**，因为用户数据不在程序目录里：

```
设置 / 最近 / 历史  →  %APPDATA%\supfast-desktop\state.json
```

`state.json` 由程序在第一次改动设置时才创建，`dist\` 里永远不会出现它 —— 脚本里还加了一道检查，
万一手滑把它复制进 `dist\`，打包会直接失败。所以谁解压这个包，谁看到的就是空文件行、
空的「最近」、空的历史、默认参数（23.976 / 1080p / 8 路）。

想在本机模拟「新用户第一次启动」，不必删自己的状态，换个 APPDATA 就行：

```bat
set APPDATA=%TEMP%\supfast-fresh
dist\supfast.exe
```

期望看到：文件行「未选择文件 · 拖入 .ass 或 .ssa」、最近 0 条、历史 0 条、输出目录空、
`开始转换` 灰色、切段计划「尚未分析」。

### 发 Release

发版由 GitHub Actions 完成：打 `v*` tag 时，`.github/workflows/release.yml` 把
`packaging\artifacts\` 里的 zip 发成 Release，正文取 `packaging\release-notes-<版本>.md`。

```bat
:: 1. 出包（版本号在 pack-release.bat 顶部改）
build.bat
pack-release.bat

:: 2. 放进版本库，只留这一个 zip（workflow 会检查数量必须为 1）
del packaging\artifacts\*.zip
copy release\supfast-<版本>-win64.zip packaging\artifacts\

:: 3. 写一份 release 正文
::    packaging\release-notes-<版本>.md

:: 4. 提交 + 打 tag（tag 即触发发布）
git add -A && git commit -m "release: <版本>"
git push
git tag -a v<版本> -m "supfast <版本>" && git push origin v<版本>
```

包里有 `mod\` 的二进制（`avs2bdnxml.exe`、`VSFilterMod.dll`），runner 上没法现编，
所以 zip 随仓库走。它 3.1 MB，每个版本一份，记得删掉旧版本的那份。

也可以在 Actions 页面用 `workflow_dispatch` 手动触发，填一个已存在的 tag。

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
- **SUPer 不随包分发**：可选的「高质量 PGS」后端需要自行安装 GPL-3.0 的 SUPer，
  见 [SUPer 高质量 PGS（可选）](#super-高质量-pgs可选)。内置的
  [Rust 原生 PGS](#rust-原生-pgs内置--推荐) 不需要它，已带无损的调色板渐变与帧间对象拆分。
- **段时序分歧已解决，两条工具链统一**：早期把 PDS / END 的 DTS 写 0（沿用拼段
  工具链惯例），SUPer 的 `check_pts_dts_sanity` 会判 PTS-DTS out of spec。现在
  `sup_retime`（默认拼段流）与 `src/pgs.rs`（原生整流流）都用同一套模板：同一显示集
  所有段共用一个 `DTS = PTS − lead`、PDS / END 的 PTS 也取该值，提前量收紧到
  `(wipe, 2·wipe]`（1080p 为 9587）。四个原生样本 SUPer `is_compliant` 均为
  `(True, 0)`；test2 / drama 默认流 `check_pts_dts_sanity` 也都由 `False` 变 `True`。
  详见 [段时序合规](#段时序合规与-super-verifier-对拍)。
- **默认拼段流仍不是「严格合规」的整流流**：时序已合规、硬解模型下 late 显示集
  从 174 / 2430 降到 0，但 `avs2bdnxml` 分段输出本身还有结构性错误 —— `test2` 上
  SUPer `is_compliant=(False, 0)`，失败项是 `Window change mid-epoch` /
  `Object-slot dimensions mismatch` 等，内置自检表现为 1 个 `WDS_CHANGED` 警告。
  另外受 PGS 硬约束「新 epoch 的 DTS 必须晚于上一 epoch 最后呈现」，密集连续段会
  被整段推后，源头 PTS 相对 BDN `InTC` 仍有最大 0.195 s（test2）/ 0.151 s（drama）
  的残留平移。需要零警告、零平移的整流流请勾
  [Rust 原生 PGS](#rust-原生-pgs内置--推荐)，对照数据见
  [对拍基准](#真机播放验证前置对拍基准)。
- 无边框窗口用 `data-tauri-drag-region` 拖动，**双击标题栏不会最大化**，用右上角按钮。
- 目标机器需要 WebView2 运行时（Win10 1803+ 与 Win11 自带）。
- 右键菜单是关掉的：WebView2 默认会弹「重新加载 / 另存为 / 打印 / 检查」，
  这里用 `SetAreDefaultContextMenusEnabled(false)` 关掉，页面再兜一层 `contextmenu`。
  代价是输入框里右键粘贴没了，`Ctrl+V` 照常可用。
- **对拍基准是软解码模型，不是真机**：第二版已补 WDS 窗口裁剪、`cropped` 对象、
  DTS 缓冲 / PTS 呈现与 RX / object buffer 峰值，但仍不模拟 OS 级呈现（vsync 对齐、
  硬件 overlay 缩放、播放器色彩管理）。它验证「同一时刻屏幕上的像素是否一致」，
  不能替代真机 / 硬解播放验证；第三角请用 `--engine compare` 拿录屏核对
  （见 [对拍基准](#真机播放验证前置对拍基准)）。
