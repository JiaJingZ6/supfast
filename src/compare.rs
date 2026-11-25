//! 真机录屏对照：黄金帧清单 → ffmpeg 抽帧 → bbox 裁剪 → 逐像素比对
//!
//! 软基准（`baseline`）回答「SUP 解码出来的画面是否一致」；本模块回答
//! 「播放器实际输出到屏幕上的画面是否一致」。流程：
//!
//! ```text
//! baseline 导出 *_frames.json + 参考画面 PNG（bbox 裁好的 RGBA）
//!   → 用 ffmpeg 在 present_pts 时刻从录屏抽一帧、缩放到源视频尺寸
//!   → 裁到同一 bbox，逐像素比 RGB（透明/半透明边缘可跳过）
//!   → 每帧判定 identical / equivalent / divergent + 差异热图
//! ```
//!
//! 播放器会做 YCbCr→RGB、色彩管理、缩放和抗锯齿，逐像素严格相等不现实，
//! 所以默认 `--cmp-tol 12` + `--cmp-ratio-tol 0.02` + `--shrink 1` +
//! `--min-alpha 200`，只把「大块内容错/缺」判成 divergent。

use crate::pngutil::{read_rgba_png, write_rgba_png};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const PGS_HZ: f64 = 90_000.0;

pub struct Options {
    pub frames: PathBuf,
    pub recording: PathBuf,
    pub ffmpeg: Option<String>,
    /// 参考 PNG 目录；留空则用帧清单里的 png_dir / 帧清单所在目录
    pub png_dir: Option<PathBuf>,
    /// 录屏时间轴相对 SUP PTS 0 的偏移（毫秒，可负）
    pub offset_ms: f64,
    /// 逐通道容差
    pub tol: u8,
    /// 超容差像素占比上限
    pub ratio_tol: f64,
    /// bbox 内缩像素（跳过抗锯齿边缘）
    pub shrink: u32,
    /// 只比较 alpha >= 该值的参考像素
    pub min_alpha: u8,
    /// 用名义 pts 而不是硬解 present_pts 抽帧
    pub nominal: bool,
    /// 0 = 全部有参考 PNG 的帧
    pub max_frames: usize,
    /// 保存录屏裁图与差异热图
    pub shots_dir: Option<PathBuf>,
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (a, b) = s.split_once(['x', 'X', '*'])
        .ok_or_else(|| format!("无法解析视频尺寸 '{}'，应为 1920x1080", s))?;
    let w: u32 = a.trim().parse().map_err(|_| format!("宽度无效 '{}'", a))?;
    let h: u32 = b.trim().parse().map_err(|_| format!("高度无效 '{}'", b))?;
    if w == 0 || h == 0 {
        return Err(format!("视频尺寸无效 '{}'", s));
    }
    Ok((w, h))
}

fn find_ffmpeg(given: &Option<String>) -> Result<String, String> {
    if let Some(g) = given {
        if !g.is_empty() {
            return Ok(g.clone());
        }
    }
    for c in ["ffmpeg", "ffmpeg.exe"] {
        if Command::new(c).arg("-version").stdout(Stdio::null()).stderr(Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false)
        {
            return Ok(c.to_string());
        }
    }
    for c in [r"H:\ffmpeg\bin\ffmpeg.exe", r"C:\ffmpeg\bin\ffmpeg.exe"] {
        if Path::new(c).is_file() {
            return Ok(c.to_string());
        }
    }
    Err("找不到 ffmpeg：请用 --ffmpeg <path> 指定，或把它加入 PATH".into())
}

