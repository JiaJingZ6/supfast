//! 真机播放验证前置：SUP 对拍基准
//!
//! 三角关系：
//!
//! ```text
//!   源 PNG (BDN ground truth)  ←→  SUP 解码画面 (本工具 / 播放器模型)  ←→  真机播放输出
//!          ① --xml 源头对拍              ② 跨流对拍                    ③ 留给真机录屏
//! ```
//!
//! 本工具把 ①② 做成可复现的基准，并导出「黄金帧清单 + 参考画面 PNG」，
//! 让 ③ 有明确的对照物。跨流对拍不比字节，只比**任意时刻屏幕上的画面**：
//! 把每条流还原成「显示集 PTS → 当前画面」，在两流所有 PTS 的并集上采样，
//! 每个采样点取各自「最后一个 pts ≤ t」的帧比较。这样编码策略不同（epoch
//! 划分、对象复用、清屏显示集数量）也能公平比较，还能把「一边已经清屏、
//! 另一边还挂着字幕」这种真实的播放差异单独报出来。
//!
//! 对拍分两条轨道：内容轨（可见帧按序号对齐，验证「每张图有没有编对」）与
//! 时间轴轨（在两流 PTS 并集上采样，验证「同一时刻挂着哪张画面」）。
//! 综合判据：identical（两轨都一致）/ content_ok_time_shift（内容无损但时间轴
//! 有平移或清屏时机差异，真机验证重点）/ equivalent（内容仅有颜色舍入级差异）/
//! divergent（内容帧有实质差异）。
//!
//! 画面在 PGS 原生域 `[Y, Cr, Cb, Alpha]` 比对，不做 YCbCr→RGB 往返，
//! 透明像素的 RGB 不参与比较（PGS 里没有意义）。

use crate::pgsdec;
use crate::verify;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_WORST: usize = 20;
const MAX_INTERVALS: usize = 40;

pub struct Candidate {
    pub name: String,
    pub path: PathBuf,
}

pub struct Options {
    pub reference: PathBuf,
    pub candidates: Vec<Candidate>,
    pub xml: Option<PathBuf>,
    pub fps: f64,
    /// 时间轴对齐容差（帧数）
    pub tol_frames: u32,
    /// 逐通道容差，用于 equivalent 判定
    pub color_tol: u8,
    /// 超容差像素占比上限
    pub ratio_tol: f64,
    pub frames_out: Option<PathBuf>,
    pub png_dir: Option<PathBuf>,
    pub max_png: usize,
}

fn clock(t: u32) -> String {
    let s = t as f64 / pgsdec::PGS_HZ;
    format!(
        "{:02}:{:02}:{:06.3}",
        (s / 3600.0) as u32,
        (s / 60.0) as u32 % 60,
        s % 60.0
    )
}

fn r4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

fn abs_sub(a: u32, b: u32) -> u32 {
    if a >= b { a - b } else { b - a }
}

fn verify_brief(path: &Path) -> Value {
    match verify::verify_sup_file(path) {
        Ok(v) => json!({
            "ok": v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false),
            "compliant": v.get("compliant").and_then(|x| x.as_bool()).unwrap_or(false),
            "errors": v.get("errors").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0),
            "warnings": v.get("warnings").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0),
        }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

fn stream_brief(d: &pgsdec::Decoded) -> Value {
    let first = d.frames.first().map(|f| f.pts).unwrap_or(0);
    let last = d.frames.last().map(|f| f.pts).unwrap_or(0);
    json!({
        "path": d.path,
        "bytes": d.bytes,
        "video": format!("{}x{}", d.width, d.height),
        "display_sets": d.display_sets,
        "frames": d.frames.len(),
        "epochs": d.epochs,
        "palette_updates": d.palette_updates,
        "clear_frames": d.frames.iter().filter(|f| f.is_clear()).count(),
        "windows": d.windows.len(),
        "first_pts": first,
        "last_pts": last,
        "duration_s": r4((last.saturating_sub(first)) as f64 / pgsdec::PGS_HZ),
        "incomplete_ds": d.incomplete_ds,
        "undefined_refs": d.frames.iter().map(|f| f.undefined_refs).sum::<usize>(),
        "undefined_pal": d.frames.iter().map(|f| f.undefined_pal).sum::<usize>(),
    })
}

