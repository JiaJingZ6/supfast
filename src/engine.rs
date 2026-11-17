//! supfast —— avs2bdnxml 加速前端（tanshao 模式的干净复刻）
//!
//! # 原理
//!
//! 原版 `avs2bdnxml` 每帧都要向 AviSynth 要一张 1920x1080 RGBA 图
//! （`MaskSubMod` 渲染 + `LanczosResize` 全帧重采样），约 3~4 ms/帧，
//! 与画面有没有变化无关。
//!
//! 本工具先自己解析 ASS 时间轴，把整条时间轴切成「画面恒定段」和「画面变化段」：
//!
//! * 恒定段（只有静态事件在显示，无 `\move` `\t()` `\fad` `\k` 等动态标签）
//!   → 只渲染该段首帧 1 张图，整段共用
//! * 变化段（含动态标签）
//!   → 逐帧渲染，与原版一致
//!
//! 随后逐段独立调用 `avs2bdnxml`，最后按 ASS 的真实时间码重写 BDN XML，
//! 并把 PGS 的 PTS 重映射回真实时间。
//!
//! # 三个必须知道的坑
//!
//! 1. `avs2bdnxml` 会把每一帧和**前一帧**比较去重。若把多个静态段拼成一条时间轴，
//!    第 2 段开始会被判成「和前面一样」而**整段丢失**。因此每段必须独立调用。
//! 2. `Trim(k,k)` 只取 1 帧时 `avs2bdnxml` 会输出空事件。
//!    静态段必须至少取 2 帧并 `.Loop()` 补齐，取第 1 张 PNG。
//! 3. `avs2bdnxml` 把 PNG 写到**当前工作目录**而不是 XML 旁边。
//!    并行时必须给每段独立 cwd，否则互相覆盖。
//!
//! 另外：AviSynth 不缓存帧，`Loop` / `FreezeFrame` / `++` 都无法实现时间轴级去重，
//! 所以只能落到「分段独立进程」这一条路。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 静态段最少渲染帧数（1 帧会输出空事件）
const MIN_STATIC_FRAMES: u32 = 2;
/// 切段上限：逐段调用的固定开销会反噬
const MAX_SEGMENTS: usize = 20000; // 段数≈字幕条数，正常影片几百到几千
/// PGS 时间基
const PGS_HZ: f64 = 90000.0;

/// 会让「显示期间画面逐帧变化」的 ASS 覆写标签
const DYNAMIC_TAGS: &[&str] = &[
    "\\move", "\\t(", "\\fad(", "\\fade(", "\\org(", "\\frx(", "\\fry(", "\\frz(",
    "\\fr(", "\\be(", "\\blur(", "\\jitter(", "\\shake(",
];

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Event {
    start: u32,
    end: u32,
    #[allow(dead_code)]
    layer: i32,
    #[allow(dead_code)]
    style: String,
    #[allow(dead_code)]
    text: String,
    dynamic: bool,
}

impl Event {
    fn new(start: u32, end: u32, layer: i32, style: String, text: String) -> Self {
        let dynamic = is_dynamic(&text);
        Event { start, end, layer, style, text, dynamic }
    }
}

/// 一段连续时间区间
#[derive(Debug, Clone, Copy)]
struct Segment {
    start: u32,
    end: u32,
    dynamic: bool,
}

// ---------------------------------------------------------------------------
// 动态标签检测
// ---------------------------------------------------------------------------

fn is_dynamic(text: &str) -> bool {
    // 只在 `{...}` 覆盖块内判断，普通文本里的反斜杠不误判
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            let block = &text[i..j.min(text.len())];
            for t in DYNAMIC_TAGS {
                if block.contains(t) {
                    return true;
                }
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    // 卡拉OK的 \k 需要额外判断（避免 \kf 被 \k 前缀吞掉的情况已由列表覆盖）
    let mut k = 0usize;
    while let Some(p) = text[k..].find("\\k") {
        let idx = k + p + 2;
        let rest = &text[idx..];
        // \k / \kf / \ko 后面跟数字才算
        let rest = rest.strip_prefix('f').or_else(|| rest.strip_prefix('o')).unwrap_or(rest);
        let rest = rest.trim_start();
        if rest.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            return true;
        }
        k = idx;
        if k >= text.len() {
            break;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 时间码
// ---------------------------------------------------------------------------

/// `h:mm:ss.cc` → 帧号（**向上取整**，与 tanshao.exe 实际行为一致）
///
/// 实测对齐：10.00s@23.976 → 240（raw 239.76）
///           150.00s        → 3597（raw 3596.40）
///           290.00s        → 6954（raw 6953.04）
fn parse_timecode(tc: &str, fps: f64) -> Option<u32> {
    let tc = tc.trim();
    let mut parts = tc.split(|c| c == ':' || c == '.');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    let frac = parts.next();
    let sub = match frac {
        Some(f) => {
            let f = f.trim();
            if f.is_empty() { 0.0 } else { format!("0.{}", f).parse::<f64>().ok()? }
        }
        None => 0.0,
    };
    let sec = h * 3600.0 + m * 60.0 + s + sub;
    Some((sec * fps).ceil() as u32)
}

/// 帧号 → `HH:MM:SS:FF`
fn frames_to_tc(fr: u32, fps: f64) -> String {
    let fpsi = fps.round().max(1.0) as u32;
    let sec = fr / fpsi;
    let rem = fr % fpsi;
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    let s = sec % 60;
    format!("{:02}:{:02}:{:02}:{:02}", h, m, s, rem)
}

/// `HH:MM:SS:FF` → 帧号（用于解析 avs2bdnxml 自己产出的 XML）
fn tc_to_frames(tc: &str, fps: f64) -> u32 {
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

// ---------------------------------------------------------------------------
// 文本读取（带编码探测）
// ---------------------------------------------------------------------------

fn read_text(path: &Path) -> std::io::Result<String> {
    let raw = fs::read(path)?;
    // UTF-8 BOM
    if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        if let Ok(s) = std::str::from_utf8(&raw[3..]) {
            return Ok(s.to_string());
        }
    }
    if let Ok(s) = std::str::from_utf8(&raw) {
        return Ok(s.to_string());
    }
    // UTF-16 LE BOM
    if raw.starts_with(&[0xFF, 0xFE]) {
        let u16s: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Ok(String::from_utf16_lossy(&u16s));
    }
    // 系统 ANSI 代码页兜底（中文 Windows 即 GBK）。
    // 必须真解码：之前把非 ASCII 一律换成 U+FFFD，不同台词解码后全都一样，
    // 切段签名因此碰撞，时间相接的两句会被并成一段、只渲第一句（整串漏字幕）。
    #[cfg(windows)]
    if let Some(s) = decode_ansi(&raw) {
        return Ok(s);
    }
    Ok(String::from_utf8_lossy(&raw).to_string())
}

/// 用系统 API 按 ANSI 代码页解码，省掉内嵌两万字符的 GBK 表。
#[cfg(windows)]
fn decode_ansi(raw: &[u8]) -> Option<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte_str: *const u8,
            cb_multi_byte: i32,
            wide_char_str: *mut u16,
            cch_wide_char: i32,
        ) -> i32;
    }
    const CP_ACP: u32 = 0;
    if raw.is_empty() || raw.len() > i32::MAX as usize {
        return None;
    }
    unsafe {
        let n = MultiByteToWideChar(CP_ACP, 0, raw.as_ptr(), raw.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return None;
        }
        let mut buf = vec![0u16; n as usize];
        let m = MultiByteToWideChar(CP_ACP, 0, raw.as_ptr(), raw.len() as i32, buf.as_mut_ptr(), n);
        if m <= 0 {
            return None;
        }
        buf.truncate(m as usize);
        Some(String::from_utf16_lossy(&buf))
    }
}

