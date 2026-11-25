//! HDMV PGS / SUP 合规自检 —— 只读体检，不改流、不引入任何新依赖
//!
//! 依据 HDMV PGS 解码器模型（见 cubicibo/SUPer 的 `doc/codec.md`）：
//!   * 段头 13 字节：`PG`(2) + PTS(4) + DTS(4) + type(1) + size(2)，90 kHz 时钟
//!   * RX（coded data）缓冲 1 MiB：同一 DTS 取走的段字节总和不得超
//!   * object buffer 4 MiB：一个 epoch 内唯一 object_id 的位图面积总和不得超，且最多 64 个
//!   * 图形解码器 Rd = 16e6 px/s、composition 控制器 Rc = 32e6 px/s
//!   * DTS 非递减且 ≤ PTS；`PTS - DTS < 90000`（PTS 最多超前 1 秒）。
//!     注意：PTS==DTS 是合法的（PES 里此时只写 PTS），PDS 段按规范甚至要求相等，
//!     见 SUPer `bytestream/verifier.py:check_pts_dts_sanity`。以前把它当警告是误报。
//!   * PCS 的 composition_state 高两位：0x80 Epoch Start / 0x40 Acquisition / 0x00 Normal
//!
//! 为什么要有这个：supfast 的 `.sup` 是把每段 avs2bdnxml 的输出重映射 PTS 后拼起来的，
//! 拼缝处的 PTS 单调性、epoch 完整性、缓冲占用从来没验证过。播放器能放 ≠ 合规。
//!
//! 段时序惯例（2026-11 起两条工具链统一）：同一显示集所有段共用一个 DTS =
//! `PTS − lead`，PCS / WDS / ODS 的 PTS 取呈现时刻，PDS / END 的 PTS 也取该 DTS
//! （规范强制 pts == dts）。`sup_retime` 与 `src/pgs.rs` 都是这套模板，SUPer
//! `check_pts_dts_sanity` 全项通过。检查时仍兼容历史流：DTS=0 表示「不适用」的段
//! 排除在 DTS 相关检查之外，旧 avs2bdnxml 把 PDS 的 PTS 写成接近 2^32 的哨兵值，
//! 也不代表时间。
//! 另外 ODS 首片布局是 `id(2) ver(1) flags(1) len(3) w(2) h(2)`（宽高在 [7..11]），
//! 与部分文档写的 4 字节 len 不同，按实际流解析。

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

const PGS_HZ: f64 = 90000.0;

const PCS: u8 = 0x16;
const WDS: u8 = 0x17;
const PDS: u8 = 0x14;
const ODS: u8 = 0x15;
const END: u8 = 0x80;

/// RX / coded data buffer，1 MiB
const RX_BUFFER: u64 = 1 << 20;
/// object buffer，4 MiB
const OBJ_BUFFER: u64 = 4 << 20;
/// 一个 epoch 内最多 64 个唯一 object_id
const MAX_OBJECTS: usize = 64;
/// RLE 位图解码 16e6 px/s，窗口合成 32e6 px/s
const RD: f64 = 16e6;
const RC: f64 = 32e6;
/// 日志里最多逐条列几条，剩下的聚合成一行
const MAX_LINE_ISSUES: usize = 5;

struct Issue {
    level: &'static str,
    code: &'static str,
    msg: String,
}

fn add(v: &mut Vec<Issue>, level: &'static str, code: &'static str, msg: impl Into<String>) {
    v.push(Issue { level, code, msg: msg.into() });
}

fn seg_name(t: u8) -> &'static str {
    match t {
        PCS => "PCS", WDS => "WDS", PDS => "PDS", ODS => "ODS", END => "END", _ => "未知段",
    }
}

/// 90 kHz tick → HH:MM:SS.mmm
fn clock(t: u32) -> String {
    let s = t as f64 / PGS_HZ;
    format!("{:02}:{:02}:{:06.3}", (s / 3600.0) as u32, (s / 60.0) as u32 % 60, s % 60.0)
}

fn kb(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.2} MiB", n as f64 / (1 << 20) as f64)
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

/// ODS 首片里的 object_id 与位图面积；非首片或解析不出来返回 None
fn ods_area(p: &[u8]) -> Option<(u16, u64)> {
    if p.len() < 11 || p[3] & 0x80 == 0 {
        return None;
    }
    let w = u16::from_be_bytes([p[7], p[8]]) as u64;
    let h = u16::from_be_bytes([p[9], p[10]]) as u64;
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    Some((u16::from_be_bytes([p[0], p[1]]), w * h))
}

