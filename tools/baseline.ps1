<#
.SYNOPSIS
  supfast 真机播放验证前置：SUP 对拍基准（跨流画面 + 源头 PNG + 黄金帧清单）。

.DESCRIPTION
  对每组样本调用 `supfast --engine baseline`：
    1. 用 PGS 软件解码器把每条 SUP 还原成「显示集 PTS → 屏幕画面」；
    2. 在两流所有 PTS 的并集上采样，逐像素比较可见画面（PGS 原生 YCrCb+Alpha 域）；
    3. 可选 --xml：把 BDN 源 PNG 组合成期望画面，按事件序号与各流对拍；
    4. 导出黄金帧清单（每帧 PTS / hash / bbox）与参考画面 PNG，供真机录屏对照。

  判据：identical（两轨都一致）/ content_ok_time_shift（内容无损、时间轴有平移）/
        equivalent（内容仅颜色舍入差异）/ decode_risk（存在 late 显示集或缓冲超限）/
        divergent（内容帧有实质差异）。

  脚本不重新渲染字幕，直接用 test\ 下已有的 SUP + XML + PNG，因此结果可复现、
  不受 avs2bdnxml 环境影响。要重产物请先跑 convert / --engine pgs。
  test2 / drama 的参考流是「默认流时序合规化」后的新默认产物 `<名>.retime.sup`
  （`supfast.exe --engine convert ...` 生成 `<名>\<名>.sup` 后重命名即可），
  同目录的 `<名>.sup` 是合规化前的旧默认流，作为 `default-old` 候选做 A/B。

.PARAMETER Exe
  supfast.exe 路径，默认 <项目根>\dist\supfast.exe（先跑 build.bat）。

.PARAMETER TestRoot
  样本根目录，默认 <项目根>\..\test。

.PARAMETER ResultsDir
  结果落盘目录，默认 tools\baseline-results。

.PARAMETER Samples
  要跑的样本，默认 fade,split,test2,drama。

.PARAMETER MaxPng
  每组最多导出几张参考画面 PNG，默认 8；-NoPng 完全不导出。

.PARAMETER Fps / Tol / ColorTol
  帧率（默认 23.976）、时间轴容差帧数（默认 1）、逐通道颜色容差（默认 2）。
#>
[CmdletBinding()]
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\dist\supfast.exe'),
    [string]$TestRoot = (Join-Path $PSScriptRoot '..\..\test'),
    [string]$ResultsDir = (Join-Path $PSScriptRoot 'baseline-results'),
    [string[]]$Samples = @('fade', 'split', 'test2', 'drama'),
    [int]$MaxPng = 8,
    [switch]$NoPng,
    [double]$Fps = 23.976,
    [int]$Tol = 1,
    [int]$ColorTol = 2
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path $Exe).Path
$root = (Resolve-Path $TestRoot).Path
New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null
$results = (Resolve-Path $ResultsDir).Path
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Get-SampleDef([string]$name) {
    switch ($name) {
        'fade' {
            return @{
                Ref = Join-Path $root 'pgs_fade\fade.off.sup'
                Xml = Join-Path $root 'pgs_fade\fade.xml'
                Cands = @(
                    @{ N = 'grad-on';    P = Join-Path $root 'pgs_fade\fade.sup' },
                    @{ N = 'grad-final'; P = Join-Path $root 'pgs_fade\fade.final.sup' }
                )
            }
        }
        'split' {
            return @{
                Ref = Join-Path $root 'pgs_split\split.off.sup'
                Xml = Join-Path $root 'pgs_split\split.xml'
                Cands = @(
                    @{ N = 'split-on';    P = Join-Path $root 'pgs_split\split.sup' },
                    @{ N = 'split-final'; P = Join-Path $root 'pgs_split\split.fixed.sup' }
                )
            }
        }
        'test2' {
            return @{
                Ref = Join-Path $root 'test2\test2.retime.sup'
                Xml = Join-Path $root 'test2\test2.xml'
                Cands = @(
                    @{ N = 'native-final'; P = Join-Path $root 'test2\test2.fixed.sup' },
                    @{ N = 'native-stage1'; P = Join-Path $root 'pgs_e2e\test2\test2.native.sup' },
                    @{ N = 'avs2bdnxml-ref'; P = Join-Path $root 'out_ref3\ref3.sup' },
                    @{ N = 'default-old'; P = Join-Path $root 'test2\test2.sup' }
                )
            }
        }
        'drama' {
            return @{
                Ref = Join-Path $root 'drama\drama.retime.sup'
                Xml = Join-Path $root 'drama\drama.xml'
                Cands = @(
                    @{ N = 'native-final';  P = Join-Path $root 'drama\drama.timing.sup' },
                    @{ N = 'native-stage1'; P = Join-Path $root 'drama\drama.native.sup' },
                    @{ N = 'default-old';   P = Join-Path $root 'drama\drama.sup' }
                )
            }
        }
        default { throw "未知样本: $name" }
    }
}

$summary = @()
$summary += "# supfast 对拍基准 $stamp"
$summary += ''
$summary += '真机播放验证前置基准：源 PNG → SUP 解码画面 → 真机录屏（本工具覆盖前两角）。'
$summary += ''
$summary += "| 样本 | 参考流 | 候选 | 综合 | 内容轨 | 一致/等价/差异帧 | 时间轴轨 | 存在性 | 错位 | PTS 最大偏移 | 解码 | 有效轨 | 源头对拍 |"
$summary += '|---|---|---|---|---|---|---|---:|---:|---:|---|---|---|'
$paths = @()