// ---------------------------------------------------------------------------
// ASS 解析
// ---------------------------------------------------------------------------

struct AssFile {
    events: Vec<Event>,
    playres_x: u32,
    playres_y: u32,
    timer_fps: f64,
}

fn parse_ass(path: &Path, fps: f64) -> std::io::Result<AssFile> {
    let text = read_text(path)?;
    let mut playres_x = 0u32;
    let mut playres_y = 0u32;
    let mut timer_fps = 0.0f64;
    let mut events = Vec::new();
    let mut section = String::new();
    let mut fmt: Vec<String> = Vec::new();

    for line in text.lines() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        if s.starts_with('[') && s.ends_with(']') {
            section = s[1..s.len() - 1].to_lowercase();
            continue;
        }

        if section == "script info" {
            let low = s.to_lowercase();
            if low.starts_with("playresx:") {
                playres_x = s[9..].trim().parse().unwrap_or(0);
            } else if low.starts_with("playresy:") {
                playres_y = s[9..].trim().parse().unwrap_or(0);
            } else if low.starts_with("timer:") {
                timer_fps = s[6..].trim().parse().unwrap_or(0.0);
            }
            continue;
        }

        if section.starts_with("v4") || section == "events" {
            let low = s.to_lowercase();
            if low.starts_with("format:") {
                fmt = s[7..].split(',').map(|x| x.trim().to_lowercase()).collect();
                continue;
            }
            if !low.starts_with("dialogue:") {
                continue;
            }
            let body = &s[9..];
            let ncomma = if fmt.is_empty() { 9 } else { fmt.len().saturating_sub(1) };
            let parts = split_n(body, ',', ncomma);
            if parts.len() < ncomma + 1 {
                continue;
            }
            let idx = |name: &str, dflt: usize| -> usize {
                fmt.iter().position(|f| f == name).unwrap_or(dflt)
            };
            let get = |i: usize| -> &str { parts.get(i).map(|s| s.as_str()).unwrap_or("") };

            let layer = get(idx("layer", 0)).trim().parse().unwrap_or(0);
            let st = match parse_timecode(get(idx("start", 1)), fps) {
                Some(v) => v,
                None => continue,
            };
            let en = match parse_timecode(get(idx("end", 2)), fps) {
                Some(v) => v,
                None => continue,
            };
            let style = get(idx("style", 3)).trim().to_string();
            let tpos = idx("text", parts.len() - 1);
            let txt = get(tpos).trim().to_string();

            if en <= st {
                continue;
            }
            events.push(Event::new(st, en, layer, style, txt));
        }
    }

    Ok(AssFile { events, playres_x, playres_y, timer_fps })
}

/// 按分隔符切 n 次，多余部分全给最后一段（ASS 的 Text 字段可能含逗号）
fn split_n(s: &str, sep: char, n: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(n + 1);
    let mut cur = String::new();
    let mut cnt = 0usize;
    for ch in s.chars() {
        if ch == sep && cnt < n {
            out.push(cur.clone());
            cur.clear();
            cnt += 1;
        } else {
            cur.push(ch);
        }
    }
    out.push(cur);
    out
}

// ---------------------------------------------------------------------------
// 切段
// ---------------------------------------------------------------------------

fn build_segments(events: &[Event]) -> Vec<Segment> {
    if events.is_empty() {
        return Vec::new();
    }
    // 所有边界点
    let mut bounds: Vec<u32> = Vec::with_capacity(events.len() * 2);
    for e in events {
        bounds.push(e.start);
        bounds.push(e.end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    // 扫描线：事件按起点排序，边界点从左往右走时增量维护 active 集合。
    // 早先的写法对每个边界窗口重扫一遍全部事件，是 O(n²)：1831 条事件 670 万次
    // 比较还能忍，2 万条（MAX_SEGMENTS 的上限）会到 8 亿次，analyze 直接把界面卡死。
    let mut order: Vec<usize> = (0..events.len()).collect();
    order.sort_unstable_by_key(|&i| events[i].start);
    let mut next = 0usize;
    let mut active: Vec<usize> = Vec::new();

    let mut segs: Vec<Segment> = Vec::new();
    // 只有「时间相接 + 可见事件集合完全相同」才合并。字幕常常上一句结束就是
    // 下一句开始，若只看 dynamic 标记就会把整串台词并成一段，只渲第一句，
    // 后面的字幕全部丢失。
    let mut last_sig: Option<String> = None;
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b <= a {
            continue;
        }
        while next < order.len() && events[order[next]].start <= a {
            active.push(order[next]);
            next += 1;
        }
        active.retain(|&i| events[i].end > a);

        if active.is_empty() {
            last_sig = None;
            continue; // 空档：整段跳过
        }
        let dyn_ = active.iter().any(|&i| events[i].dynamic);
        let mut sig: Vec<(i32, &str, &str)> = active
            .iter()
            .map(|&i| (events[i].layer, events[i].style.as_str(), events[i].text.as_str()))
            .collect();
        sig.sort_unstable();
        let mut key = String::new();
        key.push(if dyn_ { 'd' } else { 's' });
        for (l, st, tx) in &sig {
            key.push('\u{1}');
            key.push_str(&l.to_string());
            key.push('\u{2}');
            key.push_str(st);
            key.push('\u{2}');
            key.push_str(tx);
        }
        if let Some(last) = segs.last_mut() {
            if last.end == a && last_sig.as_deref() == Some(key.as_str()) {
                last.end = b;
                last_sig = Some(key);
                continue;
            }
        }
        segs.push(Segment { start: a, end: b, dynamic: dyn_ });
        last_sig = Some(key);
    }
    segs
}

// ---------------------------------------------------------------------------
// AVS 生成
// ---------------------------------------------------------------------------

fn avs_escape(p: &str) -> String {
    p.replace('\\', "\\\\")
}

fn avs_for_segment(
    plugin: &str,
    ass: &str,
    w: u32,
    h: u32,
    fps: f64,
    total_frames: u32,
    seg: &Segment,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("LoadPlugin(\"{}\")\n", avs_escape(plugin)));
    out.push_str(&format!(
        "__src = MaskSubMod(\"{}\", {}, {}, {}, {})\n",
        avs_escape(ass),
        w,
        h,
        fps_str(fps),
        total_frames
    ));
    out.push_str(&format!("__src = LanczosResize(__src, {}, {})\n", w, h));

    if seg.dynamic {
        out.push_str(&format!("__src.Trim({}, {})\n", seg.start, seg.end - 1));
    } else {
        // 静态段：取 2 帧再 Loop 补齐，avs2bdnxml 才会输出 1 个非空事件
        let f0 = seg.start;
        let f1 = (seg.start + 1).min(seg.end.saturating_sub(1).max(seg.start));
        out.push_str(&format!(
            "__src.Trim({}, {}).Loop({})\n",
            f0, f1, MIN_STATIC_FRAMES
        ));
    }
    out
}