// ---------------------------------------------------------------------------
// PNG 写出（给真机录屏对照用的参考画面，8bit RGBA，无第三方图像库）
// ---------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
        }
    }
    !c
}

fn chunk(out: &mut Vec<u8>, typ: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(data);
    let mut c = Vec::with_capacity(4 + data.len());
    c.extend_from_slice(typ);
    c.extend_from_slice(data);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}

fn write_rgba_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    if w == 0 || h == 0 {
        return Err("空画面不导出 PNG".into());
    }
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0u8); // filter: None
        let o = y * w as usize * 4;
        raw.extend_from_slice(&rgba[o..o + w as usize * 4]);
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(6));
    enc.write_all(&raw).map_err(|e| format!("zlib 压缩失败: {}", e))?;
    let idat = enc.finish().map_err(|e| format!("zlib 收尾失败: {}", e))?;
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8bit RGBA, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    fs::write(path, out).map_err(|e| format!("写 PNG 失败 {}: {}", path.display(), e))
}

/// 黄金帧清单：每个显示集一行，含 PTS / 帧号 / hash / bbox / 不透明像素数。
/// 给了 `--png-dir` 就把前 `max_png` 个非清屏帧裁到 bbox 导出参考画面。
fn golden(
    d: &pgsdec::Decoded,
    o: &Options,
) -> Result<Value, String> {
    let mut want: HashSet<usize> = HashSet::new();
    if o.png_dir.is_some() {
        for (i, f) in d.frames.iter().enumerate() {
            if !f.is_clear() && want.len() < o.max_png {
                want.insert(i);
            }
        }
    }
    let px: HashMap<usize, Vec<(u32, [u8; 4])>> = if want.is_empty() {
        HashMap::new()
    } else {
        pgsdec::decode(Path::new(&d.path), Some(&want))?
            .pixels
            .into_iter()
            .collect()
    };
    let png_dir = o.png_dir.as_ref();
    if let Some(dir) = png_dir {
        fs::create_dir_all(dir).map_err(|e| format!("建 PNG 目录失败: {}", e))?;
    }
    let bt709 = d.height >= 720;
    let stride = d.width.max(1);
    let mut frames: Vec<Value> = Vec::with_capacity(d.frames.len());
    for (i, f) in d.frames.iter().enumerate() {
        let mut item = json!({
            "i": i,
            "pts": f.pts,
            "time": clock(f.pts),
            "frame_no": (f.pts as f64 / (pgsdec::PGS_HZ / o.fps.max(1.0))).round() as u64,
            "kind": f.kind(),
            "state": format!("0x{:02X}", f.state),
            "pal_upd": f.pal_upd,
            "nref": f.nref,
            "comp_no": f.comp_no,
            "hash": format!("{:016x}", f.hash),
            "opaque_pixels": f.opaque,
            "bbox": f.bbox,
            "png": Value::Null,
        });
        if let (Some(dir), Some(p)) = (png_dir, px.get(&i)) {
            let [bx, by, bw, bh] = f.bbox;
            if bw > 0 && bh > 0 {
                let mut rgba = vec![0u8; (bw * bh * 4) as usize];
                for &(k, col) in p {
                    let x = k % stride;
                    let y = k / stride;
                    if x >= bx && y >= by && x < bx + bw && y < by + bh {
                        let q = (((y - by) * bw + (x - bx)) * 4) as usize;
                        let c = pgsdec::ycbcra_to_rgba(col, bt709);
                        rgba[q..q + 4].copy_from_slice(&c);
                    }
                }
                let name = format!("{:06}_{:010}.png", i, f.pts);
                write_rgba_png(&dir.join(&name), bw, bh, &rgba)?;
                item["png"] = json!(name);
            }
        }
        frames.push(item);
    }
    let out = json!({
        "source": d.path,
        "video": format!("{}x{}", d.width, d.height),
        "fps": o.fps,
        "frames": frames,
        "png_count": px.len(),
    });
    if let Some(p) = &o.frames_out {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(p, serde_json::to_string_pretty(&out).unwrap())
            .map_err(|e| format!("写帧清单失败 {}: {}", p.display(), e))?;
    }
    Ok(json!({
        "frames_path": o.frames_out.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        "png_dir": o.png_dir.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        "frame_count": frames.len(),
        "png_count": px.len(),
        "frames": frames,
    }))
}

