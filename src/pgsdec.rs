//! HDMV PGS 软件解码器 —— 对拍基准里的「播放器模型」
//!
//! 把任意 `.sup`（不管出自 avs2bdnxml、SUPer 还是本项目原生编码器）还原成
//! 「显示集 PTS → 屏幕画面」的帧序列。对拍不比字节，只比呈现结果：编码策略
//! （epoch 划分、对象复用、调色板分配）可以完全不同，只要同一时刻屏幕上的
//! 像素一致，真机播放就应当一致。
//!
//! 画面用 PGS 原生域 `[Y, Cr, Cb, Alpha]` 的稀疏像素表示，不做 YCbCr→RGB
//! 往返，避免把转换舍入当成编码误差。透明像素不入库（PGS 里透明色的 RGB
//! 没有意义），所以「两帧 hash 相同」等价于「所有可见像素逐通道完全一致」。
//!
//! 模型约定（与 HDMV PGS / ffmpeg pgssubdec / `src/pgs.rs` 一致）：
//!   * Epoch Start 清空对象缓冲、调色板、窗口与画面构成；Acquisition Point /
//!     Normal Case 保留 epoch 内已定义的对象（这样才能复用 object_id）；
//!   * palette update 显示集沿用上一集的 composition objects，只换调色板；
//!   * 每个 END 段提交一次画面更新；composition objects 为空就是清屏帧；
//!   * 对象按 PCS 给的绝对 (x, y) 摆放，后画的覆盖先画的。
//!
//! **第一版的已知简化**：不按 WDS 窗口做像素裁剪（同一 BDN 来源的流里对象都
//! 落在窗口内，裁剪与否结果相同）；不做 `cropped` 对象的裁剪偏移。窗口定义
//! 仍被解析并统计，后续版本可以补上完整裁剪语义。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

pub const PGS_HZ: f64 = 90_000.0;

const PCS: u8 = 0x16;
const WDS: u8 = 0x17;
const PDS: u8 = 0x14;
const ODS: u8 = 0x15;
const END: u8 = 0x80;

/// 一帧的元信息（不驻留像素，便于整片流只存摘要）
#[derive(Clone, Debug)]
pub struct FrameMeta {
    pub pts: u32,
    /// PCS composition_state 原值：0x80 Epoch Start / 0x40 Acquisition / 0x00 Normal
    pub state: u8,
    pub pal_upd: bool,
    pub nref: usize,
    pub comp_no: u16,
    /// 规范化画面的 FNV-1a 64：只对 alpha>0 的像素，按 (x,y) 升序
    pub hash: u64,
    pub opaque: usize,
    /// [x, y, w, h]，空帧为全 0
    pub bbox: [u32; 4],
    /// PCS 引用了但对象缓冲里没有的 object_id 个数
    pub undefined_refs: usize,
    /// PDS 里查不到的调色板条目引用次数
    pub undefined_pal: usize,
}

impl FrameMeta {
    pub fn is_clear(&self) -> bool {
        self.nref == 0 && self.opaque == 0
    }
    pub fn kind(&self) -> &'static str {
        if self.pal_upd {
            "palette_update"
        } else if self.is_clear() {
            "clear"
        } else if self.state == 0x80 {
            "epoch_start"
        } else if self.state == 0x40 {
            "acquisition"
        } else {
            "normal"
        }
    }
}

pub struct Decoded {
    pub path: String,
    pub bytes: usize,
    pub width: u32,
    pub height: u32,
    pub frames: Vec<FrameMeta>,
    pub display_sets: usize,
    pub epochs: usize,
    pub palette_updates: usize,
    pub incomplete_ds: usize,
    /// 只有 `keep` 指定的帧才保留像素：(帧下标, 排序后的 (key, YCrCbA))
    pub pixels: Vec<(usize, Vec<(u32, [u8; 4])>)>,
    /// 解析出的窗口定义 [id, x, y, w, h]
    pub windows: Vec<[u32; 5]>,
}

struct Seg<'a> {
    typ: u8,
    pts: u32,
    p: &'a [u8],
}

fn rd32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}
fn rd16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([d[o], d[o + 1]])
}

