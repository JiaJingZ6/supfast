<#
  真机录屏对照：拿 baseline 导出的黄金帧清单，用 ffmpeg 从播放器录屏抽帧，
  裁到同一 bbox 后逐像素比对，产出 JSON 报告 + Markdown 摘要 + 差异热图。

  用法见 tools\compare_recording.bat 或 README「真机播放验证」章节。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Frames,
    [Parameter(Mandatory = $true)][string]$Recording,
    [string]$Ffmpeg = '',
    [string]$PngDir = '',
    [double]$OffsetMs = 0,
    [int]$Tol = 12,
    [double]$RatioTol = 0.02,
    [int]$Shrink = 1,
    [int]$MinAlpha = 200,
    [switch]$Nominal,
    [int]$MaxFrames = 0,
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'dist\supfast.exe'
if (-not (Test-Path $exe)) { throw "找不到 $exe，请先编译 release" }
if (-not (Test-Path $Frames)) { throw "黄金帧清单不存在：$Frames" }
if (-not (Test-Path $Recording)) { throw "录屏不存在：$Recording" }

$base = if ($OutDir -ne '') { $OutDir } else { Join-Path $PSScriptRoot 'compare-results' }
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$dir = Join-Path $base $stamp
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$report = Join-Path $dir 'report.json'
$shots = Join-Path $dir 'shots'
$md = Join-Path $dir 'summary.md'

$a = @(
    '--engine', 'compare',
    '--frames', $Frames,
    '--recording', $Recording,
    '--offset-ms', "$OffsetMs",
    '--cmp-tol', "$Tol",
    '--cmp-ratio-tol', "$RatioTol",
    '--shrink', "$Shrink",
    '--min-alpha', "$MinAlpha",
    '--max-frames', "$MaxFrames",
    '--json', $report,
    '--shots-dir', $shots
)
if ($Ffmpeg -ne '') { $a += @('--ffmpeg', $Ffmpeg) }
if ($PngDir -ne '') { $a += @('--png-dir', $PngDir) }
if ($Nominal) { $a += '--nominal' }

Write-Host "== 真机录屏对照 ==" -ForegroundColor Cyan
# release 版是 windows 子系统，不附着控制台；用 Start-Process -Wait 明确等它写完 --json
$argLine = ($a | ForEach-Object {
    $v = [string]$_
    if ($v -match '[\s"]') { '"' + $v.Replace('"', '\"') + '"' } else { $v }
}) -join ' '
$proc = Start-Process -FilePath $exe -ArgumentList $argLine -Wait -PassThru
if ($proc.ExitCode -ne 0) { throw "对照失败 (exit $($proc.ExitCode))" }
if (-not (Test-Path $report)) { throw "未生成报告：$report" }

$r = Get-Content $report -Raw -Encoding UTF8 | ConvertFrom-Json
$r.summary.lines | ForEach-Object { Write-Host "  $_" }

$lines = @()
$lines += "# 真机录屏对照 $stamp"
$lines += ''
$lines += "- 录屏：$($r.recording)"
$lines += "- 黄金帧清单：$($r.frames_json)"
$lines += "- 视频尺寸：$($r.params.video) · ffmpeg：$($r.params.ffmpeg)"
$lines += "- 抽帧基准：$(if ($r.params.nominal) { '名义 PTS' } else { '硬解 present_pts' }) · 偏移 $($r.params.offset_ms) ms"
$lines += "- 判据容差：逐通道 $($r.params.tol) / 超容差占比上限 $($r.params.ratio_tol) / bbox 内缩 $($r.params.shrink) px / 最低 alpha $($r.params.min_alpha)"
$lines += ''
$lines += "**结论：$($r.summary.verdict)** · 共 $($r.summary.frames) 帧 · 一致 $($r.summary.identical) / 等价 $($r.summary.equivalent) / 差异 $($r.summary.divergent) / 抽帧失败 $($r.summary.errors)"
$lines += ''
$lines += "| 帧 | 时刻(s) | bbox | 判定 | 比对像素 | 超容差 | 最大通道差 | 参考画面 | 录屏裁图 | 差异热图 |"
$lines += '|---|---:|---|---|---:|---:|---:|---|---|---|'
foreach ($f in $r.frames) {
    $bb = ($f.bbox -join ',')
    $d = $f.diff
    $lines += "| $($f.i) | $($f.time_s) | $bb | $($f.verdict) | $(if ($d) { $d.compared_pixels } else { 0 }) | $(if ($d) { $d.diff_over_tol } else { 0 }) | $(if ($d) { $d.max_channel_delta } else { 0 }) | $($f.ref_png) | $(Split-Path $f.shot_png -Leaf) | $(Split-Path $f.diff_png -Leaf) |"
}
$lines += ''
$lines += '## 判据说明'
$lines += ''
$lines += '- identical：所有参与比对的像素逐通道一致。'
$lines += '- equivalent：超容差像素占比 <= 上限（播放器色彩管理 / 缩放 / 抗锯齿噪声）。'
$lines += '- divergent：大块内容错、缺或颜色明显不对，需人工看差异热图。'
$lines += '- error：ffmpeg 在该时刻抽不到帧（录屏比 PTS 短或偏移没对齐）。'
$lines += ''
$lines += "差异热图：`shots\*.diff.png`（红越亮代表通道差越大）；录屏裁图：`shots\*.shot.png`。"
$lines += ''
$lines += '## 对齐提示'
$lines += ''
$lines += '1. 若所有帧都偏早/偏晚，调 `--OffsetMs`（录屏起点相对 SUP PTS 0 的毫秒偏移）。'
$lines += '2. 若只有边缘像素超容差，调大 `--Shrink` 或提高 `--MinAlpha`。'
$lines += '3. 若整体颜色系统性偏移（播放器用 BT.601 解码 BT.709），先确认播放器色彩设置，'
$lines += '   再考虑调大 `--Tol`；这种情况不应靠放宽容差掩盖。'
$lines += '4. `--Nominal` 可切回名义 PTS 抽帧，用来区分「时间轴口径」与「硬解呈现」两类差异。'

$lines | Set-Content -Path $md -Encoding UTF8
Write-Host "报告：$report" -ForegroundColor Green
Write-Host "摘要：$md" -ForegroundColor Green
Write-Host "截图：$shots" -ForegroundColor Green
if ($r.summary.verdict -eq 'divergent' -or $r.summary.errors -gt 0) {
    Write-Host "[需复核] 存在 divergent / 抽帧失败帧" -ForegroundColor Yellow
}