// ---------------------------------------------------------------------------
// 像素比对
// ---------------------------------------------------------------------------

struct PxDiff {
    total: usize,
    exact: usize,
    over_tol: usize,
    max_delta: u8,
    sum_abs: u64,
    sum_sq: u64,
}

/// 两边都已按 key 升序、只含 alpha>0 的像素；缺省位置视为全透明。
/// 透明像素的颜色不参与比较，只有 alpha 差算数。
fn compare_px(a: &[(u32, [u8; 4])], b: &[(u32, [u8; 4])], color_tol: u8) -> PxDiff {
    let mut r = PxDiff { total: 0, exact: 0, over_tol: 0, max_delta: 0, sum_abs: 0, sum_sq: 0 };
    let mut i = 0usize;
    let mut j = 0usize;
    let zero = [0u8; 4];
    while i < a.len() || j < b.len() {
        let (ka, ca, kb, cb) = match (a.get(i), b.get(j)) {
            (Some(&(ka, ca)), Some(&(kb, cb))) => (ka, ca, kb, cb),
            (Some(&(ka, ca)), None) => (ka, ca, u32::MAX, zero),
            (None, Some(&(kb, cb))) => (u32::MAX, zero, kb, cb),
            (None, None) => break,
        };
        let (va, vb) = if ka == kb {
            i += 1;
            j += 1;
            (ca, cb)
        } else if ka < kb {
            i += 1;
            (ca, zero)
        } else {
            j += 1;
            (zero, cb)
        };
        // 透明像素的颜色无意义
        let na = if va[3] == 0 { zero } else { va };
        let nb = if vb[3] == 0 { zero } else { vb };
        let mut pixel_over = false;
        let mut pixel_diff = false;
        for c in 0..4 {
            let d = ((na[c] as i32 - nb[c] as i32).abs()) as u8;
            if d > 0 {
                pixel_diff = true;
            }
            if d > color_tol {
                pixel_over = true;
            }
            if d > r.max_delta {
                r.max_delta = d;
            }
            r.sum_abs += d as u64;
            r.sum_sq += (d as u64) * (d as u64);
        }
        r.total += 1;
        if !pixel_diff {
            r.exact += 1;
        }
        if pixel_over {
            r.over_tol += 1;
        }
    }
    r
}

fn diff_json(d: &PxDiff) -> Value {
    let mean = if d.total > 0 {
        d.sum_abs as f64 / (d.total * 4) as f64
    } else {
        0.0
    };
    let mse = if d.total > 0 {
        d.sum_sq as f64 / (d.total * 4) as f64
    } else {
        0.0
    };
    let psnr = if mse > 0.0 {
        json!((10.0 * (255.0f64 * 255.0 / mse).log10() * 100.0).round() / 100.0)
    } else {
        json!("inf")
    };
    json!({
        "total_pixels": d.total,
        "exact_pixels": d.exact,
        "diff_pixels": d.total - d.exact,
        "diff_over_tol": d.over_tol,
        "max_channel_delta": d.max_delta,
        "mean_abs_delta": (mean * 10000.0).round() / 10000.0,
        "psnr_db": psnr,
    })
}