/// 拆段。与 verify.rs 一样容忍 `PG` 边界外的垃圾字节，坏长度直接报错。
fn parse_segments(d: &[u8]) -> Result<Vec<Seg<'_>>, String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 13 <= d.len() {
        if &d[i..i + 2] != b"PG" {
            i += 1;
            continue;
        }
        let typ = d[i + 10];
        let sz = ((d[i + 11] as usize) << 8) | d[i + 12] as usize;
        let end = i + 13 + sz;
        if end > d.len() {
            return Err(format!(
                "偏移 {} 的段声明 {} 字节负载，文件只剩 {} 字节",
                i,
                sz,
                d.len() - i - 13
            ));
        }
        out.push(Seg { typ, pts: rd32(d, i + 2), p: &d[i + 13..end] });
        i = end;
    }
    Ok(out)
}

/// PGS RLE 解码（与 `pgs::rle_encode` 互逆，也兼容 avs2bdnxml / SUPer 的输出）
fn rle_decode(data: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w.saturating_mul(h)];
    let mut k = 0usize;
    let mut y = 0usize;
    let mut x = 0usize;
    while k < data.len() {
        let b = data[k];
        k += 1;
        if b != 0 {
            if y < h && x < w {
                out[y * w + x] = b;
                x += 1;
            }
            continue;
        }
        if k >= data.len() {
            break;
        }
        let n = data[k];
        k += 1;
        if n == 0 {
            y += 1;
            x = 0;
            continue;
        }
        let mut len = (n & 0x3F) as usize;
        if n & 0x40 != 0 {
            if k >= data.len() {
                break;
            }
            len = (len << 8) | data[k] as usize;
            k += 1;
        }
        let color = if n & 0x80 != 0 {
            if k >= data.len() {
                break;
            }
            let c = data[k];
            k += 1;
            c
        } else {
            0
        };
        for _ in 0..len {
            if y < h && x < w {
                out[y * w + x] = color;
            }
            x += 1;
        }
    }
    out
}

struct Obj {
    w: u32,
    h: u32,
    idx: Vec<u8>,
}

struct Frag {
    w: u32,
    h: u32,
    rle: Vec<u8>,
}

struct Cur {
    pts: u32,
    state: u8,
    pal_upd: bool,
    pal_id: u8,
    comp_no: u16,
    /// (object_id, window_id, x, y)
    refs: Vec<(u16, u8, u32, u32)>,
    has_wds: bool,
    n_pds: usize,
    n_ods: usize,
}

fn parse_pcs(p: &[u8]) -> Cur {
    let state = p.get(7).copied().unwrap_or(0) & 0xC0;
    let pal_upd = p.get(8).copied().unwrap_or(0) & 0x80 != 0;
    let pal_id = p.get(9).copied().unwrap_or(0);
    let comp_no = if p.len() >= 7 { rd16(p, 5) } else { 0 };
    let nref = p.get(10).copied().unwrap_or(0) as usize;
    let mut refs = Vec::with_capacity(nref.min(16));
    for k in 0..nref.min(16) {
        let o = 11 + k * 8;
        if o + 8 > p.len() {
            break;
        }
        refs.push((rd16(p, o), p[o + 2], rd16(p, o + 4) as u32, rd16(p, o + 6) as u32));
    }
    Cur {
        pts: 0,
        state,
        pal_upd,
        pal_id,
        comp_no,
        refs,
        has_wds: false,
        n_pds: 0,
        n_ods: 0,
    }
}

fn fnv(h: &mut u64, b: u8) {
    *h ^= b as u64;
    *h = h.wrapping_mul(0x100_0000_01b3);
}

/// 对规范化像素序列算 hash。像素必须已按 key 升序排序、且只含 alpha>0。
pub fn hash_pixels(px: &[(u32, [u8; 4])], stride: u32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in stride.to_le_bytes() {
        fnv(&mut h, b);
    }
    for &(k, v) in px {
        for b in k.to_le_bytes() {
            fnv(&mut h, b);
        }
        for b in v {
            fnv(&mut h, b);
        }
    }
    h
}