/// WDS：num_windows(1) + 每窗 id(1) x(2) y(2) w(2) h(2)
fn wds_area(p: &[u8]) -> u64 {
    if p.is_empty() {
        return 0;
    }
    let n = (p[0] as usize).min(2);
    let mut a = 0u64;
    for k in 0..n {
        let o = 1 + k * 9;
        if o + 9 > p.len() {
            break;
        }
        let w = u16::from_be_bytes([p[o + 5], p[o + 6]]) as u64;
        let h = u16::from_be_bytes([p[o + 7], p[o + 8]]) as u64;
        a += w * h;
    }
    a
}

struct Seg<'a> {
    typ: u8,
    pts: u32,
    dts: u32,
    bytes: usize,
    payload: &'a [u8],
    off: usize,
}

struct Ds {
    pts: u32,
    dts: u32,
    state: u8,
    bytes: u64,
    has_end: bool,
    window_area: u64,
    objs: Vec<(u16, u64)>,
    wds: Vec<u8>,
    /// PCS 引用的 (object_id, window_id)
    refs: Vec<(u16, u8)>,
    /// PCS 的 palette_update_flag：本显示集只换调色板
    pal_upd: bool,
    /// PCS 里 composition objects 的原始字节（用于逐字节比较连续性）
    cobjs: Vec<u8>,
    /// PCS 里 composition objects 的个数
    nref: usize,
    /// PCS 里每个 composition object 的 (object_id, x, y)
    cobj_xy: Vec<(u16, u32, u32)>,
    /// PCS 指定的 palette_id
    palette_id: u8,
    /// 本显示集里 PDS 定义的 palette_id
    pds_ids: Vec<u8>,
    /// 本显示集里 PDS 的 palette_version（按段顺序）
    pds_vers: Vec<u8>,
    /// 本显示集里 WDS 定义的 window_id
    wds_ids: Vec<u8>,
    /// ODS 分片：(object_id, sequence_flag, 首片声明的 data_length, 本片数据字节数)
    ods_frags: Vec<(u16, u8, u32, usize)>,
    off: usize,
}

/// 读磁盘上的 `.sup` 并体检。读文件失败才返回 Err；流本身有问题一律写进报告。
pub fn verify_sup_file(path: &Path) -> Result<Value, String> {
    let data = fs::read(path).map_err(|e| format!("读 SUP 失败: {}", e))?;
    Ok(verify_sup_bytes(&data))
}