foreach ($name in $Samples) {
    $def = Get-SampleDef $name
    $missing = @()
    if (-not (Test-Path $def.Ref)) { $missing += $def.Ref }
    if ($def.Xml -and -not (Test-Path $def.Xml)) { $missing += $def.Xml }
    foreach ($c in $def.Cands) { if (-not (Test-Path $c.P)) { $missing += $c.P } }
    if ($missing.Count -gt 0) {
        Write-Host "[SKIP] $name 缺文件: $($missing -join '; ')" -ForegroundColor Yellow
        continue
    }

    $report = Join-Path $results "${stamp}_${name}.json"
    $frames = Join-Path $results "${stamp}_${name}.frames.json"
    $pngDir = Join-Path $results "${stamp}_${name}.png"
    $a = @('--engine', 'baseline', '--ref', $def.Ref)
    foreach ($c in $def.Cands) { $a += @('--cand', "$($c.N)=$($c.P)") }
    if ($def.Xml) { $a += @('--xml', $def.Xml) }
    $a += @('--fps', "$Fps", '--tol', "$Tol", '--color-tol', "$ColorTol", '--frames', $frames)
    if (-not $NoPng) { $a += @('--png-dir', $pngDir, '--max-png', "$MaxPng") }
    $a += @('--json', $report)

    Write-Host "== $name ==" -ForegroundColor Cyan
    # windows 子系统 exe 不附着控制台，用 Start-Process -Wait 明确等它写完 --json
    $proc = Start-Process -FilePath $exePath -ArgumentList $a -Wait -PassThru
    if ($proc.ExitCode -ne 0) {
        Write-Host "[FAIL] $name 对拍失败 (exit $($proc.ExitCode))" -ForegroundColor Red
        continue
    }
    $r = Get-Content $report -Raw -Encoding UTF8 | ConvertFrom-Json
    foreach ($line in $r.summary.lines) { Write-Host "  $line" }

    $srcMap = @{}
    if ($r.source) {
        foreach ($c in $r.source.checks) { $srcMap[$c.name] = $c }
    }
    foreach ($p in $r.pairs) {
        $sc = $srcMap[$p.name]
        $pairSrc = if ($sc) { "$($sc.verdict) ($($sc.identical_frames)/$($sc.aligned))" } else { 'n/a' }
        $dec = if ($p.timeline.decode.risk) { '有风险' } else { '可行' }
        $summary += "| $name | $(Split-Path $def.Ref -Leaf) | $($p.name) | $($p.verdict) | $($p.content.verdict) | $($p.content.identical_frames)/$($p.content.equivalent_frames)/$($p.content.divergent_frames) | $($p.timeline.verdict) | $($p.timeline.presence_mismatch) | $($p.timeline.frame_mismatch) | $($p.timeline.pts_delta.max_ticks) | $dec | $($p.timeline.effective.verdict) | $pairSrc |"
    }
    $paths += "- $name 报告：$report"
    $paths += "- $name 黄金帧清单：$frames（$($r.golden.frame_count) 帧，参考画面 $($r.golden.png_count) 张）"
}

$summary += ''
$summary += '## 产物'
$summary += ''
$summary += $paths
$summary += ''
$summary += '## 判据说明'
$summary += ''
$summary += '- identical：内容轨与时间轴轨都完全一致。'
$summary += '- equivalent：内容轨只差 --color-tol 以内（YCbCr 舍入 / 量化噪声），时间轴一致。'
$summary += '- content_ok_time_shift：每一帧画面都无损，但两流的 PTS 有平移，或清屏时机不同'
$summary += '  （默认拼段流按段单调化，原生整流流按事件 out 补清屏）。这是真实的播放差异，'
$summary += '  真机验证时重点看报告 timeline.mismatch_intervals 列出的时间段。'
$summary += '- decode_risk：任一流存在 late 显示集（DTS + 解码耗时 > PTS）或 RX / object buffer 超限，'
$summary += '  名义时间轴已不足以描述真机行为，须看 timeline.effective 与 timeline.decode。'
$summary += '- divergent：内容帧有超过容差的实质差异，需要定位（看 content.worst）。'
$summary += ''
$summary += '解码列来自硬解模型：段按 DTS 进 RX buffer，RLE 解码受 Rd=16e6 px/s、清屏/合成受 Rc=32e6 px/s 限制，'
$summary += ' `present_pts = max(pts, dts + need)`；有效轨即基于 present_pts 重新采样的时间轴。'
$summary += ''
$summary += '源头对拍（--xml）按事件序号对齐，PTS 差只作参考：默认拼段流的 PTS 由 sup_retime'
$summary += ' 按分段原始 PCS PTS 重映射，并受「新 epoch 的 DTS 必须晚于上一 epoch 最后呈现」约束，'
$summary += ' 与 BDN InTC 仍可能有小幅平移，本来就不在同一基准上。'
$summary += ''
$summary += '真机对照：跑 tools\compare_recording.bat，传 --Frames <本轮 *.frames.json> --Recording <录屏>，'
$summary += ' 工具按 present_pts 用 ffmpeg 抽帧、裁到同一 bbox 逐像素比对，并导出差异热图与 JSON 判定。'

$mdPath = Join-Path $results "${stamp}_summary.md"
[System.IO.File]::WriteAllText($mdPath, ($summary -join "`r`n"), $utf8)
Write-Host ''
Write-Host "汇总: $mdPath" -ForegroundColor Green