fn fps_str(fps: f64) -> String {
    // 23.976 不要输出成 23.975999...
    format!("{}", fps)
}

// ---------------------------------------------------------------------------
// 结果 XML 解析
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// PGS (SUP) 时间戳重映射
// ---------------------------------------------------------------------------
//
// 段头固定 13 字节：'PG'(2) + PTS(4) + DTS(4) + type(1) + size(2)
//   PCS(0x16) 显示/清屏指令 → 主时间戳
//   WDS(0x17) 窗口定义     → 跟 PCS
//   ODS(0x15) 对象位图     → 比 PCS 早一帧
//   PDS(0x14) palette      → 时间戳无意义
//   END(0x80) epoch 结束   → 跟 PCS

fn sup_retime(
    data: &[u8],
    fps: f64,
    real_start: u32,
    comp_frames: u32,
    real_frames: u32,
    drop_tail: bool,
) -> Result<Vec<u8>, String> {
    let frame_ticks = PGS_HZ / fps;
    let statics = comp_frames != real_frames;

    // 拆段：只记区间，不复制字节（整片 42 MB 的 SUP 少走两趟内存）
    struct Seg {
        typ: u8,
        s: usize,
        e: usize,
    }
    let mut segs: Vec<Seg> = Vec::new();
    let mut i = 0usize;
    while i + 13 <= data.len() {
        if &data[i..i + 2] != b"PG" {
            i += 1;
            continue;
        }
        let typ = data[i + 10];
        let size = ((data[i + 11] as usize) << 8) | (data[i + 12] as usize);
        let end = i + 13 + size;
        if end > data.len() {
            // 以前这里静默 break，剩下的字节直接丢掉，产出的 .sup 尾巴缺失，
            // 播放器上表现为最后一段字幕不显示。宁可报错让调用方指出是哪一段。
            return Err(format!(
                "SUP 被截断：段头声明 {} 字节负载，实际只剩 {}",
                size,
                data.len().saturating_sub(i + 13)
            ));
        }
        segs.push(Seg { typ, s: i, e: end });
        i = end;
    }
    if i < data.len() && data[i..].starts_with(b"PG") {
        return Err("SUP 尾部不完整（段头不足 13 字节）".into());
    }
    if segs.is_empty() {
        return Ok(data.to_vec());
    }

    // 静态段渲的是 2 帧，尾部自带一个「清屏 epoch」。若下一段正好从这段结尾开始，
    // 这个清屏和下一段的显示是同一时间戳 —— 原版不会有它（直接换 PCS 就行）。
    // 丢掉它省一条 41 字节的冗余 epoch，也让 PCS 数和原版一致。
    if drop_tail && statics {
        let ps: Vec<usize> = segs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.typ == 0x16)
            .map(|(k, _)| k)
            .collect();
        if ps.len() == 2 {
            segs.truncate(ps[1]);
        }
    }

    let pcs_idx: Vec<usize> = segs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.typ == 0x16)
        .map(|(k, _)| k)
        .collect();

    // 每个 PCS 应携带的真实帧（浮点，便于算 PTS）
    let mut pcs_time: Vec<f64> = Vec::with_capacity(pcs_idx.len());
    for (n, _) in pcs_idx.iter().enumerate() {
        if statics {
            pcs_time.push(if n == 0 {
                real_start as f64
            } else {
                (real_start + real_frames) as f64
            });
        } else {
            pcs_time.push(real_start as f64 + n as f64);
        }
    }

    let ts = |frame: f64| -> u32 { (frame.max(0.0) * frame_ticks).round() as u32 };

    let mut cur = 0.0f64;
    let mut pi = 0usize;
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    for s in &segs {
        let base = out.len();
        out.extend_from_slice(&data[s.s..s.e]);
        match s.typ {
            0x16 => {
                if pi < pcs_time.len() {
                    cur = pcs_time[pi];
                }
                let t = ts(cur).to_be_bytes();
                out[base + 2..base + 6].copy_from_slice(&t);
                out[base + 6..base + 10].copy_from_slice(&t);
                pi += 1;
            }
            0x17 | 0x80 => {
                let t = ts(cur).to_be_bytes();
                out[base + 2..base + 6].copy_from_slice(&t);
                out[base + 6..base + 10].copy_from_slice(&t);
            }
            0x15 => {
                let t = ts((cur - 1.0).max(0.0)).to_be_bytes();
                out[base + 2..base + 6].copy_from_slice(&t);
                out[base + 6..base + 10].copy_from_slice(&t);
            }
            _ => {} // PDS 原样
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 路径处理
// ---------------------------------------------------------------------------

/// 剥掉 Windows 扩展长度路径前缀 `\\?\` 和 `\\?\UNC\`
///
/// `std::fs::canonicalize` 在 Windows 上一律返回 `\\?\C:\...` 形式的路径，
/// 而 AviSynth 的 `LoadPlugin` / `MaskSubMod` 以及 VSFilterMod 都不认这个前缀，
/// 直接把带前缀的路径写进 AVS 脚本会导致静默失败（无事件输出）。
fn strip_verbatim(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{}", rest);
    }
    if let Some(rest) = p.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    p.to_string()
}

/// 解析产物目录。一律是 `<父目录>\<字幕名>\`：
/// 留空时父目录取字幕所在目录，填了（或浏览选了）就用那个目录当父目录。
/// 所以「留空」和「选字幕所在目录」结果完全一致，不会因为选没选而产生两套布局。
/// 万一选中的目录本身已经叫字幕名（重复点进去过），就不再套一层。
fn resolve_outdir(ass_path: &Path, outdir: Option<&Path>, base: &str) -> PathBuf {
    let parent = match outdir {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => match ass_path.parent() {
            // 相对路径 "test.ass" 的 parent 是空串，落回当前目录
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        },
    };
    let already = parent
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(base));
    if already {
        parent
    } else {
        parent.join(base)
    }
}