/// 解码一条 SUP。`keep` 里的帧下标会额外保留像素，其余只留摘要。
/// 两遍用法：先 `keep=None` 拿全部 hash，再对 hash 不同的帧二次解码取像素。
pub fn decode(path: &Path, keep: Option<&HashSet<usize>>) -> Result<Decoded, String> {
    let data = fs::read(path).map_err(|e| format!("读 SUP 失败 {}: {}", path.display(), e))?;
    let segs = parse_segments(&data)?;
    let mut width = 0u32;
    let mut height = 0u32;

    let mut objects: HashMap<u16, Obj> = HashMap::new();
    let mut frags: HashMap<u16, Frag> = HashMap::new();
    let mut palette: HashMap<(u8, u8), [u8; 4]> = HashMap::new();
    let mut windows: Vec<[u32; 5]> = Vec::new();
    let mut last_refs: Vec<(u16, u8, u32, u32)> = Vec::new();

    let mut frames: Vec<FrameMeta> = Vec::new();
    let mut pixels: Vec<(usize, Vec<(u32, [u8; 4])>)> = Vec::new();
    let mut display_sets = 0usize;
    let mut epochs = 0usize;
    let mut palette_updates = 0usize;
    let mut incomplete_ds = 0usize;
    let mut cur: Option<Cur> = None;

    for s in &segs {
        match s.typ {
            PCS => {
                if cur.is_some() {
                    // 没有 END 的显示集，解码器不会提交这次更新；记下来但不产帧
                    incomplete_ds += 1;
                }
                let mut c = parse_pcs(s.p);
                c.pts = s.pts;
                if s.p.len() >= 5 {
                    if width == 0 {
                        width = rd16(s.p, 0) as u32;
                        height = rd16(s.p, 2) as u32;
                    }
                }
                if c.state == 0x80 {
                    epochs += 1;
                    objects.clear();
                    frags.clear();
                    palette.clear();
                    windows.clear();
                    last_refs.clear();
                }
                if c.pal_upd {
                    palette_updates += 1;
                }
                cur = Some(c);
            }
            WDS => {
                if let Some(c) = cur.as_mut() {
                    c.has_wds = true;
                }
                let p = s.p;
                if !p.is_empty() {
                    let n = (p[0] as usize).min(2);
                    for k in 0..n {
                        let o = 1 + k * 9;
                        if o + 9 > p.len() {
                            break;
                        }
                        let w = [
                            p[o] as u32,
                            rd16(p, o + 1) as u32,
                            rd16(p, o + 3) as u32,
                            rd16(p, o + 5) as u32,
                            rd16(p, o + 7) as u32,
                        ];
                        if !windows.iter().any(|q| q[0] == w[0] && q[1..] == w[1..]) {
                            windows.push(w);
                        }
                    }
                }
            }
            PDS => {
                if let Some(c) = cur.as_mut() {
                    c.n_pds += 1;
                }
                let p = s.p;
                if p.len() >= 2 {
                    let pal_id = p[0];
                    let mut o = 2usize;
                    while o + 5 <= p.len() {
                        palette.insert((pal_id, p[o]), [p[o + 1], p[o + 2], p[o + 3], p[o + 4]]);
                        o += 5;
                    }
                }
            }
            ODS => {
                if let Some(c) = cur.as_mut() {
                    c.n_ods += 1;
                }
                let p = s.p;
                if p.len() < 4 {
                    continue;
                }
                let id = rd16(p, 0);
                let flags = p[3];
                if flags & 0x80 != 0 {
                    if p.len() < 11 {
                        continue;
                    }
                    frags.insert(
                        id,
                        Frag {
                            w: rd16(p, 7) as u32,
                            h: rd16(p, 9) as u32,
                            rle: p[11..].to_vec(),
                        },
                    );
                } else if let Some(f) = frags.get_mut(&id) {
                    f.rle.extend_from_slice(&p[4..]);
                }
                if flags & 0x40 != 0 {
                    if let Some(f) = frags.remove(&id) {
                        if f.w > 0 && f.h > 0 && f.w <= 8192 && f.h <= 8192 {
                            objects.insert(id, Obj {
                                w: f.w,
                                h: f.h,
                                idx: rle_decode(&f.rle, f.w as usize, f.h as usize),
                            });
                        }
                    }
                }
            }
            END => {
                let c = match cur.take() {
                    Some(c) => c,
                    None => continue,
                };
                display_sets += 1;
                let idx = frames.len();
                // palette update 沿用上一次画面构成；本集带了 composition
                // objects 时以本集为准（规范也要求 PU 的 cobjs 与上一集相同）。
                let refs = if !c.refs.is_empty() {
                    c.refs.clone()
                } else if c.pal_upd {
                    last_refs.clone()
                } else {
                    Vec::new()
                };
                if !refs.is_empty() {
                    last_refs = refs.clone();
                }
                let stride = width.max(1);
                let mut px: Vec<(u32, [u8; 4])> = Vec::new();
                let mut undefined_refs = 0usize;
                let mut undefined_pal = 0usize;
                for &(oid, _wid, ox, oy) in &refs {
                    let Some(obj) = objects.get(&oid) else {
                        undefined_refs += 1;
                        continue;
                    };
                    for py in 0..obj.h {
                        let yy = oy.saturating_add(py);
                        if height > 0 && yy >= height {
                            break;
                        }
                        for pxx in 0..obj.w {
                            let xx = ox.saturating_add(pxx);
                            if xx >= stride || (height > 0 && yy >= height) {
                                continue;
                            }
                            let e = obj.idx.get((py * obj.w + pxx) as usize).copied().unwrap_or(0);
                            let Some(col) = palette.get(&(c.pal_id, e)).copied() else {
                                undefined_pal += 1;
                                continue;
                            };
                            if col[3] == 0 {
                                continue;
                            }
                            let k = yy.saturating_mul(stride).saturating_add(xx);
                            px.push((k, col));
                        }
                    }
                }
                px.sort_by_key(|p| p.0);
                // 同坐标后画的覆盖先画的：倒序去重保留最后一次，再翻回升序
                px.reverse();
                px.dedup_by_key(|p| p.0);
                px.reverse();
                let opaque = px.len();
                let hash = hash_pixels(&px, stride);
                let bbox = if px.is_empty() {
                    [0, 0, 0, 0]
                } else {
                    let mut x0 = u32::MAX;
                    let mut y0 = u32::MAX;
                    let mut x1 = 0u32;
                    let mut y1 = 0u32;
                    for &(k, _) in &px {
                        let x = k % stride;
                        let y = k / stride;
                        x0 = x0.min(x);
                        y0 = y0.min(y);
                        x1 = x1.max(x);
                        y1 = y1.max(y);
                    }
                    [x0, y0, x1 - x0 + 1, y1 - y0 + 1]
                };
                frames.push(FrameMeta {
                    pts: c.pts,
                    state: c.state,
                    pal_upd: c.pal_upd,
                    nref: c.refs.len(),
                    comp_no: c.comp_no,
                    hash,
                    opaque,
                    bbox,
                    undefined_refs,
                    undefined_pal,
                });
                if keep.map(|w| w.contains(&idx)).unwrap_or(false) {
                    pixels.push((idx, px));
                }
            }
            _ => {}
        }
    }
    if cur.is_some() {
        incomplete_ds += 1;
    }
    if frames.is_empty() {
        return Err(format!("{} 里没有解码出任何画面帧", path.display()));
    }
    Ok(Decoded {
        path: path.to_string_lossy().to_string(),
        bytes: data.len(),
        width,
        height,
        frames,
        display_sets,
        epochs,
        palette_updates,
        incomplete_ds,
        pixels,
        windows,
    })
}