/// 从录屏的 t 秒处抽一帧，缩放到 (w,h)，返回 RGB24 原始字节。
fn grab(ffmpeg: &str, rec: &Path, t: f64, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let tstr = format!("{:.6}", t.max(0.0));
    let vf = format!("scale={}:{}", w, h);
    let out = Command::new(ffmpeg)
        .arg("-hide_banner").arg("-loglevel").arg("error")
        .arg("-ss").arg(&tstr)
        .arg("-i").arg(rec)
        .arg("-frames:v").arg("1")
        .arg("-vf").arg(&vf)
        .arg("-pix_fmt").arg("rgb24")
        .arg("-f").arg("rawvideo")
        .arg("pipe:1")
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("启动 ffmpeg 失败（可用 --ffmpeg 指定）: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "ffmpeg 抽帧失败 @{}s：{}",
            tstr,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let need = w as usize * h as usize * 3;
    if out.stdout.len() < need {
        return Err(format!(
            "ffmpeg 只输出 {} 字节，需要 {}（录屏可能比该 PTS 短）",
            out.stdout.len(), need
        ));
    }
    Ok(out.stdout[..need].to_vec())
}

struct Diff {
    compared: usize,
    exact: usize,
    over_tol: usize,
    max_delta: u8,
    sum_abs: u64,
    sum_sq: u64,
}

fn diff_json(d: &Diff) -> Value {
    let mean = if d.compared > 0 {
        d.sum_abs as f64 / (d.compared * 3) as f64
    } else {
        0.0
    };
    let mse = if d.compared > 0 {
        d.sum_sq as f64 / (d.compared * 3) as f64
    } else {
        0.0
    };
    let psnr = if mse > 0.0 {
        json!((10.0 * (255.0f64 * 255.0 / mse).log10() * 100.0).round() / 100.0)
    } else {
        json!("inf")
    };
    json!({
        "compared_pixels": d.compared,
        "exact_pixels": d.exact,
        "diff_pixels": d.compared - d.exact,
        "diff_over_tol": d.over_tol,
        "over_tol_ratio": if d.compared > 0 {
            (d.over_tol as f64 / d.compared as f64 * 10000.0).round() / 10000.0
        } else { 0.0 },
        "max_channel_delta": d.max_delta,
        "mean_abs_delta": (mean * 10000.0).round() / 10000.0,
        "psnr_db": psnr,
    })
}

/// 参考画面（bbox 尺寸 RGBA）与录屏全帧（RGB24）比对。
/// 返回 (差异统计, 录屏 bbox 裁图, 差异热图)。
#[allow(clippy::too_many_arguments)]
fn compare_frame(
    ref_rgba: &[u8],
    shot: &[u8],
    w: u32,
    h: u32,
    bbox: [u32; 4],
    o: &Options,
) -> (Diff, Vec<u8>, Vec<u8>) {
    let [bx, by, fw, fh] = bbox;
    let mut crop = vec![0u8; (fw * fh * 4) as usize];
    let mut heat = vec![0u8; (fw * fh * 4) as usize];
    let mut d = Diff { compared: 0, exact: 0, over_tol: 0, max_delta: 0, sum_abs: 0, sum_sq: 0 };
    for y in 0..fh {
        for x in 0..fw {
            let q = ((y * fw + x) * 4) as usize;
            let sx = bx.saturating_add(x);
            let sy = by.saturating_add(y);
            if sx < w && sy < h {
                let sp = ((sy * w + sx) * 3) as usize;
                crop[q] = shot[sp];
                crop[q + 1] = shot[sp + 1];
                crop[q + 2] = shot[sp + 2];
                crop[q + 3] = 255;
            }
            let inside = x >= o.shrink && y >= o.shrink
                && x.saturating_add(o.shrink) < fw && y.saturating_add(o.shrink) < fh;
            if !inside || sx >= w || sy >= h || ref_rgba[q + 3] < o.min_alpha {
                continue;
            }
            let mut over = false;
            let mut diff = false;
            let mut mx = 0u8;
            for c in 0..3 {
                let delta = ((ref_rgba[q + c] as i32 - crop[q + c] as i32).abs()) as u8;
                if delta > 0 {
                    diff = true;
                }
                if delta > o.tol {
                    over = true;
                }
                mx = mx.max(delta);
                if delta > d.max_delta {
                    d.max_delta = delta;
                }
                d.sum_abs += delta as u64;
                d.sum_sq += (delta as u64) * (delta as u64);
            }
            d.compared += 1;
            if !diff {
                d.exact += 1;
            }
            if over {
                d.over_tol += 1;
            }
            let v = (mx as u32 * 4).min(255) as u8;
            heat[q] = v;
            heat[q + 3] = 255;
        }
    }
    (d, crop, heat)
}

pub fn run(o: &Options) -> Result<Value, String> {
    if !o.frames.is_file() {
        return Err(format!("黄金帧清单不存在：{}", o.frames.display()));
    }
    if !o.recording.is_file() {
        return Err(format!("录屏文件不存在：{}", o.recording.display()));
    }
    let doc: Value = serde_json::from_slice(&fs::read(&o.frames)
        .map_err(|e| format!("读帧清单失败: {}", e))?)
        .map_err(|e| format!("帧清单不是合法 JSON: {}", e))?;
    let (w, h) = parse_size(doc.get("video").and_then(|v| v.as_str()).unwrap_or(""))?;
    let ffmpeg = find_ffmpeg(&o.ffmpeg)?;

    // 参考 PNG 目录优先级：--png-dir > 帧清单里的 png_dir > 帧清单所在目录
    let png_dir = o.png_dir.clone().unwrap_or_else(|| {
        doc.get("png_dir").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| o.frames.parent().unwrap_or(Path::new(".")).to_path_buf())
    });

    let all: Vec<&Value> = doc.get("frames").and_then(|v| v.as_array())
        .map(|a| a.iter().filter(|f| f.get("png").and_then(|p| p.as_str()).is_some()).collect())
        .unwrap_or_default();
    let take = if o.max_frames == 0 { all.len() } else { all.len().min(o.max_frames) };
    if let Some(dir) = &o.shots_dir {
        fs::create_dir_all(dir).map_err(|e| format!("建截图目录失败: {}", e))?;
    }

    let mut results: Vec<Value> = Vec::new();
    let mut identical = 0usize;
    let mut equivalent = 0usize;
    let mut divergent = 0usize;
    let mut errors = 0usize;
    let mut agg = Diff { compared: 0, exact: 0, over_tol: 0, max_delta: 0, sum_abs: 0, sum_sq: 0 };

    for f in all.iter().take(take) {
        let i = f.get("i").and_then(|v| v.as_u64()).unwrap_or(0);
        let pts = f.get("pts").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let present = f.get("present_pts").and_then(|v| v.as_u64()).unwrap_or(pts as u64) as u32;
        let key_pts = if o.nominal { pts } else { present };
        let t = key_pts as f64 / PGS_HZ - o.offset_ms / 1000.0;
        let png = f.get("png").and_then(|v| v.as_str()).unwrap_or("");
        let bbox_val = f.get("bbox").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let bbox = [
            bbox_val.get(0).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            bbox_val.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            bbox_val.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            bbox_val.get(3).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        ];
        let mut item = json!({
            "i": i,
            "pts": pts,
            "present_pts": present,
            "key_pts": key_pts,
            "time_s": (t * 1000.0).round() / 1000.0,
            "bbox": bbox,
            "ref_png": png,
        });
        let run_one = || -> Result<(Diff, Vec<u8>, Vec<u8>, u32, u32), String> {
            if t < 0.0 {
                return Err("抽帧时刻为负：请调大 --offset-ms".into());
            }
            let ref_path = png_dir.join(png);
            let (bw, bh, ref_rgba) = read_rgba_png(&ref_path)?;
            if bw != bbox[2] || bh != bbox[3] {
                return Err(format!(
                    "参考画面 {}x{} 与 bbox {}x{} 不符：{}",
                    bw, bh, bbox[2], bbox[3], ref_path.display()
                ));
            }
            let shot = grab(&ffmpeg, &o.recording, t, w, h)?;
            let (d, crop, heat) = compare_frame(&ref_rgba, &shot, w, h, bbox, o);
            Ok((d, crop, heat, bw, bh))
        };
        match run_one() {
            Ok((d, crop, heat, bw, bh)) => {
                let ratio = if d.compared > 0 { d.over_tol as f64 / d.compared as f64 } else { 0.0 };
                let verdict = if d.over_tol == 0 && d.exact == d.compared {
                    "identical"
                } else if ratio <= o.ratio_tol {
                    "equivalent"
                } else {
                    "divergent"
                };
                match verdict {
                    "identical" => identical += 1,
                    "equivalent" => equivalent += 1,
                    _ => divergent += 1,
                }
                agg.compared += d.compared;
                agg.exact += d.exact;
                agg.over_tol += d.over_tol;
                agg.max_delta = agg.max_delta.max(d.max_delta);
                agg.sum_abs += d.sum_abs;
                agg.sum_sq += d.sum_sq;
                item["verdict"] = json!(verdict);
                item["diff"] = diff_json(&d);
                if let Some(dir) = &o.shots_dir {
                    let stem = Path::new(png).file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| format!("{:06}", i));
                    let sp = dir.join(format!("{}.shot.png", stem));
                    let hp = dir.join(format!("{}.diff.png", stem));
                    write_rgba_png(&sp, bw, bh, &crop)?;
                    write_rgba_png(&hp, bw, bh, &heat)?;
                    item["shot_png"] = json!(sp.to_string_lossy().to_string());
                    item["diff_png"] = json!(hp.to_string_lossy().to_string());
                }
            }
            Err(e) => {
                errors += 1;
                item["verdict"] = json!("error");
                item["error"] = json!(e);
            }
        }
        results.push(item);
    }

    let verdict = if errors > 0 && results.len() == errors {
        "error"
    } else if divergent > 0 {
        "divergent"
    } else if equivalent > 0 || errors > 0 {
        "equivalent"
    } else {
        "identical"
    };
    let lines = vec![
        format!("真机录屏对照 · {} · 参考 {} 帧 · 一致 {} / 等价 {} / 差异 {} / 抽帧失败 {}",
            o.recording.display(), results.len(), identical, equivalent, divergent, errors),
        format!("超容差像素 {} / 比对 {} · 最大通道差 {} · 容差 {} / 占比上限 {}",
            agg.over_tol, agg.compared, agg.max_delta, o.tol, o.ratio_tol),
        format!("抽帧基准 {}（{}）· 偏移 {} ms · bbox 内缩 {} px · 最低 alpha {}",
            if o.nominal { "名义 PTS" } else { "硬解 present_pts" },
            ffmpeg, o.offset_ms, o.shrink, o.min_alpha),
    ];

    Ok(json!({
        "ok": true,
        "engine": "supfast compare 1",
        "params": {
            "ffmpeg": ffmpeg,
            "video": format!("{}x{}", w, h),
            "offset_ms": o.offset_ms,
            "tol": o.tol,
            "ratio_tol": o.ratio_tol,
            "shrink": o.shrink,
            "min_alpha": o.min_alpha,
            "nominal": o.nominal,
            "max_frames": o.max_frames,
            "png_dir": png_dir.to_string_lossy().to_string(),
            "shots_dir": o.shots_dir.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        },
        "frames_json": o.frames.to_string_lossy().to_string(),
        "recording": o.recording.to_string_lossy().to_string(),
        "frames": results,
        "summary": {
            "verdict": verdict,
            "frames": results.len(),
            "identical": identical,
            "equivalent": equivalent,
            "divergent": divergent,
            "errors": errors,
            "pixels": diff_json(&agg),
            "lines": lines,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_video_size() {
        assert_eq!(parse_size("1920x1080").unwrap(), (1920, 1080));
        assert_eq!(parse_size("720X576").unwrap(), (720, 576));
        assert!(parse_size("abc").is_err());
        assert!(parse_size("0x10").is_err());
    }

    fn opts(tol: u8) -> Options {
        Options {
            frames: PathBuf::new(), recording: PathBuf::new(), ffmpeg: None,
            png_dir: None, offset_ms: 0.0, tol, ratio_tol: 0.02, shrink: 0,
            min_alpha: 200, nominal: false, max_frames: 0, shots_dir: None,
        }
    }

    #[test]
    fn compare_frame_matches_identical_pixels() {
        // 2x2 参考，1 个不透明红像素 + 1 个半透明 + 2 个全透明
        let mut r = vec![0u8; 16];
        r[0] = 200; r[3] = 255;
        r[4] = 10; r[7] = 128; // 半透明，低于 min_alpha，应跳过
        // 录屏同尺寸，全部刷成和参考一致
        let mut s = vec![0u8; 2 * 2 * 3];
        s[0] = 200;
        let (d, crop, heat) = compare_frame(&r, &s, 2, 2, [0, 0, 2, 2], &opts(0));
        assert_eq!(d.compared, 1);
        assert_eq!(d.exact, 1);
        assert_eq!(d.over_tol, 0);
        assert_eq!(crop[0], 200);
        assert_eq!(heat[3], 255);
    }

    #[test]
    fn compare_frame_flags_real_difference() {
        let mut r = vec![0u8; 4];
        r[0] = 200; r[3] = 255;
        let s = vec![100u8, 0, 0];
        let (d, _, heat) = compare_frame(&r, &s, 1, 1, [0, 0, 1, 1], &opts(5));
        assert_eq!(d.compared, 1);
        assert_eq!(d.exact, 0);
        assert_eq!(d.over_tol, 1);
        assert_eq!(d.max_delta, 100);
        assert_eq!(heat[0], 255);
    }

    #[test]
    fn shrink_and_min_alpha_skip_edges() {
        let mut r = vec![0u8; 4];
        r[0] = 200; r[3] = 255;
        let s = vec![0u8, 0, 0];
        let mut o = opts(0);
        o.shrink = 1; // 1x1 画面内缩 1px 后没有可比较像素
        let (d, _, _) = compare_frame(&r, &s, 1, 1, [0, 0, 1, 1], &o);
        assert_eq!(d.compared, 0);
    }
}