/// 取「最后一个 pts <= t」的帧下标
fn frame_at(frames: &[pgsdec::FrameMeta], t: u32) -> Option<usize> {
    let mut lo = 0usize;
    let mut hi = frames.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if frames[mid].pts <= t {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 { None } else { Some(lo - 1) }
}

fn pixels_of(path: &Path, want: &HashSet<usize>) -> Result<HashMap<usize, Vec<(u32, [u8; 4])>>, String> {
    if want.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(pgsdec::decode(path, Some(want))?.pixels.into_iter().collect())
}

struct PairResult {
    value: Value,
    verdict: String,
}

fn compare_pair(
    refd: &pgsdec::Decoded,
    candd: &pgsdec::Decoded,
    name: &str,
    o: &Options,
    tol_ticks: u32,
) -> Result<PairResult, String> {
    // ---- 轨道 1：内容 -------------------------------------------------
    // 可见帧按序号对齐，回答「每张图有没有被编对」。不同工具链的整体 PTS
    // 平移（sup_retime 的段级单调化 vs 原生编码器的 mono_times）不影响这一轨。
    let va: Vec<usize> = refd.frames.iter().enumerate()
        .filter(|(_, f)| !f.is_clear()).map(|(i, _)| i).collect();
    let vb: Vec<usize> = candd.frames.iter().enumerate()
        .filter(|(_, f)| !f.is_clear()).map(|(i, _)| i).collect();
    let n = va.len().min(vb.len());
    let mut mismatched: Vec<usize> = Vec::new();
    let mut pts_delta_sum = 0u64;
    let mut pts_delta_max = 0u32;
    let mut pts_shifted = 0usize;
    for k in 0..n {
        let a = &refd.frames[va[k]];
        let b = &candd.frames[vb[k]];
        let d = abs_sub(a.pts, b.pts);
        pts_delta_sum += d as u64;
        pts_delta_max = pts_delta_max.max(d);
        if d > tol_ticks {
            pts_shifted += 1;
        }
        if a.hash != b.hash {
            mismatched.push(k);
        }
    }
    let content_identical = n.saturating_sub(mismatched.len());
    let mut agg = PxDiff { total: 0, exact: 0, over_tol: 0, max_delta: 0, sum_abs: 0, sum_sq: 0 };
    let mut worst: Vec<Value> = Vec::new();
    let mut divergent_frames = 0usize;
    let mut equivalent_frames = 0usize;
    let empty: Vec<(u32, [u8; 4])> = Vec::new();
    // 只对 hash 不同的帧二次解码取像素，且分批，避免整片流把内存吃满
    const CHUNK: usize = 256;
    for chunk in mismatched.chunks(CHUNK) {
        let mut ka: HashSet<usize> = HashSet::new();
        let mut kb: HashSet<usize> = HashSet::new();
        for &k in chunk {
            ka.insert(va[k]);
            kb.insert(vb[k]);
        }
        let pa = pixels_of(Path::new(&refd.path), &ka)?;
        let pb = pixels_of(Path::new(&candd.path), &kb)?;
        for &k in chunk {
            let a = pa.get(&va[k]).unwrap_or(&empty);
            let b = pb.get(&vb[k]).unwrap_or(&empty);
            let d = compare_px(a, b, o.color_tol);
            if d.over_tol > 0 {
                divergent_frames += 1;
            } else {
                equivalent_frames += 1;
            }
            agg.total += d.total;
            agg.exact += d.exact;
            agg.over_tol += d.over_tol;
            agg.max_delta = agg.max_delta.max(d.max_delta);
            agg.sum_abs += d.sum_abs;
            agg.sum_sq += d.sum_sq;
            if worst.len() < MAX_WORST * 3 {
                worst.push(json!({
                    "frame": k,
                    "ref_frame": va[k], "cand_frame": vb[k],
                    "ref_pts": refd.frames[va[k]].pts,
                    "cand_pts": candd.frames[vb[k]].pts,
                    "ref_hash": format!("{:016x}", refd.frames[va[k]].hash),
                    "cand_hash": format!("{:016x}", candd.frames[vb[k]].hash),
                    "diff": diff_json(&d),
                }));
            }
        }
    }
    worst.sort_by(|a, b| {
        let x = b.get("diff").and_then(|d| d.get("diff_over_tol")).and_then(|v| v.as_u64()).unwrap_or(0);
        let y = a.get("diff").and_then(|d| d.get("diff_over_tol")).and_then(|v| v.as_u64()).unwrap_or(0);
        x.cmp(&y)
    });
    worst.truncate(MAX_WORST);
    let content_verdict = if divergent_frames > 0 {
        "divergent"
    } else if equivalent_frames > 0 {
        "equivalent"
    } else {
        "identical"
    };

    // ---- 轨道 2：时间轴 -----------------------------------------------
    // 在两流所有 PTS 的并集上采样，比较「此刻屏幕上挂着哪张画面」。
    // 只比 hash，不做像素比对：PTS 平移会让 A 的第 k 帧撞上 B 的第 k±1 帧，
    // 那种错位是时序问题，不该被算成画质误差。
    let mut samples: Vec<u32> = refd.frames.iter().map(|f| f.pts)
        .chain(candd.frames.iter().map(|f| f.pts)).collect();
    samples.sort_unstable();
    samples.dedup();
    let mut tl_identical = 0usize;
    let mut tl_presence = 0usize;
    let mut tl_color = 0usize;
    let mut tl_frame = 0usize;
    let mut tl_not_started = 0usize;
    let mut intervals: Vec<(u32, u32, usize, &'static str)> = Vec::new();
    let mut run_start: Option<(u32, u32, usize, &'static str)> = None;
    for &t in &samples {
        let ai = frame_at(&refd.frames, t);
        let bi = frame_at(&candd.frames, t);
        let kind = match (ai, bi) {
            (None, _) | (_, None) => "not_started",
            (Some(a), Some(b)) => {
                let fa = &refd.frames[a];
                let fb = &candd.frames[b];
                if fa.hash == fb.hash {
                    "identical"
                } else if fa.is_clear() != fb.is_clear() {
                    "presence"
                } else if fa.opaque == fb.opaque && fa.bbox == fb.bbox {
                    // 像素集合一模一样，只有颜色值不同：工具链间 ±1 的 YCbCr
                    // 舍入差，不是画面错位，也不该算成时序问题。
                    "color_only"
                } else {
                    "frame_mismatch"
                }
            }
        };
        match kind {
            "identical" => tl_identical += 1,
            "presence" => tl_presence += 1,
            "color_only" => tl_color += 1,
            "frame_mismatch" => tl_frame += 1,
            _ => tl_not_started += 1,
        }
        let mismatch = kind == "presence" || kind == "frame_mismatch";
        if mismatch {
            match &mut run_start {
                Some(r) => {
                    r.1 = t;
                    r.2 += 1;
                    if r.3 != kind {
                        r.3 = "mixed";
                    }
                }
                None => run_start = Some((t, t, 1, kind)),
            }
        } else if let Some(r) = run_start.take() {
            intervals.push(r);
        }
    }
    if let Some(r) = run_start.take() {
        intervals.push(r);
    }
    let timeline_verdict = if tl_presence == 0 && tl_frame == 0 && tl_color == 0 && tl_not_started == 0 {
        "identical"
    } else if tl_frame > 0 {
        "shifted"
    } else if tl_presence > 0 {
        "presence_diff"
    } else {
        "color_only"
    };
    let timeline_ok = {
        let a0 = refd.frames.first().map(|f| f.pts).unwrap_or(0);
        let b0 = candd.frames.first().map(|f| f.pts).unwrap_or(0);
        let a1 = refd.frames.last().map(|f| f.pts).unwrap_or(0);
        let b1 = candd.frames.last().map(|f| f.pts).unwrap_or(0);
        abs_sub(a0, b0) <= tol_ticks && abs_sub(a1, b1) <= tol_ticks
    };

    // ---- 综合 ---------------------------------------------------------
    // 内容优先：画面编错了就是 divergent。内容无损但时间轴不一致，单独标成
    // content_ok_time_shift —— 这正是真机播放验证要重点复核的那一类。
    // 只有真正的错位 / 存在性差异 / 按序号的 PTS 平移才算「时间轴不一致」；
    // color_only 只是工具链间的颜色舍入，不影响播放时序结论。
    let tl_shift = pts_shifted > 0 || tl_frame > 0 || tl_presence > 0;
    let verdict = if content_verdict == "divergent" {
        "divergent"
    } else if content_verdict == "identical" && !tl_shift {
        "identical"
    } else if !tl_shift {
        "equivalent"
    } else {
        "content_ok_time_shift"
    };

    let value = json!({
        "name": name,
        "verdict": verdict,
        "content": {
            "ref_visible_frames": va.len(),
            "cand_visible_frames": vb.len(),
            "aligned": n,
            "count_match": va.len() == vb.len(),
            "identical_frames": content_identical,
            "equivalent_frames": equivalent_frames,
            "divergent_frames": divergent_frames,
            "pixels": diff_json(&agg),
            "verdict": content_verdict,
            "worst": worst,
        },
        "timeline": {
            "ref_frames": refd.frames.len(),
            "cand_frames": candd.frames.len(),
            "sample_points": samples.len(),
            "ref_first_pts": refd.frames.first().map(|f| f.pts).unwrap_or(0),
            "cand_first_pts": candd.frames.first().map(|f| f.pts).unwrap_or(0),
            "ref_last_pts": refd.frames.last().map(|f| f.pts).unwrap_or(0),
            "cand_last_pts": candd.frames.last().map(|f| f.pts).unwrap_or(0),
            "ok": timeline_ok,
            "tol_ticks": tol_ticks,
            "identical": tl_identical,
            "presence_mismatch": tl_presence,
            "color_only": tl_color,
            "frame_mismatch": tl_frame,
            "not_started": tl_not_started,
            "verdict": timeline_verdict,
            "pts_delta": {
                "max_ticks": pts_delta_max,
                "mean_ticks": if n > 0 { (pts_delta_sum as f64 / n as f64 * 100.0).round() / 100.0 } else { 0.0 },
                "shifted_frames": pts_shifted,
            },
            "mismatch_intervals": intervals.iter().take(MAX_INTERVALS).map(|&(a, b, n, k)| json!({
                "start_pts": a, "end_pts": b, "start": clock(a), "end": clock(b),
                "samples": n, "kind": k,
            })).collect::<Vec<_>>(),
            "mismatch_intervals_total": intervals.len(),
        },
    });
    Ok(PairResult { value, verdict: verdict.to_string() })
}

// ---------------------------------------------------------------------------
// 源头对拍：源 PNG vs 每条流的非清屏帧（按事件序号对齐）
// ---------------------------------------------------------------------------

fn source_check(
    name: &str,
    d: &pgsdec::Decoded,
    src: &[crate::pgs::SrcFrame],
    o: &Options,
) -> Result<Value, String> {
    let visible: Vec<usize> = d.frames.iter().enumerate().filter(|(_, f)| !f.is_clear()).map(|(i, _)| i).collect();
    let n = visible.len().min(src.len());
    let mut want: HashSet<usize> = HashSet::new();
    for &i in visible.iter().take(n) {
        want.insert(i);
    }
    let px = pixels_of(Path::new(&d.path), &want)?;
    let empty: Vec<(u32, [u8; 4])> = Vec::new();
    let mut agg = PxDiff { total: 0, exact: 0, over_tol: 0, max_delta: 0, sum_abs: 0, sum_sq: 0 };
    let mut identical = 0usize;
    let mut max_pts_delta = 0u32;
    let mut sum_pts_delta = 0u64;
    let mut worst: Vec<Value> = Vec::new();
    for k in 0..n {
        let i = visible[k];
        let got = px.get(&i).unwrap_or(&empty);
        let diff = compare_px(&src[k].px, got, o.color_tol);
        let delta = abs_sub(src[k].pts, d.frames[i].pts);
        max_pts_delta = max_pts_delta.max(delta);
        sum_pts_delta += delta as u64;
        if diff.total == diff.exact {
            identical += 1;
        } else if worst.len() < MAX_WORST {
            worst.push(json!({
                "event": k, "frame": i,
                "event_pts": src[k].pts, "frame_pts": d.frames[i].pts,
                "diff": diff_json(&diff),
            }));
        }
        agg.total += diff.total;
        agg.exact += diff.exact;
        agg.over_tol += diff.over_tol;
        agg.max_delta = agg.max_delta.max(diff.max_delta);
        agg.sum_abs += diff.sum_abs;
        agg.sum_sq += diff.sum_sq;
    }
    Ok(json!({
        "name": name,
        "source_events": src.len(),
        "stream_visible_frames": visible.len(),
        "aligned": n,
        "count_match": visible.len() == src.len(),
        "identical_frames": identical,
        "pixels": diff_json(&agg),
        "max_pts_delta_ticks": max_pts_delta,
        "mean_pts_delta_ticks": if n > 0 { (sum_pts_delta as f64 / n as f64 * 100.0).round() / 100.0 } else { 0.0 },
        "worst": worst,
        "verdict": if n == 0 { "no_data" } else if agg.over_tol == 0 && agg.total == agg.exact { "identical" }
                   else if agg.over_tol == 0 { "equivalent" } else { "divergent" },
    }))
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

fn severity(v: &str) -> u8 {
    match v {
        "divergent" => 3,
        "content_ok_time_shift" => 2,
        "equivalent" => 1,
        _ => 0,
    }
}

pub fn run(o: &Options) -> Result<Value, String> {
    if !o.reference.is_file() {
        return Err(format!("参考流不存在：{}", o.reference.display()));
    }
    for c in &o.candidates {
        if !c.path.is_file() {
            return Err(format!("候选流不存在：{} ({})", c.path.display(), c.name));
        }
    }
    let fps = if o.fps > 1.0 { o.fps } else { 24_000.0 / 1001.0 };
    let tol_ticks = ((pgsdec::PGS_HZ / fps) * o.tol_frames as f64).round() as u32;

    let refd = pgsdec::decode(&o.reference, None)?;
    let mut streams: Vec<(String, pgsdec::Decoded)> = Vec::new();
    for c in &o.candidates {
        let d = pgsdec::decode(&c.path, None)?;
        streams.push((c.name.clone(), d));
    }

    let gold = golden(&refd, o)?;

    let mut pairs: Vec<Value> = Vec::new();
    let mut worst_verdict = String::from("identical");
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "对拍基准 · 参考 {} · {} 条候选 · 采样容差 {} 帧 / 颜色容差 {}",
        o.reference.display(),
        streams.len(),
        o.tol_frames,
        o.color_tol
    ));
    lines.push(format!(
        "三角  ① 源 PNG → ② SUP 解码画面（本工具）→ ③ 真机录屏（本工具导出帧清单与参考画面供对照）"
    ));

    for (name, d) in &streams {
        let r = compare_pair(&refd, d, name, o, tol_ticks)?;
        if severity(&r.verdict) > severity(&worst_verdict) {
            worst_verdict = r.verdict.clone();
        }
        let p = &r.value;
        lines.push(format!(
            "[{}] {} · 内容 {}/{} 帧一致（等价 {} / 实质差异 {}）· 时序 {}（采样 {}，存在性 {} / 错位 {} / 颜色舍入 {}）· PTS 偏移最大 {} tick · 起止 {}",
            r.verdict,
            name,
            p["content"]["identical_frames"].as_u64().unwrap_or(0),
            p["content"]["aligned"].as_u64().unwrap_or(0),
            p["content"]["equivalent_frames"].as_u64().unwrap_or(0),
            p["content"]["divergent_frames"].as_u64().unwrap_or(0),
            p["timeline"]["verdict"].as_str().unwrap_or(""),
            p["timeline"]["sample_points"].as_u64().unwrap_or(0),
            p["timeline"]["presence_mismatch"].as_u64().unwrap_or(0),
            p["timeline"]["frame_mismatch"].as_u64().unwrap_or(0),
            p["timeline"]["color_only"].as_u64().unwrap_or(0),
            p["timeline"]["pts_delta"]["max_ticks"].as_u64().unwrap_or(0),
            if p["timeline"]["ok"].as_bool().unwrap_or(false) { "一致" } else { "不一致" },
        ));
        pairs.push(r.value);
    }
    let source = if let Some(xml) = &o.xml {
        let (vw, vh, xfps, src) = crate::pgs::source_frames(xml)?;
        let mut checks: Vec<Value> = Vec::new();
        checks.push(source_check("ref", &refd, &src, o)?);
        for (name, d) in &streams {
            checks.push(source_check(name, d, &src, o)?);
        }
        for c in &checks {
            lines.push(format!(
                "[源] {} · 事件 {} / 可见帧 {} · 对齐 {} · 逐像素一致 {} / {} · 最大通道差 {}",
                c["name"].as_str().unwrap_or(""),
                c["source_events"].as_u64().unwrap_or(0),
                c["stream_visible_frames"].as_u64().unwrap_or(0),
                c["aligned"].as_u64().unwrap_or(0),
                c["identical_frames"].as_u64().unwrap_or(0),
                c["aligned"].as_u64().unwrap_or(0),
                c["pixels"]["max_channel_delta"].as_u64().unwrap_or(0),
            ));
        }
        Some(json!({
            "xml": xml.to_string_lossy().to_string(),
            "video": format!("{}x{}", vw, vh),
            "fps": xfps,
            "events": src.len(),
            "checks": checks,
        }))
    } else {
        lines.push("[源] 未给 --xml，跳过源头对拍（只做流间对拍）".into());
        None
    };

    let reference = json!({
        "name": "ref",
        "stats": stream_brief(&refd),
        "verify": verify_brief(&o.reference),
    });
    let stream_json: Vec<Value> = streams
        .iter()
        .map(|(name, d)| json!({
            "name": name,
            "stats": stream_brief(d),
            "verify": verify_brief(Path::new(&d.path)),
        }))
        .collect();

    Ok(json!({
        "ok": true,
        "engine": "supfast baseline 1",
        "params": {
            "fps": fps, "tol_frames": o.tol_frames, "tol_ticks": tol_ticks,
            "color_tol": o.color_tol, "ratio_tol": o.ratio_tol,
            "max_png": o.max_png,
        },
        "reference": reference,
        "streams": stream_json,
        "golden": gold,
        "pairs": pairs,
        "source": source,
        "summary": {
            "verdict": worst_verdict,
            "lines": lines,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_px_ignores_transparent_rgb() {
        let a = vec![(1u32, [100, 120, 130, 255]), (2u32, [200, 10, 20, 0])];
        let b = vec![(1u32, [100, 120, 130, 255])];
        let d = compare_px(&a, &b, 0);
        assert_eq!(d.total, 2);
        assert_eq!(d.exact, 2); // 全透明那格不算差异
    }

    #[test]
    fn compare_px_reports_channel_delta() {
        let a = vec![(1u32, [100, 120, 130, 255])];
        let b = vec![(1u32, [100, 122, 130, 255])];
        let d = compare_px(&a, &b, 0);
        assert_eq!(d.total, 1);
        assert_eq!(d.exact, 0);
        assert_eq!(d.over_tol, 1);
        assert_eq!(d.max_delta, 2);
    }

    #[test]
    fn frame_at_picks_last_not_after() {
        let fs = vec![
            pgsdec::FrameMeta { pts: 10, state: 0x80, pal_upd: false, nref: 1, comp_no: 0,
                hash: 0, opaque: 1, bbox: [0,0,1,1], undefined_refs: 0, undefined_pal: 0 },
            pgsdec::FrameMeta { pts: 20, state: 0, pal_upd: false, nref: 0, comp_no: 1,
                hash: 0, opaque: 0, bbox: [0,0,0,0], undefined_refs: 0, undefined_pal: 0 },
        ];
        assert_eq!(frame_at(&fs, 5), None);
        assert_eq!(frame_at(&fs, 10), Some(0));
        assert_eq!(frame_at(&fs, 19), Some(0));
        assert_eq!(frame_at(&fs, 20), Some(1));
        assert_eq!(frame_at(&fs, 999), Some(1));
    }

    #[test]
    fn png_roundtrip_has_valid_signature() {
        let p = std::env::temp_dir().join("supfast_baseline_png.png");
        write_rgba_png(&p, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 128]).unwrap();
        let d = fs::read(&p).unwrap();
        assert_eq!(&d[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&d[12..16], b"IHDR");
        let _ = fs::remove_file(&p);
    }
}