/// 只在转换收尾跑一次，整片读入换零 seek 复杂度；42 MB 的 SUP 也就一次峰值内存。
fn verify_sup_bytes(data: &[u8]) -> Value {
    let mut issues: Vec<Issue> = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();
    let mut garbage = 0usize;
    let mut i = 0usize;

    // ---------- 1. 拆段 + 段级结构 ----------
    while i + 13 <= data.len() {
        if &data[i..i + 2] != b"PG" {
            i += 1;
            garbage += 1;
            continue;
        }
        let rd32 = |o: usize| u32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        let pts = rd32(i + 2);
        let dts = rd32(i + 6);
        let typ = data[i + 10];
        let sz = ((data[i + 11] as usize) << 8) | data[i + 12] as usize;
        let end = i + 13 + sz;
        if end > data.len() {
            add(&mut issues, "error", "TRUNCATED", format!(
                "偏移 {} 的 {} 段声明 {} 字节负载，文件只剩 {} 字节",
                i, seg_name(typ), sz, data.len() - i - 13
            ));
            break;
        }
        let payload = &data[i + 13..end];
        match typ {
            PCS if payload.len() < 11 => add(&mut issues, "error", "SHORT_PCS",
                format!("偏移 {} 的 PCS 只有 {} 字节（至少 11）", i, payload.len())),
            WDS => {
                let need = 1 + 9 * payload.first().copied().unwrap_or(0) as usize;
                if payload.len() < need {
                    add(&mut issues, "error", "SHORT_WDS", format!(
                        "偏移 {} 的 WDS 声明 {} 个窗口却只有 {} 字节", i, payload.first().copied().unwrap_or(0), payload.len()
                    ));
                }
            }
            PDS if payload.len() < 2 => add(&mut issues, "error", "SHORT_PDS",
                format!("偏移 {} 的 PDS 只有 {} 字节", i, payload.len())),
            ODS if payload.len() < 7 => add(&mut issues, "error", "SHORT_ODS",
                format!("偏移 {} 的 ODS 只有 {} 字节", i, payload.len())),
            PCS | PDS | ODS | END => {}
            _ => add(&mut issues, "error", "UNKNOWN_SEG",
                format!("偏移 {} 出现未知段类型 0x{:02X}", i, typ)),
        }
        segs.push(Seg { typ, pts, dts, bytes: 13 + sz, payload, off: i });
        i = end;
    }

    let tail = data.len() - i;
    if tail >= 2 && data[i..].starts_with(b"PG") {
        add(&mut issues, "error", "SHORT_HEADER",
            format!("文件尾部 {} 字节不够一个 13 字节段头", tail));
    } else if tail > 0 {
        garbage += tail;
    }
    if garbage > 0 {
        add(&mut issues, "error", "NOT_ALIGNED",
            format!("{} 字节落在 'PG' 段边界之外（文件可能被拼接或截断过）", garbage));
    }
    if segs.is_empty() {
        add(&mut issues, "error", "EMPTY", "文件里没有任何 PGS 段");
    }

    // ---------- 2. 显示集 / epoch 划分 ----------
    let mut dss: Vec<Ds> = Vec::new();
    let mut cur: Option<Ds> = None;
    let mut orphan = 0usize;
    let mut no_end = 0usize;
    for s in &segs {
        match s.typ {
            PCS => {
                if let Some(d) = cur.take() {
                    if !d.has_end { no_end += 1; }
                    dss.push(d);
                }
                let state = s.payload.get(7).copied().unwrap_or(0xC0) & 0xC0;
                // PCS: w(2) h(2) rate(1) number(2) state(1) pal_upd(1) pal_id(1) nobj(1)
                // 之后每个对象 object_id(2) window_id(1) cropped(1) x(2) y(2)
                let p = s.payload;
                let palette_id = p.get(9).copied().unwrap_or(0);
                let pal_upd = p.get(8).copied().unwrap_or(0) & 0x80 != 0;
                let nref = p.get(10).copied().unwrap_or(0) as usize;
                let mut refs = Vec::new();
                let mut cobj_xy = Vec::new();
                for k in 0..nref.min(16) {
                    let o = 11 + k * 8;
                    if o + 3 > p.len() {
                        break;
                    }
                    let oid = u16::from_be_bytes([p[o], p[o + 1]]);
                    refs.push((oid, p[o + 2]));
                    // 布局：object_id(2) window_id(1) cropped(1) x(2) y(2)
                    if o + 8 <= p.len() {
                        let x = u16::from_be_bytes([p[o + 4], p[o + 5]]) as u32;
                        let y = u16::from_be_bytes([p[o + 6], p[o + 7]]) as u32;
                        cobj_xy.push((oid, x, y));
                    }
                }
                let cobjs = if nref > 0 {
                    let end = (11 + nref.min(16) * 8).min(p.len());
                    if end > 11 { p[11..end].to_vec() } else { Vec::new() }
                } else {
                    Vec::new()
                };
                cur = Some(Ds {
                    pts: s.pts, dts: s.dts, state, bytes: s.bytes as u64,
                    has_end: false, window_area: 0, objs: Vec::new(),
                    wds: Vec::new(), refs, pal_upd, cobjs, nref, palette_id,
                    cobj_xy,
                    pds_ids: Vec::new(), pds_vers: Vec::new(), wds_ids: Vec::new(),
                    ods_frags: Vec::new(), off: s.off,
                });
            }
            END => {
                if let Some(mut d) = cur.take() {
                    d.has_end = true;
                    d.bytes += s.bytes as u64;
                    dss.push(d);
                } else {
                    orphan += 1;
                }
            }
            _ => {
                if let Some(d) = cur.as_mut() {
                    d.bytes += s.bytes as u64;
                    match s.typ {
                        WDS => {
                            d.window_area = d.window_area.max(wds_area(s.payload));
                            if d.wds.is_empty() { d.wds = s.payload.to_vec(); }
                            let n = s.payload.first().copied().unwrap_or(0) as usize;
                            for k in 0..n.min(2) {
                                let o = 1 + k * 9;
                                if o < s.payload.len() {
                                    d.wds_ids.push(s.payload[o]);
                                }
                            }
                        }
                        PDS => {
                            if let Some(&id) = s.payload.first() {
                                d.pds_ids.push(id);
                            }
                            d.pds_vers.push(s.payload.get(1).copied().unwrap_or(0));
                        }
                        ODS => {
                            if let Some(pair) = ods_area(s.payload) { d.objs.push(pair); }
                            let q = s.payload;
                            if q.len() >= 4 {
                                let first = q[3] & 0x80 != 0;
                                let declared = if first && q.len() >= 7 {
                                    ((q[4] as u32) << 16) | ((q[5] as u32) << 8) | q[6] as u32
                                } else {
                                    0
                                };
                                // 首片多了 len(3)+w(2)+h(2)=7 字节头，续片只有 4 字节头
                                let data_len = q.len().saturating_sub(if first { 11 } else { 4 });
                                d.ods_frags.push((
                                    u16::from_be_bytes([q[0], q[1]]), q[3], declared, data_len,
                                ));
                            }
                        }
                        _ => {}
                    }
                } else {
                    orphan += 1;
                }
            }
        }
    }
    if let Some(d) = cur.take() {
        if !d.has_end { no_end += 1; }
        dss.push(d);
    }
    if orphan > 0 {
        add(&mut issues, "error", "ORPHAN_SEG",
            format!("{} 个段不属于任何显示集（出现在 PCS 之前或 END 之后）", orphan));
    }
    if no_end > 0 {
        add(&mut issues, "error", "DS_NO_END",
            format!("{} 组显示集缺少 END 段，解码器不会提交这次画面更新", no_end));
    }
    if let Some(d0) = dss.first() {
        if d0.state != 0x80 {
            add(&mut issues, "warn", "NO_EPOCH_START",
                "第一条显示集不是 Epoch Start(0x80)，解码器无法从流开头独立起播");
        }
    }
    let bad_state = dss.iter().filter(|d| d.state == 0xC0).count();
    if bad_state > 0 {
        add(&mut issues, "warn", "BAD_STATE",
            format!("{} 组显示集的 composition_state 是保留值 0xC0", bad_state));
    }

    // ---------- 3. 缓冲：RX 按 DTS 分组、object 按 epoch 分组 ----------
    let mut by_dts: BTreeMap<u32, u64> = BTreeMap::new();
    for d in &dss {
        *by_dts.entry(d.dts).or_insert(0) += d.bytes;
    }
    let rx_peak = by_dts.values().copied().max().unwrap_or(0);
    let rx_over = by_dts.values().filter(|&&v| v > RX_BUFFER).count();
    if rx_over > 0 {
        add(&mut issues, "error", "RX_OVERFLOW",
            format!("{} 个 DTS 上要取走的段超过 RX 缓冲 1 MiB（峰值 {}）", rx_over, kb(rx_peak)));
    }

    let mut epoch_map: HashMap<u16, u64> = HashMap::new();
    let mut obj_peak = 0u64;
    let mut obj_over = 0usize;
    let mut obj_cnt_over = 0usize;
    let mut max_obj_ids = 0usize;
    let mut epochs = 0usize;
    let mut wds_changed = 0usize;
    let mut ref_wds: Vec<u8> = Vec::new();
    let settle = |m: &HashMap<u16, u64>, peak: &mut u64, over: &mut usize, cnt: &mut usize, ids: &mut usize| {
        let sum: u64 = m.values().sum();
        *peak = (*peak).max(sum);
        *ids = (*ids).max(m.len());
        if sum > OBJ_BUFFER { *over += 1; }
        if m.len() > MAX_OBJECTS { *cnt += 1; }
    };
    for d in &dss {
        if d.state == 0x80 {
            settle(&epoch_map, &mut obj_peak, &mut obj_over, &mut obj_cnt_over, &mut max_obj_ids);
            epoch_map.clear();
            ref_wds.clear();
            epochs += 1;
        }
        for (id, a) in &d.objs {
            epoch_map.insert(*id, *a);
        }
        if !d.wds.is_empty() {
            if ref_wds.is_empty() {
                ref_wds = d.wds.clone();
            } else if ref_wds != d.wds {
                wds_changed += 1;
            }
        }
    }
    settle(&epoch_map, &mut obj_peak, &mut obj_over, &mut obj_cnt_over, &mut max_obj_ids);
    if obj_over > 0 {
        add(&mut issues, "error", "OBJ_OVERFLOW",
            format!("{} 个 epoch 的对象面积超过 object buffer 4 MiB（峰值 {}）", obj_over, kb(obj_peak)));
    }
    if obj_cnt_over > 0 {
        add(&mut issues, "error", "OBJ_COUNT",
            format!("{} 个 epoch 的唯一 object_id 超过 64（最多 {} 个）", obj_cnt_over, max_obj_ids));
    }
    if wds_changed > 0 {
        add(&mut issues, "warn", "WDS_CHANGED",
            format!("{} 处 WDS 在同一 epoch 内发生变化（规范要求 epoch 内窗口定义不变）", wds_changed));
    }

    // ---------- 3b. 显示集内部引用一致性 ----------
    // 纯字节级结构约束，与解码器时序模型无关，任何合规流都必须满足。
    // 瞬时码率只作统计展示，不判合规：拼段流的 DTS 是我们自己推导的，拿它算带宽会误伤。
    let mut peak_mbps = 0.0f64;
    let mut prev_dts: Option<u32> = None;
    for (&t, &b) in &by_dts {
        if let Some(pt) = prev_dts {
            let dt = t.saturating_sub(pt) as f64 / PGS_HZ;
            if dt > 0.0 {
                peak_mbps = peak_mbps.max(b as f64 * 8.0 / dt / 1e6);
            }
        }
        prev_dts = Some(t);
    }

    let mut bad_ods_len = 0usize;
    let mut bad_ods_seq = 0usize;
    for d in &dss {
        let mut by_id: BTreeMap<u16, Vec<(u8, u32, usize)>> = BTreeMap::new();
        for (id, flags, declared, data_len) in &d.ods_frags {
            by_id.entry(*id).or_default().push((*flags, *declared, *data_len));
        }
        for frags in by_id.values() {
            let first = frags[0];
            let last = *frags.last().unwrap();
            if frags.len() == 1 {
                // 单片必须同时带 first(0x80) 和 last(0x40)
                if first.0 & 0xC0 != 0xC0 { bad_ods_seq += 1; }
            } else if first.0 & 0x80 == 0 || last.0 & 0x40 == 0 {
                bad_ods_seq += 1;
            }
            let total: usize = frags.iter().map(|x| x.2).sum();
            // object_data_length 含首片的 width/height 这 4 字节
            if first.1 != (total + 4) as u32 { bad_ods_len += 1; }
        }
    }
    if bad_ods_len > 0 {
        add(&mut issues, "error", "ODS_LENGTH", format!(
            "{} 个对象的 ODS data_length 声明与实际分片数据长度不符", bad_ods_len));
    }
    if bad_ods_seq > 0 {
        add(&mut issues, "error", "ODS_SEQUENCE", format!(
            "{} 个对象的 ODS 分片序列不完整（缺首片或末片标志）", bad_ods_seq));
    }

    // epoch start 清空对象缓冲；normal case 允许复用 epoch 内早先定义的对象
    let mut defined: HashSet<u16> = HashSet::new();
    let mut bad_ref = 0usize;
    let mut bad_pal = 0usize;
    let mut bad_win = 0usize;
    for d in &dss {
        if d.state == 0x80 { defined.clear(); }
        for (id, _, _, _) in &d.ods_frags { defined.insert(*id); }
        for (oid, wid) in &d.refs {
            if !defined.contains(oid) { bad_ref += 1; }
            if !d.wds_ids.is_empty() && !d.wds_ids.contains(wid) { bad_win += 1; }
        }
        if !d.pds_ids.is_empty() && !d.pds_ids.contains(&d.palette_id) { bad_pal += 1; }
    }
    if bad_ref > 0 {
        add(&mut issues, "error", "OBJ_UNDEFINED", format!(
            "{} 处 PCS 引用了 epoch 内没有 ODS 定义的 object_id", bad_ref));
    }
    if bad_pal > 0 {
        add(&mut issues, "error", "PALETTE_MISMATCH", format!(
            "{} 组显示集的 PCS palette_id 与同组 PDS 不符", bad_pal));
    }
    if bad_win > 0 {
        add(&mut issues, "error", "WINDOW_MISMATCH", format!(
            "{} 处 PCS 引用的 window_id 不在同组 WDS 里", bad_win));
    }

    // ---------- 3c. palette update 显示集（只换调色板）----------
    // 依据 SUPer bytestream/verifier.py: test_diplayset / is_compliant：
    // PU 必须是 Normal Case、不带 WDS/ODS、必带 PDS、composition objects 非空且
    // 与上一个显示集逐字节相同。PU 是可选的合规优化，普通流里全部为 false。
    let mut pal_updates = 0usize;
    let mut pu_bad_state = 0usize;
    let mut pu_bad_wds = 0usize;
    let mut pu_bad_ods = 0usize;
    let mut pu_bad_pds = 0usize;
    let mut pu_no_cobj = 0usize;
    let mut pu_cobj_changed = 0usize;
    let mut pu_ver_bad = 0usize;
    let mut last_cobjs: Option<Vec<u8>> = None;
    let mut last_pal_ver: Option<u8> = None;
    for d in &dss {
        // Epoch Start 会清空解码器里的调色板与画面构成，跨 epoch 不能比较
        if d.state == 0x80 {
            last_cobjs = None;
            last_pal_ver = None;
        }
        if d.pal_upd {
            pal_updates += 1;
            if d.state != 0x00 { pu_bad_state += 1; }
            if !d.wds.is_empty() { pu_bad_wds += 1; }
            if !d.ods_frags.is_empty() { pu_bad_ods += 1; }
            if d.pds_ids.is_empty() || !d.pds_ids.contains(&d.palette_id) { pu_bad_pds += 1; }
            if d.nref == 0 { pu_no_cobj += 1; }
            match &last_cobjs {
                Some(prev) if *prev != d.cobjs => pu_cobj_changed += 1,
                None => pu_cobj_changed += 1,
                _ => {}
            }
        } else if d.nref > 0 {
            // 只有带 composition objects 的非 PU 显示集才更新「上一次画面构成」
            last_cobjs = Some(d.cobjs.clone());
        }
        // 同一 palette_id 的 PDS 版本需逐次 +1
        //（SUPer bytestream/verifier.py:is_compliant，否则解码器可能丢弃调色板）
        if let Some(&v) = d.pds_vers.first() {
            if d.pal_upd {
                if let Some(pv) = last_pal_ver {
                    if v != pv.wrapping_add(1) { pu_ver_bad += 1; }
                }
            }
            last_pal_ver = Some(v);
        }
    }
    if pal_updates > 0 {
        let mut chk = |n: usize, code: &'static str, msg: String| {
            if n > 0 { add(&mut issues, "error", code, msg); }
        };
        chk(pu_bad_state, "PU_BAD_STATE",
            format!("{} 组 palette update 显示集不是 Normal Case", pu_bad_state));
        chk(pu_bad_wds, "PU_WITH_WDS",
            format!("{} 组 palette update 显示集带了 WDS（同一显示集内不能既改窗口又换调色板）", pu_bad_wds));
        chk(pu_bad_ods, "PU_WITH_ODS",
            format!("{} 组 palette update 显示集带了 ODS（调色板动画不该重传位图）", pu_bad_ods));
        chk(pu_bad_pds, "PU_BAD_PDS",
            format!("{} 组 palette update 显示集缺 PDS 或 palette_id 不匹配", pu_bad_pds));
        chk(pu_no_cobj, "PU_NO_COBJ",
            format!("{} 组 palette update 显示集没有 composition objects", pu_no_cobj));
        if pu_cobj_changed > 0 {
            add(&mut issues, "warn", "PU_COBJ_CHANGED", format!(
                "{} 组 palette update 的 composition objects 与上一个画面构成不一致", pu_cobj_changed));
        }
        if pu_ver_bad > 0 {
            add(&mut issues, "warn", "PU_PDS_VERSION", format!(
                "{} 组 palette update 的 PDS 版本不是逐次 +1，解码器可能丢弃该调色板", pu_ver_bad));
        }
    }

    // ---------- 3d. 多对象显示集（帧间对象拆分）----------
    // PGS 一个 PCS 最多 2 个 composition object。拆分流会稳定出现 nref==2。
    // 只做统计与两条确定的结构约束，不臆造「对象必须落在窗口内」之类规则，
    // 避免像早期 PTS_EQ_DTS 那样误伤合规流。
    let mut multi_obj_ds = 0usize;
    let mut max_cobjs = 0usize;
    let mut dup_cobj = 0usize;
    let mut too_many_cobj = 0usize;
    for d in &dss {
        max_cobjs = max_cobjs.max(d.nref);
        if d.nref > 1 { multi_obj_ds += 1; }
        if d.nref > 2 { too_many_cobj += 1; }
        let mut local: HashSet<u16> = HashSet::new();
        for (oid, _x, _y) in &d.cobj_xy {
            if !local.insert(*oid) { dup_cobj += 1; }
        }
    }
    if too_many_cobj > 0 {
        add(&mut issues, "error", "COBJ_TOO_MANY", format!(
            "{} 组显示集的 composition objects 超过 2 个（PGS 单显示集上限）", too_many_cobj));
    }
    if dup_cobj > 0 {
        add(&mut issues, "error", "COBJ_DUP_ID", format!(
            "{} 处同一显示集内重复引用同一个 object_id", dup_cobj));
    }

    // ---------- 4. 时序 ----------
    // 现在的默认流与原生整流流都全段共用一个 DTS，不存在 dts=0 / 哨兵 PTS。
    // 下面仍跳过 dts=0，只为兼容历史产物与外部导入的旧 avs2bdnxml 流。
    let mut eq_pts_dts = 0usize;
    let mut dts_gt_pts = 0usize;
    let mut lead_over_1s = 0usize;
    let mut dts_back = 0usize;
    let mut dts_total = 0usize;
    let mut last_dts: Option<u32> = None;
    for s in &segs {
        if s.dts == 0 { continue; }
        dts_total += 1;
        if s.dts > s.pts { dts_gt_pts += 1; }
        if s.dts == s.pts { eq_pts_dts += 1; }
        if s.pts.saturating_sub(s.dts) > 90000 { lead_over_1s += 1; }
        if let Some(p) = last_dts {
            if s.dts < p { dts_back += 1; }
        }
        last_dts = Some(s.dts);
    }
    if dts_gt_pts > 0 {
        add(&mut issues, "error", "DTS_GT_PTS",
            format!("{} 个段的 DTS 晚于 PTS，解码器必然来不及", dts_gt_pts));
    }
    if lead_over_1s > 0 {
        add(&mut issues, "warn", "PTS_LEAD_1S",
            format!("{} 个段的 PTS 比 DTS 超前超过 1 秒（PTS−90000 < DTS）", lead_over_1s));
    }
    // PTS==DTS 不判不合规：H.222 的 PES 在两者相等时只写 PTS；SUPer 的
    // check_pts_dts_sanity 更明确要求 PDS 段 pts==dts、只要求 ODS/WDS 的 pts!=dts。
    // 早期版本把它列成警告，会误伤 SUPer 自己判定 compliant 的流，现只作统计。
    if dts_back > 0 {
        add(&mut issues, "warn", "DTS_NOT_MONO",
            format!("{} 处 DTS 小于前一段（PGS 要求 DTS 严格单调；ODS 提前解码时常见）", dts_back));
    }

    let mut pts_back: Vec<(usize, u32, u32)> = Vec::new();
    let mut pts_dup = 0usize;
    for w in dss.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if b.pts < a.pts {
            pts_back.push((b.off, a.pts, b.pts));
        } else if b.pts == a.pts {
            pts_dup += 1;
        }
    }
    for (off, a, b) in pts_back.iter().take(MAX_LINE_ISSUES) {
        add(&mut issues, "error", "PTS_BACK", format!(
            "偏移 {} 的显示集 PTS 从 {} 倒退到 {}（差 {} tick）", off, clock(*a), clock(*b), *b as i64 - *a as i64
        ));
    }
    if pts_back.len() > MAX_LINE_ISSUES {
        add(&mut issues, "error", "PTS_BACK",
            format!("另有 {} 处显示集 PTS 倒退", pts_back.len() - MAX_LINE_ISSUES));
    }
    if pts_dup > 0 {
        add(&mut issues, "warn", "PTS_DUP",
            format!("{} 组相邻显示集共用同一个 PTS，解码器需要在同一帧处理两次更新", pts_dup));
    }

    // ---------- 5. 解码余量（Rd / Rc 的保守估算） ----------
    let first_pcs = segs.iter().find(|s| s.typ == PCS && s.payload.len() >= 5);
    let (vw, vh) = first_pcs
        .map(|s| (u16::from_be_bytes([s.payload[0], s.payload[1]]), u16::from_be_bytes([s.payload[2], s.payload[3]])))
        .unwrap_or((0, 0));
    let plane_area = vw as u64 * vh as u64;
    let clear_ticks = (PGS_HZ * plane_area as f64 / RC).ceil() as u64; // W_d，仅 epoch start 需要
    let mut tight = 0usize;
    let mut worst_need = 0u64;
    for d in &dss {
        let decode: u64 = d.objs.iter()
            .map(|(_, a)| (PGS_HZ * *a as f64 / RD).ceil() as u64)
            .sum();
        let compose = (PGS_HZ * d.window_area as f64 / RC).ceil() as u64;
        let clear = if d.state == 0x80 { clear_ticks } else { 0 };
        let need = clear.max(decode) + compose;
        let lead = d.pts.saturating_sub(d.dts) as u64;
        if lead < need {
            tight += 1;
            worst_need = worst_need.max(need);
        }
    }
    // 这里只作粗略估算、不判不合规：SUPer 靠 display set buffering 提前解码，
    // 且它的 check_pts_dts_sanity 只对 epoch start 要求 PTS−DTS > 窗口擦除时间，
    // 并非每个显示集都要满足 need。早期版本按本估算报 TIGHT_DECODE，会误伤合规流。

    // ---------- 6. 汇总 ----------
    let counts = |t: u8| segs.iter().filter(|s| s.typ == t).count();
    let first_pts = dss.first().map(|d| d.pts).unwrap_or(0);
    let last_pts = dss.last().map(|d| d.pts).unwrap_or(0);
    let dur_ticks = last_pts.saturating_sub(first_pts);
    let dur_s = dur_ticks as f64 / PGS_HZ;
    let mbps = if dur_s > 0.0 { data.len() as f64 * 8.0 / dur_s / 1e6 } else { 0.0 };

    let errs: Vec<&Issue> = issues.iter().filter(|x| x.level == "error").collect();
    let warns: Vec<&Issue> = issues.iter().filter(|x| x.level == "warn").collect();
    let ok = errs.is_empty();
    let compliant = issues.is_empty();

    let stats = json!({
        "bytes": data.len(),
        "segments": segs.len(),
        "display_sets": dss.len(),
        "epochs": epochs,
        "pcs": counts(PCS), "wds": counts(WDS), "pds": counts(PDS),
        "ods": counts(ODS), "end": counts(END),
        "video": format!("{}x{}", vw, vh),
        "duration_s": (dur_s * 1000.0).round() / 1000.0,
        "avg_mbps": (mbps * 100.0).round() / 100.0,
        "peak_mbps": (peak_mbps * 100.0).round() / 100.0,
        "ods_len_bad": bad_ods_len,
        "ods_seq_bad": bad_ods_seq,
        "obj_undefined": bad_ref,
        "palette_bad": bad_pal,
        "window_bad": bad_win,
        "rx_peak": rx_peak,
        "rx_limit": RX_BUFFER,
        "obj_peak": obj_peak,
        "obj_limit": OBJ_BUFFER,
        "max_obj_ids": max_obj_ids,
        "pts_back": pts_back.len(),
        "pts_eq_dts": eq_pts_dts,
        "dts_valid": dts_total,
        "dts_back": dts_back,
        "tight_ds": tight,
        "decode_need_max_ticks": worst_need,
        "palette_updates": pal_updates,
        "pu_cobj_changed": pu_cobj_changed,
        "pu_pds_version_bad": pu_ver_bad,
        "multi_obj_ds": multi_obj_ds,
        "max_cobjs": max_cobjs,
        "cobj_dup": dup_cobj,
        "cobj_too_many": too_many_cobj,
    });

    let mut lines: Vec<Value> = Vec::new();
    let put = |v: &mut Vec<Value>, kind: &str, line: String| {
        v.push(json!({ "kind": kind, "line": line }));
    };
    put(&mut lines, "cmd", format!(
        "SUP 合规自检 · {} 段 / {} 组显示集 / {} 个 epoch / {} · {}",
        segs.len(), dss.len(), epochs, clock(dur_ticks), kb(data.len() as u64)
    ));
    put(&mut lines, "dim", format!(
        "结构  PCS {} · WDS {} · PDS {} · ODS {} · END {}   |   画面 {}×{}   |   平均 {:.2} Mbps · 瞬时峰值 {:.2} Mbps",
        counts(PCS), counts(WDS), counts(PDS), counts(ODS), counts(END), vw, vh, mbps, peak_mbps
    ));
    put(&mut lines, "dim", format!(
        "引用  ODS 长度 {} 异常 · 分片序列 {} 异常 · 未定义对象 {} · 调色板 {} · 窗口 {}",
        bad_ods_len, bad_ods_seq, bad_ref, bad_pal, bad_win
    ));
    put(&mut lines, "dim", format!(
        "缓冲  RX 峰值 {} / {} · object 峰值 {} / {} · 单 epoch 最多 {} 个对象",
        kb(rx_peak), kb(RX_BUFFER), kb(obj_peak), kb(OBJ_BUFFER), max_obj_ids
    ));
    put(&mut lines, "dim", format!(
        "时序  PCS PTS {} · DTS {} · PTS==DTS {} 段（合法，PDS 规范如此）· 提前量估算偏紧 {} 组（仅供参考）",
        if pts_back.is_empty() { "递增 OK" } else { "有倒退" },
        if dts_back == 0 { "非递减 OK" } else { "有回退" },
        eq_pts_dts, tight
    ));
    if pal_updates > 0 {
        put(&mut lines, "dim", format!(
            "调色板动画  palette update {} 组 · 画面构成不一致 {} · PDS 版本跳变 {}（均应为 0）",
            pal_updates, pu_cobj_changed, pu_ver_bad
        ));
    }
    for x in errs.iter().take(MAX_LINE_ISSUES) {
        put(&mut lines, "err", format!("✗ [{}] {}", x.code, x.msg));
    }
    if errs.len() > MAX_LINE_ISSUES {
        put(&mut lines, "err", format!("… 另有 {} 处错误", errs.len() - MAX_LINE_ISSUES));
    }
    for x in warns.iter().take(MAX_LINE_ISSUES) {
        put(&mut lines, "warn", format!("! [{}] {}", x.code, x.msg));
    }
    if warns.len() > MAX_LINE_ISSUES {
        put(&mut lines, "warn", format!("… 另有 {} 项警告", warns.len() - MAX_LINE_ISSUES));
    }
    if ok {
        put(&mut lines, if warns.is_empty() { "done" } else { "warn" }, format!(
            "✓ 自检通过 · 无错误 · {} 项警告{}",
            warns.len(),
            if compliant { " · 严格合规" } else { " · 严格合规：否" }
        ));
    } else {
        put(&mut lines, "err", format!(
            "✗ 自检未通过 · {} 处错误 / {} 项警告（产物已生成，建议修复后再 mux）",
            errs.len(), warns.len()
        ));
    }

    json!({
        "ok": ok,
        "compliant": compliant,
        "errors": errs.iter().map(|x| json!({"code": x.code, "msg": x.msg})).collect::<Vec<_>>(),
        "warnings": warns.iter().map(|x| json!({"code": x.code, "msg": x.msg})).collect::<Vec<_>>(),
        "stats": stats,
        "lines": lines,
    })
}