// ---------------------------------------------------------------------------
// 工具定位
// ---------------------------------------------------------------------------

fn find_exe(root: Option<&str>) -> Option<(PathBuf, String)> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(r) = root {
        cands.push(PathBuf::from(r));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.to_path_buf());
            cands.push(dir.join("mod"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.clone());
        cands.push(cwd.join("mod"));
    }
    cands.push(PathBuf::from(r"H:\DIY\mod"));
    cands.push(PathBuf::from(r"H:\DIY"));

    for c in cands {
        let exe = c.join("avs2bdnxml.exe");
        if exe.is_file() {
            let dll = c.join("csri").join("VSFilterMod.dll");
            let dll_s = if dll.is_file() {
                dll.to_string_lossy().to_string()
            } else {
                String::new()
            };
            return Some((exe, dll_s));
        }
    }
    None
}


// ---------------------------------------------------------------------------
// 下面是从 JS 版 engine/supfast.js 移植的调度层：
//   analyze()  给界面出切段计划（含全片 bands）
//   convert()  N 路并行逐段渲染 → 拷贝 PNG → 重写 XML → 拼接 SUP
// ---------------------------------------------------------------------------

use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
/// CREATE_NO_WINDOW：GUI 进程里 spawn 控制台程序时不分配 conhost，
/// 否则每段都要新建一个控制台（既闪窗，单段开销也翻倍）
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 单段超时：12 路并发下 avs2bdnxml 偶发卡死（不退出、不报错、不产文件）
const SEG_TIMEOUT_SECS: u64 = 120;
/// 单段最多尝试次数
const SEG_ATTEMPTS: usize = 3;

pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;

// ---------------------------------------------------------------------------
// 从 serde_json::Value 里宽松取值（界面那边 fps 是字符串，别的地方可能是数字）
// ---------------------------------------------------------------------------

pub fn pick_f64(v: &Value, k: &str) -> f64 {
    match v.get(k) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

pub fn pick_u32(v: &Value, k: &str) -> u32 {
    let f = pick_f64(v, k);
    if f.is_finite() && f > 0.0 { f.round() as u32 } else { 0 }
}

fn pick_flag(v: &Value, k: &str, def: bool) -> bool {
    match v.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        Some(Value::Null) | None => def,
        _ => def,
    }
}

