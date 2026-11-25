//! 原生 HDMV PGS 编码器（不依赖 SUPer / Python / AviSynth 的 SUP 输出）
//!
//! 输入是 supfast 已经产出的 BDN XML + 8bit 索引色 PNG（`avs2bdnxml -p 1` 的产物）。
//! 与「逐段拼 SUP」不同，这里一次性规划整条流：
//!   * 按 object buffer 容量把事件串划分成 epoch，epoch 内窗口定义固定；
//!   * 新位图走 Acquisition Point，重复位图走 Normal Case 复用 object_id；
//!   * 每个显示集统一推导 PTS/DTS，满足解码器提前量与单调性；
//!   * 自己写 PCS/WDS/PDS/ODS/END 段与 PGS RLE，ODS 超长自动分片。
//!
//! 段格式依据 HDMV PGS 规范；RLE 编码参考 MIT 许可的 brule 实现，
//! 未复制 SUPer（GPL-3.0）的任何代码。

use flate2::read::ZlibDecoder;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

const PGS_HZ: f64 = 90_000.0;
const RD: f64 = 16e6;
const RC: f64 = 32e6;
/// 单个 ODS 片段的 RLE 负载上限（段 payload 上限 65535，留出 11 字节首片头）
const MAX_FRAG: usize = 60_000;
/// 一个 epoch 内对象面积预算，留 1 MiB 余量给 4 MiB 的 object buffer
const OBJ_BUDGET: u64 = 3 * 1024 * 1024;
/// 一个 epoch 内最多 60 个唯一 object_id（规范上限 64）
const MAX_OBJ_IDS: usize = 60;
/// 超过 2 秒的字幕空档就另起一个 epoch
const MAX_GAP_TICKS: u32 = 180_000;
/// 反量化重映射的序列缓冲上限：避免异常输入撑爆内存（约 32 MiB）
const FADE_MAX_SEQ_BYTES: usize = 32 * 1024 * 1024;
/// 一条调色板动画链最多纳入的帧数
const FADE_MAX_FRAMES: usize = 512;
/// 一条帧间对象拆分链最多纳入的帧数（防止超长动画吃光 object_id 预算）
const SPLIT_MAX_FRAMES: usize = 128;
/// 只有预估净收益达到这个字节数才启用拆分，避免边际优化换来额外风险
const SPLIT_MIN_GAIN: isize = 64;

const PCS: u8 = 0x16;
const WDS: u8 = 0x17;
const PDS: u8 = 0x14;
const ODS: u8 = 0x15;
const END: u8 = 0x80;

/// composition_state 字段的值（写入 PCS 时左移 6 位）
const ST_NORMAL: u8 = 0;
const ST_ACQ: u8 = 1;
const ST_EPOCH: u8 = 2;

// ---------------------------------------------------------------------------
// PNG：只吃 8bit 索引色（colortype 3），这是 avs2bdnxml -p 1 的产物
// ---------------------------------------------------------------------------