/// YCrCb(studio range) + alpha → RGBA8。导出参考画面给真机录屏对照时用。
/// `bt709` 由画面高度推断（与 `pgs::encode` 一致：>=720 用 BT.709）。
pub fn ycbcra_to_rgba(c: [u8; 4], bt709: bool) -> [u8; 4] {
    let y = c[0] as f64 - 16.0;
    let cr = c[1] as f64 - 128.0;
    let cb = c[2] as f64 - 128.0;
    let (r, g, b) = if bt709 {
        (
            1.164 * y + 1.793 * cr,
            1.164 * y - 0.213 * cb - 0.533 * cr,
            1.164 * y + 2.112 * cb,
        )
    } else {
        (
            1.164 * y + 1.596 * cr,
            1.164 * y - 0.392 * cb - 0.813 * cr,
            1.164 * y + 2.017 * cb,
        )
    };
    let cl = |v: f64| v.round().max(0.0).min(255.0) as u8;
    [cl(r), cl(g), cl(b), c[3]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(out: &mut Vec<u8>, typ: u8, pts: u32, p: &[u8]) {
        out.extend_from_slice(b"PG");
        out.extend_from_slice(&pts.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.push(typ);
        out.extend_from_slice(&(p.len() as u16).to_be_bytes());
        out.extend_from_slice(p);
    }

    /// 2×2 一帧：epoch start + 一张位图 + 一块 2 色调色板。
    fn mini_sup() -> Vec<u8> {
        let mut d = Vec::new();
        let mut p = Vec::new();
        p.extend_from_slice(&4u16.to_be_bytes()); // w
        p.extend_from_slice(&4u16.to_be_bytes()); // h
        p.push(0x10); // rate
        p.extend_from_slice(&0u16.to_be_bytes()); // comp_no
        p.push(0x80); // epoch start
        p.push(0); // pal_upd
        p.push(0); // palette_id
        p.push(1); // nobj
        p.extend_from_slice(&0u16.to_be_bytes()); // object_id
        p.push(0); // window_id
        p.push(0); // cropped
        p.extend_from_slice(&1u16.to_be_bytes()); // x
        p.extend_from_slice(&1u16.to_be_bytes()); // y
        seg(&mut d, PCS, 90000, &p);

        let mut q = vec![1u8, 0u8];
        q.extend_from_slice(&0u16.to_be_bytes());
        q.extend_from_slice(&0u16.to_be_bytes());
        q.extend_from_slice(&4u16.to_be_bytes());
        q.extend_from_slice(&4u16.to_be_bytes());
        seg(&mut d, WDS, 90000, &q);

        // PDS: entry 0 全透明、entry 1 白色不透明
        let mut r = vec![0u8, 0u8, 0, 16, 128, 128, 0, 1, 235, 128, 128, 255];
        seg(&mut d, PDS, 90000, &r);
        r.clear();

        // ODS: 2×2，第一行 [1,1]、第二行 [0,0]
        // 注意 0x00 0x00 是「行结束」，两个透明像素要写 0x00 0x02。
        let mut o = Vec::new();
        o.extend_from_slice(&0u16.to_be_bytes());
        o.push(0);
        o.push(0xC0);
        let rle: [u8; 8] = [1, 1, 0, 0, 0, 2, 0, 0];
        let declared = (rle.len() + 4) as u32;
        o.push((declared >> 16) as u8);
        o.push((declared >> 8) as u8);
        o.push(declared as u8);
        o.extend_from_slice(&2u16.to_be_bytes());
        o.extend_from_slice(&2u16.to_be_bytes());
        o.extend_from_slice(&rle);
        seg(&mut d, ODS, 90000, &o);
        seg(&mut d, END, 90000, &[]);
        d
    }

    #[test]
    fn decodes_minimal_display_set() {
        let p = std::env::temp_dir().join("supfast_pgsdec_mini.sup");
        fs::write(&p, mini_sup()).unwrap();
        let d = decode(&p, None).unwrap();
        assert_eq!(d.width, 4);
        assert_eq!(d.height, 4);
        assert_eq!(d.display_sets, 1);
        assert_eq!(d.epochs, 1);
        assert_eq!(d.frames.len(), 1);
        let f = &d.frames[0];
        assert_eq!(f.pts, 90000);
        assert_eq!(f.opaque, 2);
        assert_eq!(f.bbox, [1, 1, 2, 1]);
        assert_eq!(f.undefined_refs, 0);
        assert_eq!(f.undefined_pal, 0);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn keep_returns_pixels_and_hash_is_stable() {
        let p = std::env::temp_dir().join("supfast_pgsdec_keep.sup");
        fs::write(&p, mini_sup()).unwrap();
        let mut want = HashSet::new();
        want.insert(0usize);
        let d = decode(&p, Some(&want)).unwrap();
        assert_eq!(d.pixels.len(), 1);
        assert_eq!(d.pixels[0].1.len(), 2);
        assert_eq!(d.pixels[0].1[0].1[3], 255);
        let d2 = decode(&p, None).unwrap();
        assert_eq!(d.frames[0].hash, d2.frames[0].hash);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn clear_display_set_is_an_empty_frame() {
        let p = std::env::temp_dir().join("supfast_pgsdec_clear.sup");
        let mut d = mini_sup();
        // 追加一个 Normal Case 清屏显示集
        let mut q = Vec::new();
        q.extend_from_slice(&4u16.to_be_bytes());
        q.extend_from_slice(&4u16.to_be_bytes());
        q.push(0x10);
        q.extend_from_slice(&1u16.to_be_bytes());
        q.push(0x00);
        q.push(0);
        q.push(0);
        q.push(0);
        seg(&mut d, PCS, 180000, &q);
        seg(&mut d, END, 180000, &[]);
        fs::write(&p, d).unwrap();
        let dec = decode(&p, None).unwrap();
        assert_eq!(dec.frames.len(), 2);
        assert!(dec.frames[1].is_clear());
        assert_eq!(dec.frames[1].opaque, 0);
        let _ = fs::remove_file(&p);
    }
}