fn pick_str(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 零依赖伪随机：RandomState 每次构造都带随机种子，再混入 pid 与纳秒时间。
/// 只用于临时目录名，不需要密码学强度，但足以让名字猜不到。
fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(std::process::id() as u64);
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

/// 已经装好的运行参数
struct RunOpts {
    ass: PathBuf,
    fps: f64,
    jobs: usize,
    width: u32,
    height: u32,
    vfmt: String,
    lang: String,
    title: String,
    outdir: Option<PathBuf>,
    mod_root: Option<String>,
    palette: u32,
    autocrop: u32,
    bufsplit: u32,
    stricter: u32,
    even_y: u32,
    no_sup: bool,
    dry_run: bool,
}

fn read_opts(v: &Value) -> Result<RunOpts, String> {
    let ass_raw = pick_str(v, "ass");
    if ass_raw.is_empty() {
        return Err("请先选择 .ass 字幕".into());
    }
    let ass = fs::canonicalize(&ass_raw).map_err(|_| format!("找不到文件: {}", ass_raw))?;
    let fps = {
        let f = pick_f64(v, "fps");
        // 上限 240：再高 PGS 的 90 kHz 时间基就撑不住了（90000/1e6=0.09 tick/帧），
        // 时间戳会全部取整成同一值，产出的 SUP 每个 PCS 都是 0。
        if f >= 1.0 && f <= 240.0 { f } else { 23.976 }
    };
    let jobs = {
        let j = pick_u32(v, "jobs");
        // 每段一个子进程，每个进程几十 MB；64 路已经能吃掉几 GB 内存
        (if j == 0 { 4 } else { j.min(64) }) as usize
    };
    let out = pick_str(v, "outdir");
    let mod_root = pick_str(v, "mod_root");
    // 分辨率：0 表示沿用 ASS 的 PlayRes。上限 8192×8192，
    // 否则 LanczosResize 会直接把内存吃爆（4K 的 4 倍已经远超实用范围）。
    let cap = |n: u32| -> u32 { if n > 0 { n.min(8192) } else { 0 } };
    Ok(RunOpts {
        ass,
        fps,
        jobs,
        width: cap(pick_u32(v, "width")),
        height: cap(pick_u32(v, "height")),
        vfmt: {
            let s = pick_str(v, "vfmt");
            if s.is_empty() { "1080p".into() } else { s }
        },
        lang: {
            let s = pick_str(v, "lang");
            if s.is_empty() { "und".into() } else { s }
        },
        title: {
            let s = pick_str(v, "title");
            if s.is_empty() { "Undefined".into() } else { s }
        },
        outdir: if out.is_empty() { None } else { Some(PathBuf::from(out)) },
        mod_root: if mod_root.is_empty() { None } else { Some(mod_root) },
        palette: if v.get("palette").is_some() { pick_u32(v, "palette") } else { 1 },
        autocrop: if v.get("autocrop").is_some() { pick_u32(v, "autocrop") } else { 1 },
        bufsplit: if v.get("bufsplit").is_some() { pick_u32(v, "bufsplit") } else { 1 },
        stricter: pick_u32(v, "stricter"),
        even_y: pick_u32(v, "even_y"),
        no_sup: pick_flag(v, "no_sup", false),
        dry_run: pick_flag(v, "dry_run", false),
    })
}

// ---------------------------------------------------------------------------
// 定位 avs2bdnxml（在 find_exe 的基础上多认一个 SUPFAST_MOD 环境变量）
// ---------------------------------------------------------------------------

fn locate_exe(o: &RunOpts) -> Option<(PathBuf, String)> {
    if let Some(root) = &o.mod_root {
        let p = PathBuf::from(root);
        let exe = p.join("avs2bdnxml.exe");
        if exe.is_file() {
            let dll = p.join("csri").join("VSFilterMod.dll");
            let dll_s = if dll.is_file() { dll.to_string_lossy().to_string() } else { String::new() };
            return Some((exe, dll_s));
        }
    }
    if let Ok(env_root) = std::env::var("SUPFAST_MOD") {
        let p = PathBuf::from(env_root);
        let exe = p.join("avs2bdnxml.exe");
        if exe.is_file() {
            let dll = p.join("csri").join("VSFilterMod.dll");
            let dll_s = if dll.is_file() { dll.to_string_lossy().to_string() } else { String::new() };
            return Some((exe, dll_s));
        }
    }
    find_exe(None)
}

// ---------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------

pub fn analyze(opts: &Value) -> Result<Value, String> {
    let o = read_opts(opts)?;
    let ass_path = strip_verbatim(&o.ass.to_string_lossy());
    let ass = parse_ass(Path::new(&ass_path), o.fps).map_err(|e| format!("读取 ASS 失败: {}", e))?;
    if ass.events.is_empty() {
        return Err("ASS 里没有可用的 Dialogue".into());
    }

    let w = if o.width > 0 { o.width } else if ass.playres_x > 0 { ass.playres_x } else { 1920 };
    let h = if o.height > 0 { o.height } else if ass.playres_y > 0 { ass.playres_y } else { 1080 };
    let last_frame = ass.events.iter().map(|e| e.end).max().unwrap_or(0);
    let frames = (((last_frame as f64 / o.fps + 1.0) * o.fps).round() as u32).max(last_frame + 1);

    let segs = build_segments(&ass.events);
    let sta = segs.iter().filter(|s| !s.dynamic).count();
    let dyn_cnt = segs.iter().filter(|s| s.dynamic).count();
    let dyn_frames: u32 = segs.iter().filter(|s| s.dynamic).map(|s| s.end - s.start).sum();
    let work = dyn_frames + sta as u32 * MIN_STATIC_FRAMES;
    let duration = last_frame as f64 / o.fps;

    // 表格只给前 240 段；刻度带要整片，用压缩的 bands
    let timeline: Vec<Value> = segs
        .iter()
        .take(240)
        .enumerate()
        .map(|(i, s)| {
            json!({
                "i": i,
                "a": (s.start as f64 / o.fps * 1000.0).round() / 1000.0,
                "b": (s.end as f64 / o.fps * 1000.0).round() / 1000.0,
                "a_tc": frames_to_tc(s.start, o.fps),
                "b_tc": frames_to_tc(s.end, o.fps),
                "dyn": s.dynamic,
                "frames": s.end - s.start,
            })
        })
        .collect();

    let bands: Vec<Value> = segs
        .iter()
        .map(|s| json!([
            (s.start as f64 / o.fps * 1000.0).round() / 1000.0,
            (s.end as f64 / o.fps * 1000.0).round() / 1000.0,
            if s.dynamic { 1 } else { 0 }
        ]))
        .collect();

    // warnings 只放真会让结果不对/变慢的东西；纯信息（全静态、多少帧含动态标签）放 notes，
    // 界面上一次性记到日志里，不当弹窗——否则每次改设置重新分析都会弹同一条。
    let mut warnings: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let timer_is_precision = (ass.timer_fps - 100.0).abs() < 0.01 || (ass.timer_fps - 1000.0).abs() < 0.01;
    if ass.timer_fps > 0.0 && !timer_is_precision && (ass.timer_fps - o.fps).abs() > 0.01 {
        warnings.push(format!(
            "ASS 声明 Timer={}，与当前帧率 {} 不一致，时间轴可能整体偏移",
            ass.timer_fps, o.fps
        ));
    }
    if segs.len() > MAX_SEGMENTS {
        warnings.push(format!("切段数 {} 超过上限 {}，逐段开销会吃掉收益", segs.len(), MAX_SEGMENTS));
    }
    if sta > 0 && dyn_cnt == 0 {
        notes.push("全部为静态段，加速比最高".into());
    }
    if dyn_frames > 0 {
        notes.push(format!("有 {} 帧含动态标签，必须逐帧渲染", dyn_frames));
    }

    let seg_cost = 0.25 + 0.0002 * ass.events.len() as f64;
    let est_orig = frames as f64 * 0.0031;
    let est_fast = work as f64 * 0.0031 + (segs.len() as f64 * seg_cost) / o.jobs as f64;

    let name = Path::new(&ass_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "subtitle".into());
    // 界面要显示「产物会落在哪」，统一由后端算，避免前端另写一套路径规则
    let outdir = resolve_outdir(Path::new(&ass_path), o.outdir.as_deref(), &name);

    Ok(json!({
        "ok": true,
        "ass": ass_path,
        "name": name,
        "outdir": outdir.to_string_lossy(),
        "events": ass.events.len(),
        "frames": frames,
        "duration": (duration * 1000.0).round() / 1000.0,
        "duration_hms": hms(duration),
        "w": w,
        "h": h,
        "fps": o.fps,
        "segs": segs.len(),
        "sta": sta,
        "dyn": dyn_cnt,
        "dyn_frames": dyn_frames,
        "work": work,
        "ratio": if frames > 0 { ((work as f64 / frames as f64 * 1000.0).round()) / 10.0 } else { 0.0 },
        "est_orig": (est_orig * 100.0).round() / 100.0,
        "est_fast": (est_fast * 100.0).round() / 100.0,
        "styles": segs.is_empty().then_some(0).unwrap_or(0),
        "warnings": warnings,
        "notes": notes,
        "timeline": timeline,
        "bands": bands,
        "oversize": segs.len() > 240,
    }))
}

fn hms(sec: f64) -> String {
    let t = sec.max(0.0);
    format!(
        "{:02}:{:02}:{:02}",
        (t / 3600.0).floor() as u64,
        ((t % 3600.0) / 60.0).floor() as u64,
        (t % 60.0).floor() as u64
    )
}

// ---------------------------------------------------------------------------
// 结果 XML：一个 Event 可能挂多个 Graphic（多窗口字幕）
// ---------------------------------------------------------------------------

/// 取标签属性值。不构造 `format!("{}=\"", key)` —— 每个 Graphic 要调 8 次，
/// 整片上万个临时字符串全是为了比几个字节。
fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0usize;
    while let Some(p) = tag[from..].find(key) {
        let i = from + p + key.len();
        if tag[i..].starts_with("=\"") {
            let rest = &tag[i + 2..];
            let j = rest.find('"')?;
            return Some(&rest[..j]);
        }
        from = i;
    }
    None
}

#[derive(Debug, Clone)]
struct Graphic {
    w: u32,
    h: u32,
    x: i32,
    y: i32,
    png: String,
}

#[derive(Debug, Clone)]
struct ResultEvent {
    forced: String,
    itc: String,
    otc: String,
    graphics: Vec<Graphic>,
}