struct Png {
    w: u32,
    h: u32,
    plte: Vec<[u8; 3]>,
    trns: Vec<u8>,
    idx: Vec<u8>,
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i32 + b as i32 - c as i32;
    let pa = (p - a as i32).abs();
    let pb = (p - b as i32).abs();
    let pc = (p - c as i32).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn decode_png(path: &Path) -> Result<Png, String> {
    let d = fs::read(path).map_err(|e| format!("读 PNG 失败 {}: {}", path.display(), e))?;
    if d.len() < 8 || &d[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(format!("不是 PNG：{}", path.display()));
    }
    let mut pos = 8usize;
    let mut ihdr: Option<(u32, u32, u8, u8)> = None;
    let mut plte: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    while pos + 8 <= d.len() {
        let len = u32::from_be_bytes([d[pos], d[pos + 1], d[pos + 2], d[pos + 3]]) as usize;
        let typ = &d[pos + 4..pos + 8];
        let end = pos + 8 + len;
        if end + 4 > d.len() {
            return Err(format!("PNG chunk 越界：{}", path.display()));
        }
        let data = &d[pos + 8..end];
        match typ {
            b"IHDR" if data.len() >= 10 => {
                ihdr = Some((
                    u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
                    u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
                    data[8],
                    data[9],
                ));
            }
            b"PLTE" => {
                for c in data.chunks_exact(3) {
                    plte.push([c[0], c[1], c[2]]);
                }
            }
            b"tRNS" => trns = data.to_vec(),
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        pos = end + 4;
    }
    let (w, h, bd, ct) = ihdr.ok_or_else(|| format!("PNG 缺 IHDR：{}", path.display()))?;
    if bd != 8 || ct != 3 {
        return Err(format!(
            "{} 是 {}bit/colortype{} PNG，原生编码器只吃 8bit 索引色（请开 -p 1）",
            path.file_name().unwrap_or_default().to_string_lossy(),
            bd,
            ct
        ));
    }
    if plte.is_empty() || w == 0 || h == 0 {
        return Err(format!("PNG 缺 PLTE 或尺寸为 0：{}", path.display()));
    }
    if trns.is_empty() {
        trns = vec![255; plte.len()];
    }

    let mut raw = Vec::new();
    ZlibDecoder::new(&idat[..])
        .read_to_end(&mut raw)
        .map_err(|e| format!("解压 {} 的 IDAT 失败：{}", path.display(), e))?;
    let stride = w as usize;
    let hh = h as usize;
    if raw.len() < (stride + 1) * hh {
        return Err(format!("PNG 像素数据不足：{}", path.display()));
    }
    let mut idx = vec![0u8; stride * hh];
    let mut p = 0usize;
    for y in 0..hh {
        let ft = raw[p];
        p += 1;
        for x in 0..stride {
            let v = raw[p + x];
            let a = if x > 0 { idx[y * stride + x - 1] } else { 0 };
            let b = if y > 0 { idx[(y - 1) * stride + x] } else { 0 };
            let c = if x > 0 && y > 0 {
                idx[(y - 1) * stride + x - 1]
            } else {
                0
            };
            idx[y * stride + x] = match ft {
                0 => v,
                1 => v.wrapping_add(a),
                2 => v.wrapping_add(b),
                3 => v.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => v.wrapping_add(paeth(a, b, c)),
                _ => return Err(format!("未知 PNG filter {}：{}", ft, path.display())),
            };
        }
        p += stride;
    }

    // PGS 的 RLE 把索引 0 当作透明行程，必须让 0 号色真的是透明色。
    if trns.first().copied().unwrap_or(255) != 0 {
        if let Some(t) = trns.iter().position(|&a| a == 0) {
            plte.swap(0, t);
            trns.swap(0, t);
            for v in &mut idx {
                if *v == 0 {
                    *v = t as u8;
                } else if *v == t as u8 {
                    *v = 0;
                }
            }
        } else {
            trns[0] = 0;
        }
    }
    Ok(Png { w, h, plte, trns, idx })
}

// ---------------------------------------------------------------------------
// PGS RLE
// ---------------------------------------------------------------------------

fn rle_encode(idx: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut o = Vec::with_capacity(idx.len() / 2 + h * 2);
    for y in 0..h {
        let row = &idx[y * w..(y + 1) * w];
        let mut j = 0usize;
        while j < w {
            let c = row[j];
            let mut n = 1usize;
            while j + n < w && row[j + n] == c {
                n += 1;
            }
            if c == 0 {
                if n > 63 {
                    o.push(0x00);
                    o.push(0x40 | ((n >> 8) & 0x3F) as u8);
                    o.push((n & 0xFF) as u8);
                } else {
                    o.push(0x00);
                    o.push((n & 0x3F) as u8);
                }
            } else if n > 63 {
                o.push(0x00);
                o.push(0xC0 | ((n >> 8) & 0x3F) as u8);
                o.push((n & 0xFF) as u8);
                o.push(c);
            } else if n > 2 {
                o.push(0x00);
                o.push(0x80 | (n as u8 & 0x3F));
                o.push(c);
            } else {
                for _ in 0..n {
                    o.push(c);
                }
            }
            j += n;
        }
        o.push(0x00);
        o.push(0x00);
    }
    o
}

#[cfg(test)]
fn rle_decode(data: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    let mut k = 0usize;
    let mut y = 0usize;
    let mut x = 0usize;
    while k < data.len() {
        let b = data[k];
        if b == 0 {
            k += 1;
            if k >= data.len() {
                break;
            }
            let n = data[k];
            if n == 0 {
                y += 1;
                x = 0;
                k += 1;
                continue;
            }
            let mut len = (n & 0x3F) as usize;
            if n & 0x40 != 0 {
                k += 1;
                if k >= data.len() {
                    break;
                }
                len = (len << 8) | data[k] as usize;
            }
            let color = if n & 0x80 != 0 {
                k += 1;
                if k >= data.len() {
                    break;
                }
                data[k]
            } else {
                0
            };
            for _ in 0..len {
                if y < h && x < w {
                    out[y * w + x] = color;
                    x += 1;
                }
            }
            k += 1;
        } else {
            if y < h && x < w {
                out[y * w + x] = b;
                x += 1;
            }
            k += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// BDN XML（零依赖字符串扫描，结构与 engine::make_bdn_xml 对齐）
// ---------------------------------------------------------------------------

/// 找标签起始位置：`<Event` 后面必须跟空白或 `>`，否则会误匹配 `<Events .../>`、
/// `<Events>` 容器。BDN 里这两个标签恰好带 FirstEventInTC / LastEventOutTC 属性，
/// 一旦当成事件，整条时间轴会被级联平移到片尾（结构仍合规，画面却全错）。
fn find_tag(s: &str, name: &str, from: usize) -> Option<usize> {
    let mut p = from;
    while p < s.len() {
        let k = s[p..].find(name)?;
        let at = p + k;
        match s.as_bytes().get(at + name.len()).copied() {
            Some(b' ') | Some(b'>') | Some(b'\t') | Some(b'\r') | Some(b'\n') | None => return Some(at),
            _ => p = at + name.len(),
        }
    }
    None
}

fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0usize;
    while let Some(p) = tag[from..].find(key) {
        let s = from + p;
        // 属性名必须独立：`FirstEventInTC` 里的 "InTC" 不算 InTC
        let boundary = s == 0
            || matches!(tag.as_bytes()[s - 1], b' ' | b'\t' | b'\r' | b'\n' | b'<');
        if !boundary {
            from = s + key.len();
            continue;
        }
        let rest = tag[s + key.len()..].trim_start();
        if let Some(q) = rest.strip_prefix('=') {
            let q = q.trim_start();
            let quote = q.chars().next()?;
            if quote == '"' || quote == '\'' {
                let body = &q[1..];
                let e = body.find(quote)?;
                return Some(&body[..e]);
            }
        }
        from = s + key.len();
    }
    None
}

#[derive(Clone)]
struct Graphic {
    file: String,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

struct Ev {
    itc: String,
    otc: String,
    forced: bool,
    gs: Vec<Graphic>,
}

struct Bdn {
    fps: f64,
    fps_id: u8,
    vfmt: String,
    events: Vec<Ev>,
}

fn parse_fps(s: &str) -> f64 {
    match s.trim() {
        "23.976" | "23.98" | "24000/1001" => 24_000.0 / 1001.0,
        "29.97" | "30000/1001" => 30_000.0 / 1001.0,
        "59.94" | "60000/1001" => 60_000.0 / 1001.0,
        _ => s.trim().parse::<f64>().unwrap_or(24_000.0 / 1001.0),
    }
}

fn fps_id(fps: f64) -> u8 {
    let near = |v: f64| (fps - v).abs() < 0.02;
    if near(24_000.0 / 1001.0) {
        1
    } else if near(24.0) {
        2
    } else if near(25.0) {
        3
    } else if near(30_000.0 / 1001.0) {
        4
    } else if near(30.0) {
        5
    } else if near(50.0) {
        6
    } else if near(60_000.0 / 1001.0) {
        7
    } else {
        8
    }
}

/// BDN 的 VideoFormat 字符串 → 视频尺寸
pub fn video_size(vfmt: &str) -> (u32, u32) {
    let s = vfmt.to_ascii_lowercase();
    if s.contains("2160") {
        (3840, 2160)
    } else if s.contains("1080") {
        (1920, 1080)
    } else if s.contains("720") {
        (1280, 720)
    } else if s.contains("576") {
        (720, 576)
    } else if s.contains("480") {
        (720, 480)
    } else {
        (1920, 1080)
    }
}

fn parse_bdn(path: &Path) -> Result<Bdn, String> {
    let s = fs::read_to_string(path)
        .map_err(|e| format!("读 BDN XML 失败 {}: {}", path.display(), e))?;
    let fmt = s
        .find("<Format")
        .and_then(|p| s[p..].find('>').map(|e| &s[p..p + e + 1]))
        .unwrap_or("");
    let fps = parse_fps(attr(fmt, "FrameRate").unwrap_or("23.976"));
    let vfmt = attr(fmt, "VideoFormat").unwrap_or("1080p").to_string();

    let mut events = Vec::new();
    let mut rest = s.as_str();
    while let Some(p) = find_tag(rest, "<Event", 0) {
        let a = &rest[p..];
        let te = a.find('>').ok_or("BDN XML 的 <Event> 标签不完整")?;
        let tag = &a[..te + 1];
        let bs = te + 1;
        let ce = a[bs..].find("</Event>").ok_or("BDN XML 缺 </Event>")?;
        let body = &a[bs..bs + ce];
        let mut gs = Vec::new();
        let mut r = body;
        while let Some(gp) = find_tag(r, "<Graphic", 0) {
            let ga = &r[gp..];
            let gte = ga.find('>').ok_or("BDN XML 的 <Graphic> 标签不完整")?;
            let gtag = &ga[..gte + 1];
            let gbs = gte + 1;
            let gce = ga[gbs..]
                .find("</Graphic>")
                .ok_or("BDN XML 缺 </Graphic>")?;
            let file = ga[gbs..gbs + gce].trim().to_string();
            let u = |k: &str| attr(gtag, k).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            if !file.is_empty() {
                gs.push(Graphic {
                    file,
                    x: u("X"),
                    y: u("Y"),
                    w: u("Width"),
                    h: u("Height"),
                });
            }
            r = &ga[gbs + gce + 10..];
        }
        if !gs.is_empty() {
            events.push(Ev {
                itc: attr(tag, "InTC").unwrap_or("00:00:00:00").to_string(),
                otc: attr(tag, "OutTC").unwrap_or("00:00:00:00").to_string(),
                forced: attr(tag, "Forced").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false),
                gs,
            });
        }
        rest = &a[bs + ce + 8..];
    }
    if events.is_empty() {
        return Err("BDN XML 里没有可编码的事件".into());
    }
    Ok(Bdn { fps, fps_id: fps_id(fps), vfmt, events })
}

// ---------------------------------------------------------------------------
// 时间码 / 显示集规划
// ---------------------------------------------------------------------------

fn tc_frames(tc: &str, fps: f64) -> u32 {
    let p: Vec<&str> = tc.split(':').collect();
    if p.len() != 4 {
        return 0;
    }
    let fpsi = fps.round().max(1.0) as u32;
    let h: u32 = p[0].parse().unwrap_or(0);
    let m: u32 = p[1].parse().unwrap_or(0);
    let s: u32 = p[2].parse().unwrap_or(0);
    let f: u32 = p[3].parse().unwrap_or(0);
    (h * 3600 + m * 60 + s) * fpsi + f
}

fn ticks(frame: u32, fps: f64) -> u32 {
    ((frame as f64) * PGS_HZ / fps).round() as u32
}

/// 消除相邻事件重叠：起点落在上一段结尾之前就整段平移
fn mono_times(bdn: &Bdn) -> Vec<(u32, u32)> {
    let mut v = Vec::with_capacity(bdn.events.len());
    let mut last_out = 0u32;
    for e in &bdn.events {
        let mut a = tc_frames(&e.itc, bdn.fps);
        let mut b = tc_frames(&e.otc, bdn.fps);
        if a < last_out {
            let d = last_out - a;
            a += d;
            b += d;
        }
        if b <= a {
            b = a + 1;
        }
        last_out = b;
        v.push((a, b));
    }
    v
}

/// `min_gap` 是切 epoch 所需的最小时间空档。新 epoch 的 DTS = 首个显示集
/// PTS − lead，必须晚于上一个 epoch 的最后呈现时刻（SUPer
/// `check_pts_dts_sanity` 的 `epoch_start_dts>prev_pts`）；连续帧处切 epoch
/// 会让这条必然失败，所以把切分点推迟到下一个有足够空档的事件。
fn group_epochs(bdn: &Bdn, times: &[(u32, u32)], min_gap: u32) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut area = 0u64;
    let mut ids = 0usize;
    for i in 0..bdn.events.len() {
        let add: u64 = bdn.events[i]
            .gs
            .iter()
            .map(|g| g.w as u64 * g.h as u64)
            .sum();
        let n = bdn.events[i].gs.len();
        let in_t = ticks(times[i].0, bdn.fps);
        // empty_gap 用于「2 秒空档另起 epoch」；verifier_gap 必须与 SUPer
        // check_pts_dts_sanity 的 prev_pts 口径一致：上一 epoch 最后一个显示集的
        // PTS 在有空档时是清屏显示集（= prev_out），无清屏时是事件显示集（= prev_in）。
        // 早期用 in−prev_out 会偏小，误把合法边界合并成过大的 epoch（对象超 64/4 MiB）。
        let (empty_gap, verifier_gap) = cur.last().map_or((0u32, 0u32), |&p| {
            let prev_in = ticks(times[p].0, bdn.fps);
            let prev_out = ticks(times[p].1, bdn.fps);
            let vpts = if in_t > prev_out { prev_out } else { prev_in };
            (in_t.saturating_sub(prev_out), in_t.saturating_sub(vpts))
        });
        let long_gap = empty_gap > MAX_GAP_TICKS;
        // `first_pts − lead > prev_pts` ⟺ verifier_gap > lead（严格大于）。
        let may_cut = cur.is_empty() || verifier_gap > min_gap;
        if !cur.is_empty()
            && may_cut
            && (area + add > OBJ_BUDGET || ids + n > MAX_OBJ_IDS || long_gap)
        {
            groups.push(std::mem::take(&mut cur));
            area = add;
            ids = n;
        } else {
            area += add;
            ids += n;
        }
        cur.push(i);
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}

fn bbox(group: &[usize], bdn: &Bdn, vw: u32, vh: u32) -> (u32, u32, u32, u32) {
    let mut x0 = vw;
    let mut y0 = vh;
    let mut x1 = 0u32;
    let mut y1 = 0u32;
    for &i in group {
        for g in &bdn.events[i].gs {
            x0 = x0.min(g.x);
            y0 = y0.min(g.y);
            x1 = x1.max(g.x.saturating_add(g.w));
            y1 = y1.max(g.y.saturating_add(g.h));
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return (0, 0, vw, vh);
    }
    let x1 = x1.min(vw);
    let y1 = y1.min(vh);
    let x0 = x0.min(vw.saturating_sub(1));
    let y0 = y0.min(vh.saturating_sub(1));
    (x0, y0, (x1 - x0).max(1), (y1 - y0).max(1))
}

struct Bmp {
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    forced: bool,
    rle: Vec<u8>,
}

struct Slot {
    id: u16,
    ver: u8,
    key: Vec<u8>,
}

fn object_key(plte: &[[u8; 3]], trns: &[u8], rle: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(4 + plte.len() * 3 + trns.len() + rle.len());
    k.extend_from_slice(&(plte.len() as u32).to_be_bytes());
    for c in plte {
        k.extend_from_slice(c);
    }
    k.extend_from_slice(trns);
    k.extend_from_slice(rle);
    k
}

/// (PLTE, tRNS) → PDS 条目 (entry_id, Y, Cr, Cb, Alpha)
fn palette_entries(plte: &[[u8; 3]], trns: &[u8], bt709: bool) -> Vec<[u8; 5]> {
    plte.iter()
        .enumerate()
        .map(|(i, c)| {
            let a = trns.get(i).copied().unwrap_or(255);
            let (y, cr, cb) = rgb_to_ycrcb(*c, bt709);
            [i as u8, y, cr, cb, a]
        })
        .collect()
}

/// 索引 PNG → 逐像素 RGBA（decode_png 已保证索引 0 是透明色）
fn png_rgba(p: &Png) -> Vec<[u8; 4]> {
    p.idx
        .iter()
        .map(|&k| {
            let i = k as usize;
            let c = p.plte.get(i).copied().unwrap_or([0, 0, 0]);
            let a = p.trns.get(i).copied().unwrap_or(255);
            [c[0], c[1], c[2], a]
        })
        .collect()
}

/// PDS 差分：只保留与上一帧不同的条目。PGS 解码器按 entry_id 累积调色板，
/// 因此 palette update 显示集只需携带变化的项。
fn palette_diff(prev: &[[u8; 5]], cur: &[[u8; 5]]) -> Vec<[u8; 5]> {
    let mut out = Vec::new();
    for e in cur {
        let same = prev.iter().any(|p| p[0] == e[0] && *p == *e);
        if !same {
            out.push(*e);
        }
    }
    out
}

/// 把 N 帧 RGBA 压成「一张共用位图 + N 个调色板」——调色板渐变的核心。
///
/// 每个像素在 N 帧上的 RGBA 序列是一个特征：序列相同就能共用同一个 palette
/// entry，于是 N 帧只需一张 ODS 位图，靠逐帧 PDS 换颜色/透明度还原动画。
/// 全透明序列固定占 entry 0（PGS RLE 把索引 0 当透明行程）。序列种类超过
/// 255（PGS 调色板容量）时返回 None，调用方整条链回退逐帧 ODS，**不引入
/// 任何近似误差**。
fn build_fade_chain(
    frames: &[Vec<[u8; 4]>],
    w: u32,
    h: u32,
    bt709: bool,
) -> Option<(Vec<u8>, Vec<Vec<[u8; 5]>>)> {
    let n = frames.len();
    if n < 2 {
        return None;
    }
    let npix = (w as usize).checked_mul(h as usize)?;
    if npix == 0 || npix.checked_mul(n)?.checked_mul(4)? > FADE_MAX_SEQ_BYTES {
        return None;
    }
    if frames.iter().any(|f| f.len() != npix) {
        return None;
    }
    let mut ids: HashMap<Vec<u8>, u8> = HashMap::new();
    let mut reps: Vec<Vec<u8>> = Vec::new();
    let mut bitmap = vec![0u8; npix];
    let mut seq = vec![0u8; n * 4];
    for p in 0..npix {
        for (t, f) in frames.iter().enumerate() {
            seq[t * 4..t * 4 + 4].copy_from_slice(&f[p]);
        }
        if (0..n).all(|t| seq[t * 4 + 3] == 0) {
            continue; // 全透明 → entry 0
        }
        let id = match ids.get(seq.as_slice()) {
            Some(&v) => v,
            None => {
                let v = reps.len() + 1;
                if v > 255 {
                    return None;
                }
                let v = v as u8;
                ids.insert(seq.clone(), v);
                reps.push(seq.clone());
                v
            }
        };
        bitmap[p] = id;
    }
    let mut pals = Vec::with_capacity(n);
    for t in 0..n {
        let mut entries = Vec::with_capacity(1 + reps.len());
        entries.push([0u8, 16, 128, 128, 0]); // entry 0 恒透明
        for (k, rep) in reps.iter().enumerate() {
            let o = t * 4;
            let (y, cr, cb) = rgb_to_ycrcb([rep[o], rep[o + 1], rep[o + 2]], bt709);
            entries.push([(k + 1) as u8, y, cr, cb, rep[o + 3]]);
        }
        pals.push(entries);
    }
    Some((bitmap, pals))
}

/// 在一个 epoch 内扫描可压成调色板动画的连续事件链。
/// 条件：单 Graphic、尺寸与位置完全相同、时间首尾相接、序列种类 ≤255。
fn find_fade_chains(
    group: &[usize],
    bdn: &Bdn,
    times: &[(u32, u32)],
    dir: &Path,
    bt709: bool,
) -> Vec<FadeChain> {
    let mut out = Vec::new();
    let mut k = 0usize;
    while k < group.len() {
        let ev0 = &bdn.events[group[k]];
        if ev0.gs.len() != 1 {
            k += 1;
            continue;
        }
        let g0 = &ev0.gs[0];
        let mut j = k + 1;
        while j < group.len() && j - k < FADE_MAX_FRAMES {
            let ei = group[j];
            let ev = &bdn.events[ei];
            if ev.gs.len() != 1 {
                break;
            }
            let g = &ev.gs[0];
            if g.w != g0.w || g.h != g0.h || g.x != g0.x || g.y != g0.y {
                break;
            }
            if times[ei].0 != times[group[j - 1]].1 {
                break;
            }
            j += 1;
        }
        if j - k >= 2 {
            let mut frames = Vec::with_capacity(j - k);
            let mut ok = true;
            for q in k..j {
                match decode_png(&dir.join(&bdn.events[group[q]].gs[0].file)) {
                    Ok(p) if p.w == g0.w && p.h == g0.h => frames.push(png_rgba(&p)),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if let Some((bitmap, pals)) = build_fade_chain(&frames, g0.w, g0.h, bt709) {
                    let rle = rle_encode(&bitmap, g0.w as usize, g0.h as usize);
                    out.push(FadeChain {
                        start: k,
                        end: j,
                        rle,
                        pals,
                        w: g0.w,
                        h: g0.h,
                        x: g0.x,
                        y: g0.y,
                        forced: ev0.forced,
                    });
                    k = j;
                    continue;
                }
            }
        }
        k += 1;
    }
    out
}

/// 一条可无损压成「一张共用位图 + 逐帧调色板」的动画链。
///
/// 与 SUPer 的 PaletteSequenceEffect 同理，但不做有损聚类：把每个像素在 N 帧上的
/// RGBA 序列当作特征，序列种类不超过 PGS 调色板容量时才能用同一个 palette entry
/// 精确表达；超过就整条链回退到逐帧 ODS，绝不引入近似误差。
struct FadeChain {
    /// group 内起始下标
    start: usize,
    /// group 内结束下标（不含）
    end: usize,
    /// 共用位图的 RLE
    rle: Vec<u8>,
    /// 每帧 PDS 条目：(entry_id, Y, Cr, Cb, Alpha)
    pals: Vec<Vec<[u8; 5]>>,
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    forced: bool,
}

// ---------------------------------------------------------------------------
// 阶段三：帧间对象拆分（无损版）
// ---------------------------------------------------------------------------

/// 一条可无损拆成双对象显示的连续帧链。
///
/// SUPer 的 `ObjectDetector` 用 SSIM 阈值（约 0.986）判断「当前帧能否并进
/// 累积合成图」，把连续帧合成一个 object 只传一次 ODS —— 那是有损近似。
/// 这里做零画质损失的等价优化：把每帧按同一条线切成两个矩形对象
/// （PGS 单显示集恰好最多 2 个 composition object），跨帧逐字节比较块内容，
/// 只有完全相同的块才复用 object_id，只重传变化的块。
struct SplitChain {
    /// group 内起始下标
    start: usize,
    /// group 内结束下标（不含）
    end: usize,
    /// true = 左右切（cut 是第一块的宽），false = 上下切（cut 是第一块的高）
    horizontal: bool,
    cut: u32,
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    forced: bool,
    /// 每帧：(PLTE, tRNS, 第一块 RLE, 第二块 RLE)
    frames: Vec<(Vec<[u8; 3]>, Vec<u8>, Vec<u8>, Vec<u8>)>,
    /// 链上唯一块总数，用于 object_id 预算核算
    unique_parts: usize,
    /// 链上唯一块的像素面积总和，用于 object buffer 预算核算
    unique_area: u64,
}

/// 按一条线把索引矩阵切成两块，各自做 PGS RLE。不重新量化，PLTE 保持原样，
/// 两块拼回去与原图逐像素一致。
fn split_rle(p: &Png, horizontal: bool, cut: u32) -> (Vec<u8>, Vec<u8>) {
    let w = p.w as usize;
    let h = p.h as usize;
    let c = cut as usize;
    if horizontal {
        let mut a = Vec::with_capacity(c * h);
        let mut b = Vec::with_capacity((w - c) * h);
        for y in 0..h {
            let row = &p.idx[y * w..y * w + w];
            a.extend_from_slice(&row[..c]);
            b.extend_from_slice(&row[c..]);
        }
        (rle_encode(&a, c, h), rle_encode(&b, w - c, h))
    } else {
        let a = p.idx[..c * w].to_vec();
        let b = p.idx[c * w..].to_vec();
        (rle_encode(&a, w, c), rle_encode(&b, w, h - c))
    }
}

/// PDS 是否把 0xFF 用作非透明色。PGS 规范要求 0xFF 是全透明保留项，
/// SUPer 的 `is_compliant` 也会据此判不合规。
fn palette_has_opaque_ff(entries: &[[u8; 5]]) -> bool {
    entries.iter().any(|e| e[0] == 0xFF && e[4] != 0)
}

/// 精确统计一组 RLE 的唯一内容数与去重后的字节总量。
/// 只做长度 + 逐字节比较，不引入哈希：误判「相同」会直接复用错对象导致画面错误。
fn uniq_stats(items: &[Vec<u8>]) -> (usize, usize) {
    let mut uniq: Vec<&[u8]> = Vec::new();
    for x in items {
        if !uniq.iter().any(|u| *u == x.as_slice()) {
            uniq.push(x.as_slice());
        }
    }
    (uniq.len(), uniq.iter().map(|u| u.len()).sum())
}

/// 在候选分割线里挑净收益最大的一条。
/// 返回 `(horizontal, cut, gain_bytes, unique_parts, unique_area)`。
fn best_split(pngs: &[Png]) -> Option<(bool, u32, isize, usize, u64)> {
    if pngs.len() < 2 {
        return None;
    }
    let w = pngs[0].w;
    let h = pngs[0].h;
    if w < 2 || h < 2 {
        return None;
    }
    let full: Vec<Vec<u8>> = pngs
        .iter()
        .map(|p| rle_encode(&p.idx, p.w as usize, p.h as usize))
        .collect();
    let (uf, bf) = uniq_stats(&full);
    // 每个唯一对象约 24 字节固定开销（段头 13 + ODS 首片头 11）
    let base_cost = uf * 24 + bf;

    let mut cuts: Vec<(bool, u32)> = Vec::new();
    for &f in &[0.25_f64, 1.0 / 3.0, 0.5, 2.0 / 3.0, 0.75] {
        let cx = ((w as f64 * f).round() as u32).clamp(1, w - 1);
        let cy = ((h as f64 * f).round() as u32).clamp(1, h - 1);
        if !cuts.contains(&(true, cx)) {
            cuts.push((true, cx));
        }
        if !cuts.contains(&(false, cy)) {
            cuts.push((false, cy));
        }
    }

    let mut best: Option<(bool, u32, isize, usize, u64)> = None;
    for (horiz, cut) in cuts {
        let mut a_rles = Vec::with_capacity(pngs.len());
        let mut b_rles = Vec::with_capacity(pngs.len());
        for p in pngs {
            let (a, b) = split_rle(p, horiz, cut);
            a_rles.push(a);
            b_rles.push(b);
        }
        let (ua, ba) = uniq_stats(&a_rles);
        let (ub, bb) = uniq_stats(&b_rles);
        let (aw, ah) = if horiz { (cut, h) } else { (w, cut) };
        let (bw, bh) = if horiz { (w - cut, h) } else { (w, h - cut) };
        let unique_area =
            ua as u64 * aw as u64 * ah as u64 + ub as u64 * bw as u64 * bh as u64;
        // 每帧多一个 composition object，PCS 里多 8 字节
        let extra = pngs.len() * 8;
        let split_cost = (ua + ub) * 24 + ba + bb + extra;
        let gain = base_cost as isize - split_cost as isize;
        if gain >= SPLIT_MIN_GAIN && best.as_ref().map_or(true, |b| gain > b.2) {
            best = Some((horiz, cut, gain, ua + ub, unique_area));
        }
    }
    best
}

/// 找出 epoch 内可拆分的连续帧链。与调色板动画链互斥（后者优先、更省）。
fn find_split_chains(
    group: &[usize],
    bdn: &Bdn,
    times: &[(u32, u32)],
    dir: &Path,
    fade: &[FadeChain],
) -> Vec<SplitChain> {
    let mut out = Vec::new();
    let mut k = 0usize;
    while k < group.len() {
        if let Some(ch) = fade.iter().find(|c| c.start <= k && k < c.end) {
            k = ch.end;
            continue;
        }
        let ev0 = &bdn.events[group[k]];
        if ev0.gs.len() != 1 {
            k += 1;
            continue;
        }
        let g0 = &ev0.gs[0];
        let mut j = k + 1;
        while j < group.len() && j - k < SPLIT_MAX_FRAMES {
            if fade.iter().any(|c| c.start <= j && j < c.end) {
                break;
            }
            let ei = group[j];
            let ev = &bdn.events[ei];
            if ev.gs.len() != 1 {
                break;
            }
            let g = &ev.gs[0];
            if g.w != g0.w || g.h != g0.h || g.x != g0.x || g.y != g0.y {
                break;
            }
            if times[ei].0 != times[group[j - 1]].1 {
                break;
            }
            j += 1;
        }
        if j - k >= 2 {
            let mut pngs = Vec::with_capacity(j - k);
            let mut ok = true;
            for q in k..j {
                match decode_png(&dir.join(&bdn.events[group[q]].gs[0].file)) {
                    Ok(p) if p.w == g0.w && p.h == g0.h => pngs.push(p),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if let Some((horiz, cut, _gain, up, ua)) = best_split(&pngs) {
                    let mut frames = Vec::with_capacity(pngs.len());
                    for p in &pngs {
                        let (a, b) = split_rle(p, horiz, cut);
                        frames.push((p.plte.clone(), p.trns.clone(), a, b));
                    }
                    out.push(SplitChain {
                        start: k,
                        end: j,
                        horizontal: horiz,
                        cut,
                        w: g0.w,
                        h: g0.h,
                        x: g0.x,
                        y: g0.y,
                        forced: ev0.forced,
                        frames,
                        unique_parts: up,
                        unique_area: ua,
                    });
                    k = j;
                    continue;
                }
            }
        }
        k += 1;
    }
    out
}

struct Ds {
    pts: u32,
    state: u8,
    win: Option<(u32, u32, u32, u32)>,
    /// PDS 条目：(entry_id, Y, Cr, Cb, Alpha)。None 表示本显示集不发 PDS
    pal: Option<Vec<[u8; 5]>>,
    objs: Vec<(u16, u8, u32, u32, Vec<u8>)>,
    refs: Vec<(u16, u32, u32, bool)>,
    comp_no: u16,
    /// PCS 的 palette_update_flag：本显示集只换调色板，composition 不变
    pal_upd: bool,
}

// ---------------------------------------------------------------------------
// 段序列化
// ---------------------------------------------------------------------------

fn push_seg(out: &mut Vec<u8>, typ: u8, pts: u32, dts: u32, payload: &[u8]) -> Result<(), String> {
    if payload.len() > 65535 {
        return Err(format!("{} 段负载 {} 字节超过 65535", typ, payload.len()));
    }
    out.extend_from_slice(b"PG");
    out.extend_from_slice(&pts.to_be_bytes());
    out.extend_from_slice(&dts.to_be_bytes());
    out.push(typ);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

fn clip(v: f64, lo: f64, hi: f64) -> u8 {
    v.round().max(lo).min(hi) as u8
}

/// RGB → (Y, Cr, Cb)，studio range，矩阵与 SUPer display/palette.py 一致
fn rgb_to_ycrcb(c: [u8; 3], bt709: bool) -> (u8, u8, u8) {
    let (r, g, b) = (c[0] as f64, c[1] as f64, c[2] as f64);
    let (y, cb, cr) = if bt709 {
        (
            0.183 * r + 0.614 * g + 0.062 * b + 16.0,
            -0.101 * r - 0.339 * g + 0.439 * b + 128.0,
            0.439 * r - 0.399 * g - 0.040 * b + 128.0,
        )
    } else {
        (
            0.257 * r + 0.504 * g + 0.098 * b + 16.0,
            -0.148 * r - 0.291 * g + 0.439 * b + 128.0,
            0.439 * r - 0.368 * g - 0.071 * b + 128.0,
        )
    };
    (clip(y, 16.0, 235.0), clip(cr, 16.0, 240.0), clip(cb, 16.0, 240.0))
}

fn push_ods(
    out: &mut Vec<u8>,
    pts: u32,
    dts: u32,
    id: u16,
    ver: u8,
    w: u32,
    h: u32,
    rle: &[u8],
    n_ods: &mut usize,
) -> Result<(), String> {
    let total = rle.len();
    let declared = (total + 4) as u32; // 含首片的 width/height 4 字节
    if total <= MAX_FRAG {
        let mut p = Vec::with_capacity(11 + total);
        p.extend_from_slice(&id.to_be_bytes());
        p.push(ver);
        p.push(0xC0); // FIRST | LAST
        p.push((declared >> 16) as u8);
        p.push((declared >> 8) as u8);
        p.push(declared as u8);
        p.extend_from_slice(&(w as u16).to_be_bytes());
        p.extend_from_slice(&(h as u16).to_be_bytes());
        p.extend_from_slice(rle);
        push_seg(out, ODS, pts, dts, &p)?;
        *n_ods += 1;
        return Ok(());
    }
    let mut off = 0usize;
    let mut first = true;
    while off < total {
        let n = MAX_FRAG.min(total - off);
        let last = off + n >= total;
        let mut flag: u8 = 0;
        if first {
            flag |= 0x80;
        }
        if last {
            flag |= 0x40;
        }
        let mut p = Vec::with_capacity(if first { 11 + n } else { 4 + n });
        p.extend_from_slice(&id.to_be_bytes());
        p.push(ver);
        p.push(flag);
        if first {
            p.push((declared >> 16) as u8);
            p.push((declared >> 8) as u8);
            p.push(declared as u8);
            p.extend_from_slice(&(w as u16).to_be_bytes());
            p.extend_from_slice(&(h as u16).to_be_bytes());
        }
        p.extend_from_slice(&rle[off..off + n]);
        push_seg(out, ODS, pts, dts, &p)?;
        *n_ods += 1;
        off += n;
        first = false;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 把 BDN XML + 索引 PNG 编成一条完整 PGS 流，写到 `out`。
///
/// `vw`/`vh` 传 0 时自动按 XML 的 VideoFormat 推断；`fade` 打开调色板渐变、
/// `split` 打开帧间对象拆分（两者都默认开）。
pub fn encode(
    xml: &Path,
    out: &Path,
    vw: u32,
    vh: u32,
    fade: bool,
    split: bool,
) -> Result<Value, String> {
    let bdn = parse_bdn(xml)?;
    let (aw, ah) = video_size(&bdn.vfmt);
    let vw = if vw == 0 { aw } else { vw };
    let vh = if vh == 0 { ah } else { vh };
    let bt709 = vh >= 720;
    let dir = xml.parent().unwrap_or_else(|| Path::new("."));
    let frame_ticks = (PGS_HZ / bdn.fps).round() as u32;
    let plane = vw as f64 * vh as f64;
    // 段时序按 HDMV PGS / SUPer `check_pts_dts_sanity` 的硬约束取提前量：
    //   * Epoch Start 要求 PTS−DTS > 全屏擦除时间 wipe = plane·90k/Rc；
    //   * WDS 要求 PTS−DTS <= 2·wipe，且所有段 PTS−DTS < 1 秒（90000 ticks）；
    //   * 同一显示集内所有段共用一个 DTS，段序列必须全程非递减。
    // 取 wipe + 一帧，同时落在 (wipe, 2·wipe] 内；4K 下也远小于 1 秒。
    // 阶段一用的「清屏+解码+合成最坏上界」在 1080p 是 21251 ticks，会踩
    // WDS 的 2·wipe=11664 红线，故不再用作 DTS 提前量（解码余量改由报告统计）。
    let wipe = (PGS_HZ * plane / RC).ceil() as u32;
    let lead = (wipe.saturating_add(frame_ticks + 1)).clamp(1, 89_999);

    let times = mono_times(&bdn);
    // verifier 要求 `first_pts − lead > prev_pts`，等价于切分处空档 `gap > lead`
    // （严格大于，不是 >=），所以 min_gap 直接传 lead，不要 +1。
    let groups = group_epochs(&bdn, &times, lead);
    let mut dss: Vec<Ds> = Vec::new();
    let mut reused = 0usize;
    let mut n_fade_chains = 0usize;
    let mut n_fade_frames = 0usize;
    let mut n_pal_upd = 0usize;
    let mut n_split_chains = 0usize;
    let mut n_split_frames = 0usize;
    let mut n_split_reused = 0usize;

    for (gi, group) in groups.iter().enumerate() {
        let win = bbox(group, &bdn, vw, vh);
        let mut slots: Vec<Slot> = Vec::new();
        let mut comp_no = 0u16;
        let chains = if fade {
            find_fade_chains(group, &bdn, &times, dir, bt709)
        } else {
            Vec::new()
        };
        n_fade_chains += chains.len();
        // 拆分链与渐变链互斥：渐变只换调色板、更省，优先占用这些帧。
        let splits = if split {
            find_split_chains(group, &bdn, &times, dir, &chains)
        } else {
            Vec::new()
        };
        n_split_chains += splits.len();
        let mut skip_until = 0usize;
        for (k, &ei) in group.iter().enumerate() {
            if k < skip_until {
                continue;
            }

            // ---------- 调色板动画链：一张共用 ODS + 逐帧 PDS ----------
            if let Some(ch) = chains.iter().find(|c| c.start == k) {
                let n = ch.end - ch.start;
                // 共用位图注册成一个普通 object，key 加前缀避免与单帧位图撞车
                let mut key = b"FADE".to_vec();
                key.extend_from_slice(&ch.rle);
                let found = slots.iter().find(|s| s.key == key).map(|s| (s.id, s.ver));
                let (id, ver) = match found {
                    Some(p) => p,
                    None => {
                        let id = slots.len() as u16;
                        slots.push(Slot { id, ver: 0, key });
                        (id, 0)
                    }
                };
                // PU 的 composition objects 必须与链首逐字节相同（SUPer verifier 硬约束）
                let refs = vec![(id, ch.x, ch.y, ch.forced)];
                dss.push(Ds {
                    pts: ticks(times[group[ch.start]].0, bdn.fps),
                    state: if k == 0 { ST_EPOCH } else { ST_ACQ },
                    win: Some(win),
                    pal: Some(ch.pals[0].clone()),
                    objs: vec![(id, ver, ch.w, ch.h, ch.rle.clone())],
                    refs: refs.clone(),
                    comp_no,
                    pal_upd: false,
                });
                comp_no = comp_no.wrapping_add(1);
                for f in 1..n {
                    let diff = palette_diff(&ch.pals[f - 1], &ch.pals[f]);
                    // 差分空说明画面未变；仍发一条全量 PDS 保证版本递增、PDS 非空
                    let pal = if diff.is_empty() { ch.pals[f].clone() } else { diff };
                    dss.push(Ds {
                        pts: ticks(times[group[ch.start + f]].0, bdn.fps),
                        state: ST_NORMAL,
                        win: None,
                        pal: Some(pal),
                        objs: Vec::new(),
                        refs: refs.clone(),
                        comp_no,
                        pal_upd: true,
                    });
                    comp_no = comp_no.wrapping_add(1);
                    n_pal_upd += 1;
                }
                n_fade_frames += n;
                // 链尾按其最后一帧的 OutTC 判断是否补清屏
                let out_pts = ticks(times[group[ch.end - 1]].1, bdn.fps);
                let next_in = if ch.end < group.len() {
                    Some(ticks(times[group[ch.end]].0, bdn.fps))
                } else if gi + 1 < groups.len() {
                    Some(ticks(times[groups[gi + 1][0]].0, bdn.fps))
                } else {
                    None
                };
                let need_clear = match next_in {
                    Some(nx) => nx > out_pts,
                    None => true,
                };
                if need_clear {
                    dss.push(Ds {
                        pts: out_pts,
                        state: ST_NORMAL,
                        win: None,
                        pal: None,
                        objs: Vec::new(),
                        refs: Vec::new(),
                        comp_no,
                        pal_upd: false,
                    });
                    comp_no = comp_no.wrapping_add(1);
                }
                skip_until = ch.end;
                continue;
            }

            // ---------- 帧间对象拆分链：一张画面拆成 2 个对象，逐块复用 ----------
            if let Some(sc) = splits.iter().find(|c| c.start == k) {
                // 拆分会让唯一对象数与面积都增长，先按 group 预算核算，
                // 不够就整链回退到普通单对象路径，绝不产出超 buffer 的流。
                let group_area: u64 = group
                    .iter()
                    .map(|&i| {
                        bdn.events[i]
                            .gs
                            .iter()
                            .map(|g| g.w as u64 * g.h as u64)
                            .sum::<u64>()
                    })
                    .sum();
                let budget_ok = slots.len() + sc.unique_parts <= MAX_OBJ_IDS
                    && group_area + sc.unique_area <= OBJ_BUDGET;
                if budget_ok {
                    let (aw, ah, bw, bh) = if sc.horizontal {
                        (sc.cut, sc.h, sc.w - sc.cut, sc.h)
                    } else {
                        (sc.w, sc.cut, sc.w, sc.h - sc.cut)
                    };
                    let (bx, by) = if sc.horizontal {
                        (sc.x + sc.cut, sc.y)
                    } else {
                        (sc.x, sc.y + sc.cut)
                    };
                    for (f, (plte, trns, ra, rb)) in sc.frames.iter().enumerate() {
                        let parts: [(u32, u32, u32, u32, &Vec<u8>); 2] = [
                            (aw, ah, sc.x, sc.y, ra),
                            (bw, bh, bx, by, rb),
                        ];
                        let mut objs = Vec::new();
                        let mut refs = Vec::new();
                        for (pw, ph, px, py, rle) in parts {
                            // key 含 PLTE/tRNS：只有调色板完全一致才可能复用，
                            // 保证被复用对象的索引仍能被本帧 PDS 正确解释。
                            let key = object_key(plte, trns, rle);
                            let found = slots.iter().find(|s| s.key == key).map(|s| s.id);
                            let id = match found {
                                Some(p) => {
                                    n_split_reused += 1;
                                    p
                                }
                                None => {
                                    let id = slots.len() as u16;
                                    slots.push(Slot { id, ver: 0, key });
                                    objs.push((id, 0u8, pw, ph, rle.to_vec()));
                                    id
                                }
                            };
                            refs.push((id, px, py, sc.forced));
                        }
                        // 链内后续帧即便定义了新 ODS 也走 Normal Case：
                        // SUPer verifier 明确「Acquisition Point 之后不得访问此前的
                        // 对象缓冲」（会清空 ods_filled 并报 expired memory），而我们
                        // 正要复用链首定义好的恒定半区。Normal Case 不清空缓冲，
                        // 且允许同集定义新对象，正好满足跨帧复用。
                        let state = if k == 0 && f == 0 { ST_EPOCH } else { ST_NORMAL };
                        dss.push(Ds {
                            pts: ticks(times[group[sc.start + f]].0, bdn.fps),
                            state,
                            win: if state == ST_NORMAL { None } else { Some(win) },
                            pal: Some(palette_entries(plte, trns, bt709)),
                            objs,
                            refs,
                            comp_no,
                            pal_upd: false,
                        });
                        comp_no = comp_no.wrapping_add(1);
                        n_split_frames += 1;
                    }
                    // 链内时间首尾相接，只在链尾按 OutTC 判断是否补清屏
                    let out_pts = ticks(times[group[sc.end - 1]].1, bdn.fps);
                    let next_in = if sc.end < group.len() {
                        Some(ticks(times[group[sc.end]].0, bdn.fps))
                    } else if gi + 1 < groups.len() {
                        Some(ticks(times[groups[gi + 1][0]].0, bdn.fps))
                    } else {
                        None
                    };
                    let need_clear = match next_in {
                        Some(nx) => nx > out_pts,
                        None => true,
                    };
                    if need_clear {
                        dss.push(Ds {
                            pts: out_pts,
                            state: ST_NORMAL,
                            win: None,
                            pal: None,
                            objs: Vec::new(),
                            refs: Vec::new(),
                            comp_no,
                            pal_upd: false,
                        });
                        comp_no = comp_no.wrapping_add(1);
                    }
                    skip_until = sc.end;
                    continue;
                }
            }

            // ---------- 普通单事件 ----------
            let ev = &bdn.events[ei];
            let mut bmps: Vec<(Bmp, Vec<[u8; 3]>, Vec<u8>)> = Vec::new();
            for g in &ev.gs {
                let png = decode_png(&dir.join(&g.file))?;
                if png.w != g.w || png.h != g.h {
                    return Err(format!(
                        "{} 实际尺寸 {}x{} 与 BDN 声明的 {}x{} 不符",
                        g.file, png.w, png.h, g.w, g.h
                    ));
                }
                let rle = rle_encode(&png.idx, png.w as usize, png.h as usize);
                bmps.push((
                    Bmp { w: png.w, h: png.h, x: g.x, y: g.y, forced: ev.forced, rle },
                    png.plte,
                    png.trns,
                ));
            }
            if bmps.is_empty() {
                continue;
            }
            // PCS 只有一个 palette_id，同一显示集内所有对象必须共用调色板。
            // avs2bdnxml 的每段同源，实测双 Graphic 事件的 PLTE/tRNS 完全相同。
            let pal = (bmps[0].1.clone(), bmps[0].2.clone());
            for (b, p, t) in &bmps {
                if p != &pal.0 || t != &pal.1 {
                    return Err(format!(
                        "事件 {} 的多个 Graphic 调色板不一致，无法共用 PDS",
                        ei
                    ));
                }
                let _ = b;
            }
            if bmps.len() > 2 {
                return Err(format!(
                    "事件 {} 有 {} 个 Graphic，PGS 一个显示集最多 2 个 composition object",
                    ei,
                    bmps.len()
                ));
            }

            let mut objs = Vec::new();
            let mut refs = Vec::new();
            for (b, _p, _t) in &bmps {
                let key = object_key(&pal.0, &pal.1, &b.rle);
                // 先取出 (id, ver)，不要跨 match 持有 slots 的不可变借用
                let found = slots.iter().find(|s| s.key == key).map(|s| (s.id, s.ver));
                let (id, ver, fresh) = match found {
                    Some(pair) => (pair.0, pair.1, false),
                    None => {
                        let id = slots.len() as u16;
                        let ver = 0u8;
                        slots.push(Slot { id, ver, key });
                        (id, ver, true)
                    }
                };
                if fresh {
                    objs.push((id, ver, b.w, b.h, b.rle.clone()));
                } else {
                    reused += 1;
                }
                refs.push((id, b.x, b.y, b.forced));
            }
            let state = if k == 0 {
                ST_EPOCH
            } else if objs.is_empty() {
                ST_NORMAL
            } else {
                ST_ACQ
            };
            let (inf, outf) = times[ei];
            let in_pts = ticks(inf, bdn.fps);
            let out_pts = ticks(outf, bdn.fps);
            dss.push(Ds {
                pts: in_pts,
                state,
                win: if state == ST_NORMAL { None } else { Some(win) },
                pal: Some(palette_entries(&pal.0, &pal.1, bt709)),
                objs,
                refs,
                comp_no,
                pal_upd: false,
            });
            comp_no = comp_no.wrapping_add(1);

            // 事件之间（或全片末尾）有空档就补一个清屏显示集。
            // 紧接的下一个显示集会直接替换画面，不需要清屏。
            let next_in = if k + 1 < group.len() {
                Some(ticks(times[group[k + 1]].0, bdn.fps))
            } else if gi + 1 < groups.len() {
                Some(ticks(times[groups[gi + 1][0]].0, bdn.fps))
            } else {
                None
            };
            let need_clear = match next_in {
                Some(nx) => nx > out_pts,
                None => true,
            };
            if need_clear {
                dss.push(Ds {
                    pts: out_pts,
                    state: ST_NORMAL,
                    win: None,
                    pal: None,
                    objs: Vec::new(),
                    refs: Vec::new(),
                    comp_no,
                    pal_upd: false,
                });
                comp_no = comp_no.wrapping_add(1);
            }
        }
    }
    if dss.is_empty() {
        return Err("没有生成任何显示集".into());
    }

    // PTS 严格递增：单调化后仍有极少数取整边界会撞车，级联抬 1 tick 即可，
    // 既消除 PTS_DUP 也不会让相邻显示集挤到同一帧。
    for i in 1..dss.len() {
        if dss[i].pts <= dss[i - 1].pts {
            dss[i].pts = dss[i - 1].pts.saturating_add(1);
        }
    }

    // 带 WDS 的显示集要求「与前一显示集的 PTS 间隔 > 窗口擦除时间
    // pts_delta = 窗口面积·90k/Rc」（SUPer check_pts_dts_sanity）。逐帧连续的
    // 大窗口素材物理上凑不出这个间隔（一帧 3753 ticks，全屏窗口擦除要 5832）。
    // 这类 Acquisition Point 就省略 WDS，并降级为 Normal Case —— SUPer 的
    // test_diplayset 规定「非 Normal Case 必须带 WDS」，而 Normal Case 允许沿用
    // Epoch Start 的窗口定义、允许同集定义新对象，时序也改走「至少一帧间隔」。
    // Epoch Start 必须保留 WDS，其前方空档已由 group_epochs 保证 > lead > pts_delta。
    let mut n_wds_dropped = 0usize;
    for i in 1..dss.len() {
        if dss[i].state == ST_EPOCH {
            continue;
        }
        if let Some((_x, _y, w, h)) = dss[i].win {
            let gap = dss[i].pts.saturating_sub(dss[i - 1].pts);
            let pts_delta = (PGS_HZ * (w as f64 * h as f64) / RC).ceil() as u32;
            if gap <= pts_delta {
                dss[i].win = None;
                if dss[i].state == ST_ACQ {
                    dss[i].state = ST_NORMAL;
                }
                n_wds_dropped += 1;
            }
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut pal_ver = 0u8;
    let mut n_pcs = 0usize;
    let mut n_wds = 0usize;
    let mut n_pds = 0usize;
    let mut n_ods = 0usize;
    let mut n_end = 0usize;
    for d in &dss {
        let pts = d.pts;
        // 整个显示集的所有段共用一个 DTS（= base）。PDS / END 在 PGS 里
        // 的 DTS 被解码器与 SUPer 的 PDS.__init__/END.__init__ 强制等于其 PTS，
        // 所以它们的 PTS 也必须取 base；否则会被读成「显示集 PTS」并夹在
        // ODS（DTS=base）之间，令全局 DTS 序列回退 —— 这正是旧实现写
        // DTS=0 时 SUPer `check_pts_dts_sanity` 判 out of spec 的根因。
        // 只有 PCS/WDS/ODS 的 PTS 取呈现时刻 pts，形成 ref.sup 的排布模板：
        //   DTS: base base base base base   PTS: pts pts base pts base
        let base = pts.saturating_sub(lead).max(1);
        let ctl_pts = base;
        let ds_start = buf.len();

        let mut p = Vec::with_capacity(11 + 8 * d.refs.len());
        p.extend_from_slice(&(vw as u16).to_be_bytes());
        p.extend_from_slice(&(vh as u16).to_be_bytes());
        p.push(bdn.fps_id << 4);
        p.extend_from_slice(&d.comp_no.to_be_bytes());
        p.push(d.state << 6);
        p.push(if d.pal_upd { 0x80 } else { 0u8 }); // palette_update_flag
        p.push(0u8); // palette_id = 0
        p.push(d.refs.len() as u8);
        for &(id, x, y, forced) in &d.refs {
            p.extend_from_slice(&id.to_be_bytes());
            p.push(0u8); // window_id = 0
            p.push(if forced { 0x40 } else { 0x00 });
            p.extend_from_slice(&(x as u16).to_be_bytes());
            p.extend_from_slice(&(y as u16).to_be_bytes());
        }
        push_seg(&mut buf, PCS, pts, base, &p)?;
        n_pcs += 1;

        // epoch 内 WDS 字节恒定：Epoch Start / Acquisition 都带，Normal 不带。
        if let Some((x, y, w, h)) = d.win {
            let mut q = vec![1u8, 0u8];
            q.extend_from_slice(&(x as u16).to_be_bytes());
            q.extend_from_slice(&(y as u16).to_be_bytes());
            q.extend_from_slice(&(w as u16).to_be_bytes());
            q.extend_from_slice(&(h as u16).to_be_bytes());
            push_seg(&mut buf, WDS, pts, base, &q)?;
            n_wds += 1;
        }
        // Acquisition / Epoch Start 会重置解码器里的调色板状态，版本重新从 0 起算；
        // 其余显示集沿用递增版本号（SUPer verifier 要求同一 palette_id 逐次 +1）。
        if d.state != ST_NORMAL {
            pal_ver = 0;
        }
        if let Some(entries) = &d.pal {
            // PGS 规范要求 palette entry 0xFF 是全透明保留项。SUPer verifier
            // 会据此判不合规；输入 PNG 若把 0xFF 当可见色，宁可报错也不产出坏流。
            if palette_has_opaque_ff(entries) {
                return Err(format!(
                    "显示集 #{} 的 PDS 把 0xFF 用作非透明色；PGS 要求 0xFF 为全透明保留项（输入 PNG 颜色数应 ≤255）",
                    n_pcs
                ));
            }
            let mut q = Vec::with_capacity(2 + entries.len() * 5);
            q.push(0u8); // palette_id
            q.push(pal_ver);
            for e in entries {
                q.push(e[0]); // entry_id
                q.push(e[1]); // Y
                q.push(e[2]); // Cr
                q.push(e[3]); // Cb
                q.push(e[4]); // alpha
            }
            push_seg(&mut buf, PDS, ctl_pts, base, &q)?;
            pal_ver = pal_ver.wrapping_add(1);
            n_pds += 1;
        }
        for (id, ver, w, h, rle) in &d.objs {
            push_ods(&mut buf, pts, base, *id, *ver, *w, *h, rle, &mut n_ods)?;
        }
        push_seg(&mut buf, END, ctl_pts, base, &[])?;
        n_end += 1;

        if buf.len() - ds_start > 1 << 20 {
            return Err(format!(
                "显示集 #{} 有 {} 字节，超过 RX 缓冲 1 MiB",
                n_pcs,
                buf.len() - ds_start
            ));
        }
    }

    fs::write(out, &buf).map_err(|e| format!("写 SUP 失败 {}: {}", out.display(), e))?;
    Ok(json!({
        "ok": true,
        "sup": out.to_string_lossy(),
        "bytes": buf.len(),
        "display_sets": dss.len(),
        "epochs": groups.len(),
        "pcs": n_pcs, "wds": n_wds, "pds": n_pds, "ods": n_ods, "end": n_end,
        "events": bdn.events.len(),
        "objects_reused": reused,
        "palette_updates": n_pal_upd,
        "fade_chains": n_fade_chains,
        "fade_frames": n_fade_frames,
        "split_chains": n_split_chains,
        "split_frames": n_split_frames,
        "split_objects_reused": n_split_reused,
        // 因「相邻 PTS 间隔 <= 窗口擦除时间」而省略 WDS 的 Acquisition Point 数
        "wds_dropped": n_wds_dropped,
        "video": format!("{}x{}", vw, vh),
        "fps": bdn.fps,
        "lead_ticks": lead,
        // 仅供诊断：按 Rd 估算的全屏对象理论上界。它可能大于 lead —— 这是
        // HDMV PGS 的 display set buffering 模型，提前量不必覆盖单帧最坏解码时间。
        "decode_upper_ticks": (PGS_HZ * plane / RD).ceil() as u32,
    }))
}

// ---------------------------------------------------------------------------
// 对拍基准：把 BDN XML + 索引 PNG 还原成「事件 → 期望画面」
// ---------------------------------------------------------------------------

/// 与 `pgsdec` 解码出的帧同域（YCbCr + alpha 的稀疏像素），供
/// `--engine baseline --xml` 做源头对拍。返回 (视频宽, 视频高, fps, 帧列表)。
pub struct SrcFrame {
    /// BDN 事件起点换算的 PTS（90 kHz）
    pub pts: u32,
    /// (y * 视频宽 + x, [Y, Cr, Cb, A])，只含 alpha>0 的像素，按 key 升序
    pub px: Vec<(u32, [u8; 4])>,
}

pub fn source_frames(xml: &Path) -> Result<(u32, u32, f64, Vec<SrcFrame>), String> {
    let bdn = parse_bdn(xml)?;
    let (vw, vh) = video_size(&bdn.vfmt);
    let bt709 = vh >= 720;
    let dir = xml.parent().unwrap_or_else(|| Path::new("."));
    let times = mono_times(&bdn);
    let mut out: Vec<SrcFrame> = Vec::with_capacity(bdn.events.len());
    for (i, ev) in bdn.events.iter().enumerate() {
        let mut px: Vec<(u32, [u8; 4])> = Vec::new();
        for g in &ev.gs {
            let png = decode_png(&dir.join(&g.file))?;
            let pal = palette_entries(&png.plte, &png.trns, bt709);
            for py in 0..png.h {
                let yy = g.y.saturating_add(py);
                if yy >= vh {
                    break;
                }
                for pxx in 0..png.w {
                    let xx = g.x.saturating_add(pxx);
                    if xx >= vw {
                        continue;
                    }
                    let e = png.idx.get((py * png.w + pxx) as usize).copied().unwrap_or(0) as usize;
                    let entry = match pal.get(e) {
                        Some(v) => v,
                        None => continue,
                    };
                    if entry[4] == 0 {
                        continue;
                    }
                    px.push((yy * vw + xx, [entry[1], entry[2], entry[3], entry[4]]));
                }
            }
        }
        px.sort_by_key(|p| p.0);
        px.reverse();
        px.dedup_by_key(|p| p.0);
        px.reverse();
        out.push(SrcFrame { pts: ticks(times[i].0, bdn.fps), px });
    }
    Ok((vw, vh, bdn.fps, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_roundtrip() {
        let w = 197usize;
        let h = 13usize;
        let mut px = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                px[y * w + x] = match (x / 7 + y) % 5 {
                    0 => 0,
                    1 => 1,
                    2 => 200,
                    3 => 200,
                    _ => 7,
                };
            }
        }
        // 造一段 >63 的长透明行程
        for x in 10..180 {
            px[2 * w + x] = 0;
        }
        let enc = rle_encode(&px, w, h);
        assert_eq!(rle_decode(&enc, w, h), px);
    }

    #[test]
    fn attrs_and_timecode() {
        let tag = r#"<Event Forced="False" InTC="00:00:06:00" OutTC="00:00:08:05">"#;
        assert_eq!(attr(tag, "InTC"), Some("00:00:06:00"));
        assert_eq!(attr(tag, "Forced"), Some("False"));
        assert_eq!(attr(tag, "Nope"), None);
        let fps = parse_fps("23.976");
        assert_eq!(tc_frames("00:00:06:00", fps), 144);
        assert!((ticks(144, fps) as i64 - 540_540).abs() < 10);
        assert_eq!(fps_id(fps), 1);
        assert_eq!(video_size("1080p"), (1920, 1080));
    }

    #[test]
    fn bdn_tag_boundaries() {
        // 回归：Description 里的 <Events .../> 不能被当成事件，
        // FirstEventInTC / LastEventOutTC 也不能被 attr 当成 InTC / OutTC。
        let desc = r#"<Events LastEventOutTC="00:02:51:20" FirstEventInTC="00:00:06:00" NumberofEvents="129" Type="Graphic"/>"#;
        assert_eq!(attr(desc, "InTC"), None);
        assert_eq!(attr(desc, "OutTC"), None);
        assert!(find_tag(desc, "<Event", 0).is_none());
        assert_eq!(find_tag("<Events><Event Forced=\"False\">", "<Event", 0), Some(8));
    }

    #[test]
    fn rgb_conversion_range() {
        let (y, cr, cb) = rgb_to_ycrcb([255, 255, 255], true);
        assert!(y >= 16 && y <= 235);
        assert!(cr >= 16 && cr <= 240);
        assert!(cb >= 16 && cb <= 240);
    }

    /// 把 RGBA 帧转成 PGS 调色板域（YCrCb + alpha），与 build_fade_chain 的输出同域
    fn fade_domain(frames: &[Vec<[u8; 4]>], bt709: bool) -> Vec<Vec<[u8; 4]>> {
        frames
            .iter()
            .map(|f| {
                f.iter()
                    .map(|c| {
                        let (y, cr, cb) = rgb_to_ycrcb([c[0], c[1], c[2]], bt709);
                        [y, cr, cb, c[3]]
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn fade_chain_is_lossless() {
        // 3 帧淡入：同一张「文字」位图，alpha 逐级抬升，另有一块恒定色
        let w = 9usize;
        let h = 3usize;
        let np = w * h;
        let mk = |a: u8| -> Vec<[u8; 4]> {
            (0..np)
                .map(|p| {
                    if p % 7 == 0 {
                        [0, 0, 0, 0] // 全透明
                    } else if p % 3 == 0 {
                        [200, 30, 40, a] // 淡入主色
                    } else {
                        [16, 180, 90, 255] // 恒定色
                    }
                })
                .collect()
        };
        let frames = vec![mk(0), mk(96), mk(255)];
        let want = fade_domain(&frames, true);
        let (bitmap, pals) = build_fade_chain(&frames, w as u32, h as u32, true).unwrap();
        assert_eq!(bitmap.len(), np);
        assert_eq!(pals.len(), 3);
        // 共用位图 + 每帧调色板必须逐像素、逐帧还原出完全相同的调色板域颜色
        for t in 0..3 {
            for p in 0..np {
                let id = bitmap[p] as usize;
                let e = pals[t].iter().find(|e| e[0] as usize == id).unwrap();
                assert_eq!([e[1], e[2], e[3], e[4]], want[t][p], "frame {t} pixel {p}");
            }
        }
        // 全透明像素必须落在 entry 0
        for p in (0..np).step_by(7) {
            assert_eq!(bitmap[p], 0);
        }
        // 序列种类远小于容量：非透明序列应为 2 种
        let uniq: std::collections::HashSet<u8> = bitmap.iter().copied().collect();
        assert_eq!(uniq.len(), 3);
    }

    #[test]
    fn fade_chain_over_capacity_falls_back() {
        // 260 种各不相同的颜色序列 → 超过 255，必须回退
        let frames: Vec<Vec<[u8; 4]>> = (0..2)
            .map(|t| {
                (0..300u32)
                    .map(|i| [(i % 256) as u8, (i / 256) as u8, t as u8, 255])
                    .collect()
            })
            .collect();
        assert!(build_fade_chain(&frames, 300, 1, false).is_none());
    }

    #[test]
    fn palette_diff_roundtrip() {
        let a = vec![[0u8, 16, 128, 128, 0], [1, 200, 100, 90, 255], [2, 30, 40, 50, 128]];
        let b = vec![[0u8, 16, 128, 128, 0], [1, 200, 100, 90, 200], [2, 30, 40, 50, 128]];
        let diff = palette_diff(&a, &b);
        assert_eq!(diff, vec![[1u8, 200, 100, 90, 200]]);
        // 把差分覆盖回累积调色板，应还原出 b
        let mut acc = a.clone();
        for e in diff {
            if let Some(slot) = acc.iter_mut().find(|s| s[0] == e[0]) {
                *slot = e;
            } else {
                acc.push(e);
            }
        }
        assert_eq!(acc, b);
        assert!(palette_diff(&a, &a).is_empty());
    }

    fn mk_png(w: u32, h: u32, fill: impl Fn(u32, u32) -> u8) -> Png {
        let mut idx = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                idx.push(fill(x, y));
            }
        }
        Png {
            w,
            h,
            plte: vec![[0, 0, 0], [255, 255, 255], [200, 30, 40], [30, 200, 90]],
            trns: vec![0, 255, 255, 255],
            idx,
        }
    }

    #[test]
    fn split_rle_lossless() {
        let p = mk_png(40, 12, |x, y| {
            if (x / 3 + y / 2) % 4 == 0 {
                1
            } else if x < 20 && y > 8 {
                2
            } else if x >= 20 && y < 3 {
                3
            } else {
                0
            }
        });
        for (horiz, cut) in [(true, 13u32), (true, 20), (false, 4), (false, 7)] {
            let (a, b) = split_rle(&p, horiz, cut);
            let w = p.w as usize;
            let h = p.h as usize;
            let c = cut as usize;
            let (aw, ah, bw, bh) = if horiz {
                (c, h, w - c, h)
            } else {
                (w, c, w, h - c)
            };
            let da = rle_decode(&a, aw, ah);
            let db = rle_decode(&b, bw, bh);
            let mut back = vec![0u8; w * h];
            if horiz {
                for y in 0..h {
                    back[y * w..y * w + c].copy_from_slice(&da[y * c..y * c + c]);
                    back[y * w + c..y * w + w]
                        .copy_from_slice(&db[y * (w - c)..y * (w - c) + w - c]);
                }
            } else {
                back[..c * w].copy_from_slice(&da);
                back[c * w..].copy_from_slice(&db);
            }
            assert_eq!(back, p.idx, "horiz={} cut={}", horiz, cut);
        }
    }

    #[test]
    fn split_chain_picks_stable_half() {
        // 8 帧：左半完全恒定，右半一根逐帧右移的竖条 → 整帧帧帧不同，
        // 现有「整帧复用」帮不上忙，但水平中线能把恒定半区单独摘出来复用。
        let pngs: Vec<Png> = (0..8u32)
            .map(|t| {
                let bar = (t * 4) % 32;
                mk_png(64, 24, |x, _y| {
                    if x < 32 {
                        1
                    } else if x >= 32 + bar && x < 36 + bar {
                        2
                    } else {
                        3
                    }
                })
            })
            .collect();
        let (horiz, cut, gain, _up, _area) =
            best_split(&pngs).expect("局部变化应存在正收益的分割线");
        assert!(horiz, "应选左右切");
        assert_eq!(cut, 32);
        assert!(gain > 0);
        let mut a_rles = Vec::new();
        for p in &pngs {
            a_rles.push(split_rle(p, horiz, cut).0);
        }
        assert_eq!(uniq_stats(&a_rles).0, 1, "恒定半区应只剩 1 个唯一块");
    }

    #[test]
    fn split_chain_no_gain_falls_back() {
        // 两帧整幅都在变，任何分割线都无净收益 → 不拆分
        let pngs: Vec<Png> = (0..2u32)
            .map(|t| mk_png(32, 12, |x, y| ((x + y + t * 5) % 4) as u8))
            .collect();
        assert!(best_split(&pngs).is_none(), "整幅都变时不应拆分");
    }

    #[test]
    fn uniq_stats_is_exact() {
        let items = vec![vec![1u8, 2, 3], vec![1, 2, 3], vec![1, 2, 4], vec![9u8]];
        let (n, bytes) = uniq_stats(&items);
        assert_eq!(n, 3);
        assert_eq!(bytes, 3 + 3 + 1);
    }

    #[test]
    fn palette_ff_must_be_transparent() {
        // 正常 ≤255 色、0xFF 全透明、0xFF 缺失，都应放行
        assert!(!palette_has_opaque_ff(&[[0u8, 16, 128, 128, 0], [254, 200, 100, 90, 255]]));
        assert!(!palette_has_opaque_ff(&[[255u8, 0, 128, 128, 0]]));
        // 0xFF 被当成可见色 → 必须拦下
        assert!(palette_has_opaque_ff(&[[255u8, 200, 100, 90, 255]]));
    }
}