fn parse_result_xml_all(path: &Path) -> Vec<ResultEvent> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<ResultEvent> = Vec::new();
    let mut rest = text.as_str();
    while let Some(p) = rest.find("<Event ") {
        let tag_end = match rest[p..].find('>') {
            Some(e) => p + e + 1,
            None => break,
        };
        let event_tag = &rest[p..tag_end];
        let close = match rest[tag_end..].find("</Event>") {
            Some(c) => tag_end + c,
            None => break,
        };
        let body = &rest[tag_end..close];
        let mut graphics: Vec<Graphic> = Vec::new();
        let mut b = body;
        while let Some(gs) = b.find("<Graphic ") {
            let gt_end = match b[gs..].find('>') {
                Some(e) => gs + e + 1,
                None => break,
            };
            let gtag = &b[gs..gt_end];
            let gclose = match b[gt_end..].find("</Graphic>") {
                Some(c) => gt_end + c,
                None => break,
            };
            let fname = b[gt_end..gclose].trim().to_string();
            graphics.push(Graphic {
                w: attr(gtag, "Width").and_then(|v| v.parse().ok()).unwrap_or(0),
                h: attr(gtag, "Height").and_then(|v| v.parse().ok()).unwrap_or(0),
                x: attr(gtag, "X").and_then(|v| v.parse().ok()).unwrap_or(0),
                y: attr(gtag, "Y").and_then(|v| v.parse().ok()).unwrap_or(0),
                png: fname,
            });
            b = &b[gclose + "</Graphic>".len()..];
        }
        if let (Some(itc), Some(otc)) = (attr(event_tag, "InTC"), attr(event_tag, "OutTC")) {
            if !graphics.is_empty() {
                out.push(ResultEvent {
                    forced: attr(event_tag, "Forced").unwrap_or("False").to_string(),
                    itc: itc.to_string(),
                    otc: otc.to_string(),
                    graphics,
                });
            }
        }
        rest = &rest[close + "</Event>".len()..];
    }
    out
}

// ---------------------------------------------------------------------------
// 单段调用（带超时 + 杀进程树）
// ---------------------------------------------------------------------------

fn kill_tree(pid: u32) {
    // 绝对路径：CreateProcess 会按当前目录/PATH 搜索，绿色版解压在可写目录时可能被塞同名 exe
    let taskkill = std::env::var("SystemRoot")
        .map(|r| format!("{}\\System32\\taskkill.exe", r))
        .unwrap_or_else(|_| "taskkill".into());
    let mut c = Command::new(taskkill);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    let _ = c
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// 返回 true 表示进程正常退出（不论 avs2bdnxml 自己报什么错，产出文件才是判据）
fn run_seg(
    exe: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
) -> bool {
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                return false;
            }
        }
        if cancel.load(Ordering::SeqCst) || t0.elapsed() > timeout {
            kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        thread::sleep(Duration::from_millis(35));
    }
}

// ---------------------------------------------------------------------------
// convert
// ---------------------------------------------------------------------------

struct Job {
    idx: usize,
    seg: Segment,
    wdir: PathBuf,
    contig: bool,
}

struct Done {
    idx: usize,
    seg: Segment,
    wdir: PathBuf,
    contig: bool,
    ok: bool,
    events: Vec<ResultEvent>,
}

pub fn convert(opts: &Value, emit: Emit, cancel: Arc<AtomicBool>) -> Result<Value, String> {
    let o = read_opts(opts)?;
    let ass_path_str = strip_verbatim(&o.ass.to_string_lossy());
    let ass_path = PathBuf::from(&ass_path_str);
    let ass = parse_ass(&ass_path, o.fps).map_err(|e| format!("读取 ASS 失败: {}", e))?;
    if ass.events.is_empty() {
        return Err("ASS 里没有可用的 Dialogue".into());
    }

    let w = if o.width > 0 { o.width } else if ass.playres_x > 0 { ass.playres_x } else { 1920 };
    let h = if o.height > 0 { o.height } else if ass.playres_y > 0 { ass.playres_y } else { 1080 };
    let last_end = ass.events.iter().map(|e| e.end).max().unwrap_or(0);
    let dur = last_end as f64 / o.fps + 1.0;
    let total_frames = ((dur * o.fps).round() as u32).max(last_end + 1);

    let segs = build_segments(&ass.events);
    if segs.is_empty() {
        return Err("切段结果为空".into());
    }

    // 「只看计划」：切完就返回，不调 avs2bdnxml、不建输出目录。
    // 这个开关之前只是读进来没用，勾了照样整片渲染。
    if o.dry_run {
        let sta = segs.iter().filter(|s| !s.dynamic).count();
        let dyn_frames: u32 = segs.iter().filter(|s| s.dynamic).map(|s| s.end - s.start).sum();
        emit(json!({"t":"stage","name":"plan","total":segs.len()}));
        return Ok(json!({
            "t":"done", "ok": true, "dry_run": true,
            "outdir": "", "xml": "", "sup": "",
            "pngs": 0, "epochs": 0, "failed": 0, "failed_segs": [],
            "secs": 0.0, "frames": total_frames, "speed": 0.0, "sup_size": 0,
            "events": ass.events.len(), "segs": segs.len(),
            "sta": sta, "dyn_frames": dyn_frames,
        }));
    }
    if segs.len() > MAX_SEGMENTS {
        return Err(format!(
            "切段数 {} 超过上限 {}；逐段调用的固定开销会吃掉收益。请拆分子幕或直接用原版。",
            segs.len(),
            MAX_SEGMENTS
        ));
    }

    let (exe, dll) = locate_exe(&o).ok_or("找不到 avs2bdnxml.exe，请在「环境」里指定 mod 目录")?;

    let base = ass_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".into());
    let outdir = resolve_outdir(&ass_path, o.outdir.as_deref(), &base);
    fs::create_dir_all(&outdir).map_err(|e| format!("建输出目录失败: {}", e))?;

    // 临时目录必须独占创建：名字若可预测，别人可以预先建好同名 junction / 符号链接，
    // create_dir_all 对已存在目录不报错，后续写入就被重定向到别处。
    let tmp = {
        let base = std::env::temp_dir();
        let mut picked = None;
        for _ in 0..8 {
            let cand = base.join(format!("supfast_{:016x}", rand_u64()));
            match fs::create_dir(&cand) {
                Ok(()) => {
                    picked = Some(cand);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("建临时目录失败: {}", e)),
            }
        }
        picked.ok_or("建临时目录失败：连续 8 次都撞名")?
    };

    let t0 = Instant::now();
    let result = (|| -> Result<Value, String> {
        // ---------- 准备 ----------
        emit(json!({"t":"stage","name":"plan","total":segs.len()}));
        let mut jobs: Vec<Job> = Vec::with_capacity(segs.len());
        for (i, seg) in segs.iter().enumerate() {
            let wdir = tmp.join(format!("w{:05}", i));
            fs::create_dir_all(&wdir).map_err(|e| format!("建目录失败: {}", e))?;
            let avs = avs_for_segment(&dll, &ass_path_str, w, h, o.fps, total_frames, seg);
            fs::write(wdir.join("seg.avs"), avs.as_bytes()).map_err(|e| format!("写 AVS 失败: {}", e))?;
            let contig = i + 1 < segs.len() && segs[i + 1].start == seg.end;
            jobs.push(Job { idx: i, seg: *seg, wdir, contig });
        }

        let args: Vec<String> = {
            let mut a: Vec<String> = vec![
                "-t".into(), o.title.clone(),
                "-l".into(), o.lang.clone(),
                "-v".into(), o.vfmt.clone(),
                "-f".into(), format!("{}", o.fps),
                "-m".into(), "3".into(),
                "-u".into(), "0".into(),
                "-e".into(), o.even_y.to_string(),
                "-n".into(), "0".into(),
                "-z".into(), o.stricter.to_string(),
                "-p".into(), o.palette.to_string(),
                "-b".into(), o.bufsplit.to_string(),
                "-a".into(), o.autocrop.to_string(),
                "-o".into(), "seg.xml".into(),
            ];
            if !o.no_sup {
                a.push("-o".into());
                a.push("seg.sup".into());
            }
            a.push("seg.avs".into());
            a
        };

        // ---------- 并行渲染 ----------
        let total_jobs = jobs.len();
        emit(json!({"t":"stage","name":"render","total":total_jobs}));
        let queue = Arc::new(Mutex::new(jobs));
        let outs: Arc<Mutex<Vec<Done>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));
        let pngs = Arc::new(AtomicUsize::new(0));
        let exe = Arc::new(exe);
        let args = Arc::new(args);

        let mut handles = Vec::new();
        for _ in 0..o.jobs.max(1) {
            let queue = Arc::clone(&queue);
            let outs = Arc::clone(&outs);
            let done = Arc::clone(&done);
            let pngs = Arc::clone(&pngs);
            let exe = Arc::clone(&exe);
            let args = Arc::clone(&args);
            let emit = Arc::clone(&emit);
            let cancel = Arc::clone(&cancel);
            let no_sup = o.no_sup;
            let fps = o.fps;
            handles.push(thread::spawn(move || loop {
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
                let job = {
                    let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                    q.pop()
                };
                let job = match job {
                    Some(j) => j,
                    None => break,
                };

                let xml_path = job.wdir.join("seg.xml");
                let sup_path = job.wdir.join("seg.sup");
                let mut ok = false;
                for _ in 0..SEG_ATTEMPTS {
                    run_seg(&exe, &args, &job.wdir, Duration::from_secs(SEG_TIMEOUT_SECS), &cancel);
                    ok = xml_path.is_file() && (no_sup || sup_path.is_file());
                    if ok {
                        break;
                    }
                    // 清掉半成品，免得读到半截文件
                    for f in ["seg.xml", "seg.sup", "retimed.sup"] {
                        let _ = fs::remove_file(job.wdir.join(f));
                    }
                    if cancel.load(Ordering::SeqCst) {
                        break;
                    }
                }

                let events = if ok { parse_result_xml_all(&xml_path) } else { Vec::new() };
                let n_png: usize = events.iter().map(|e| e.graphics.len()).sum();
                pngs.fetch_add(n_png, Ordering::SeqCst);

                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                if !ok {
                    emit(json!({
                        "t":"warn","i":job.idx,"tc":frames_to_tc(job.seg.start, fps),
                        "msg":format!("第 {} 段渲染失败（已重试 {} 次）", job.idx, SEG_ATTEMPTS)
                    }));
                }
                emit(json!({"t":"seg","done":n,"total":total_jobs,"pngs":pngs.load(Ordering::SeqCst)}));

                outs.lock().unwrap_or_else(|e| e.into_inner()).push(Done {
                    idx: job.idx,
                    seg: job.seg,
                    wdir: job.wdir.clone(),
                    contig: job.contig,
                    ok,
                    events,
                });
            }));
        }
        for h in handles {
            let _ = h.join();
        }

        if cancel.load(Ordering::SeqCst) {
            return Err("已取消".into());
        }

        let mut outs = {
            let mut g = outs.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *g)
        };
        outs.sort_by_key(|d| d.seg.start);

        // ---------- 收集 PNG / 重映射 SUP ----------
        emit(json!({"t":"stage","name":"write","render_secs":(t0.elapsed().as_secs_f64()*1000.0).round()/1000.0}));

        struct OutEv {
            rs: u32,
            re: u32,
            forced: String,
            graphics: Vec<(String, u32, u32, i32, i32)>,
        }
        let mut rendered: Vec<OutEv> = Vec::new();
        let mut sup_parts: Vec<(u32, PathBuf)> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();

        for d in &outs {
            if !d.ok {
                failed.push(json!({
                    "i": d.idx,
                    "tc": frames_to_tc(d.seg.start, o.fps),
                }));
            }
            let keep: Vec<&ResultEvent> = if d.seg.dynamic {
                d.events.iter().collect()
            } else {
                d.events.iter().take(1).collect()
            };
            for ev in keep {
                let (rs, re) = if d.seg.dynamic {
                    // 段内会去重，下标≠帧序，用子 XML 的 InTC 反算
                    let cf = tc_to_frames(&ev.itc, o.fps);
                    let of = tc_to_frames(&ev.otc, o.fps);
                    let rs = d.seg.start + cf;
                    let re = (d.seg.start + of.max(cf + 1)).min(d.seg.end);
                    (rs, re)
                } else {
                    (d.seg.start, d.seg.end)
                };
                let mut gs: Vec<(String, u32, u32, i32, i32)> = Vec::new();
                for (k, g) in ev.graphics.iter().enumerate() {
                    let src = d.wdir.join(&g.png);
                    if !src.is_file() {
                        continue;
                    }
                    let newname = format!("{:08}_{}.png", rs, k);
                    let dst = outdir.join(&newname);
                    if !dst.is_file() {
                        if fs::copy(&src, &dst).is_err() {
                            continue;
                        }
                    }
                    gs.push((newname, g.w, g.h, g.x, g.y));
                }
                if !gs.is_empty() {
                    rendered.push(OutEv { rs, re, forced: ev.forced.clone(), graphics: gs });
                }
            }

            if !o.no_sup {
                let sp = d.wdir.join("seg.sup");
                if sp.is_file() {
                    if let Ok(raw) = fs::read(&sp) {
                        let comp_n = if d.seg.dynamic { d.seg.end - d.seg.start } else { MIN_STATIC_FRAMES };
                        let retimed =
                            sup_retime(&raw, o.fps, d.seg.start, comp_n, d.seg.end - d.seg.start, d.contig)
                                .map_err(|e| {
                                    format!("第 {} 段（{}）{}", d.idx, frames_to_tc(d.seg.start, o.fps), e)
                                })?;
                        let rf = d.wdir.join("retimed.sup");
                        if fs::write(&rf, &retimed).is_ok() {
                            sup_parts.push((d.seg.start, rf));
                        }
                    }
                }
            }
        }

        if rendered.is_empty() {
            return Err("一张图都没产出".into());
        }

        // ---------- 写 XML ----------
        rendered.sort_by_key(|r| r.rs);
        let first = rendered[0].rs;
        let lastf = rendered.iter().map(|r| r.re).max().unwrap_or(first);
        let n_png: usize = rendered.iter().map(|r| r.graphics.len()).sum();

        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        xml.push_str("<BDN Version=\"0.93\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n");
        xml.push_str("xsi:noNamespaceSchemaLocation=\"BD-03-006-0093b BDN File Format.xsd\">\n<Description>\n");
        xml.push_str(&format!("<Name Title=\"{}\" Content=\"\"/>\n", xml_esc(&o.title)));
        xml.push_str(&format!("<Language Code=\"{}\"/>\n", xml_esc(&o.lang)));
        xml.push_str(&format!(
            "<Format VideoFormat=\"{}\" FrameRate=\"{}\" DropFrame=\"false\"/>\n",
            xml_esc(&o.vfmt),
            fps_str(o.fps)
        ));
        xml.push_str(&format!(
            "<Events LastEventOutTC=\"{}\" FirstEventInTC=\"{}\"\nContentInTC=\"{}\" ContentOutTC=\"{}\" NumberofEvents=\"{}\" Type=\"Graphic\"/>\n",
            frames_to_tc(lastf, o.fps),
            frames_to_tc(first, o.fps),
            frames_to_tc(0, o.fps),
            frames_to_tc(total_frames, o.fps),
            rendered.len()
        ));
        xml.push_str("</Description>\n<Events>\n");
        for r in &rendered {
            xml.push_str(&format!(
                "<Event Forced=\"{}\" InTC=\"{}\" OutTC=\"{}\">\n",
                r.forced,
                frames_to_tc(r.rs, o.fps),
                frames_to_tc(r.re, o.fps)
            ));
            for (png, gw, gh, gx, gy) in &r.graphics {
                xml.push_str(&format!(
                    "<Graphic Width=\"{}\" Height=\"{}\" X=\"{}\" Y=\"{}\">{}</Graphic>\n",
                    gw, gh, gx, gy, png
                ));
            }
            xml.push_str("</Event>\n");
        }
        xml.push_str("</Events>\n</BDN>\n");

        let out_xml = outdir.join(format!("{}.xml", base));
        fs::write(&out_xml, xml).map_err(|e| format!("写 XML 失败: {}", e))?;

        // ---------- 拼 SUP（逐段落盘，不把整条揣内存） ----------
        let mut out_sup = String::new();
        if !o.no_sup && !sup_parts.is_empty() {
            sup_parts.sort_by_key(|(at, _)| *at);
            let sup_path = outdir.join(format!("{}.sup", base));
            fs::write(&sup_path, b"").map_err(|e| format!("写 SUP 失败: {}", e))?;
            for (_, part) in &sup_parts {
                if let Ok(buf) = fs::read(part) {
                    use std::io::Write;
                    let mut f = fs::OpenOptions::new()
                        .append(true)
                        .open(&sup_path)
                        .map_err(|e| format!("写 SUP 失败: {}", e))?;
                    f.write_all(&buf).map_err(|e| format!("写 SUP 失败: {}", e))?;
                }
            }
            out_sup = sup_path.to_string_lossy().to_string();
        }

        let sup_size = if out_sup.is_empty() {
            0
        } else {
            fs::metadata(&out_sup).map(|m| m.len()).unwrap_or(0)
        };
        let total_secs = t0.elapsed().as_secs_f64();
        let speed = if total_secs > 0.0 {
            ((total_frames as f64 * 0.0031 / total_secs) * 100.0).round() / 100.0
        } else {
            0.0
        };

        Ok(json!({
            "t":"done",
            "ok": true,
            "outdir": outdir.to_string_lossy(),
            "xml": out_xml.to_string_lossy(),
            "sup": out_sup,
            "pngs": n_png,
            "epochs": sup_parts.len(),
            "failed": failed.len(),
            "failed_segs": failed,
            "secs": (total_secs * 1000.0).round() / 1000.0,
            "frames": total_frames,
            "speed": speed,
            "sup_size": sup_size,
            "events": ass.events.len(),
            "segs": segs.len(),
        }))
    })();

    let _ = fs::remove_dir_all(&tmp);
    result
}

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// 环境自检
// ---------------------------------------------------------------------------

pub fn probe_env(mod_root: Option<&str>) -> Value {
    let o = RunOpts {
        ass: PathBuf::new(),
        fps: 23.976,
        jobs: 1,
        width: 0,
        height: 0,
        vfmt: String::new(),
        lang: String::new(),
        title: String::new(),
        outdir: None,
        mod_root: mod_root.filter(|s| !s.is_empty()).map(|s| s.to_string()),
        palette: 1,
        autocrop: 1,
        bufsplit: 1,
        stricter: 0,
        even_y: 0,
        no_sup: false,
        dry_run: false,
    };
    let found = locate_exe(&o);
    let (exe_s, dll_s, dir_s) = match &found {
        Some((exe, dll)) => (
            exe.to_string_lossy().to_string(),
            dll.clone(),
            exe.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        ),
        None => (String::new(), String::new(), String::new()),
    };

    let mut avs = String::new();
    for p in [r"C:\Windows\System32\avisynth.dll", r"C:\Windows\SysWOW64\avisynth.dll"] {
        if Path::new(p).is_file() {
            avs = p.to_string();
            break;
        }
    }

    json!({
        "mod_dir": dir_s,
        "items": [
            {"key":"avs2bdnxml","name":"avs2bdnxml.exe","ok":!exe_s.is_empty(),
             "detail": if exe_s.is_empty() { "未找到（可在「环境」里指定 mod 目录）".to_string() } else { exe_s }},
            {"key":"vsfilter","name":"VSFilterMod.dll","ok":!dll_s.is_empty(),
             "detail": if dll_s.is_empty() { "未找到（应与 avs2bdnxml.exe 同级的 csri\\ 下）".to_string() } else { dll_s }},
            {"key":"avisynth","name":"AviSynth","ok":!avs.is_empty(),
             "detail": if avs.is_empty() { "未安装 AviSynth".to_string() } else { format!("{}（32 位）", avs) }},
            {"key":"runtime","name":"WebView2 运行时","ok":true,"detail":"Tauri 2 + WebView2（系统自带）"},
            {"key":"engine","name":"内置引擎","ok":true,"detail":"Rust 原生实现（无需 Python / Node）"},
        ],
    })
}
