//! 极简 PNG 读写（8bit RGB / RGBA，非交错，零第三方图像库）。
//!
//! `write_rgba_png` 给基准导出参考画面；`read_rgba_png` 给真机录屏对照读回
//! 这些参考画面。只依赖已有的 `flate2`。

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
        }
    }
    !c
}

pub fn chunk(out: &mut Vec<u8>, typ: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(data);
    let mut c = Vec::with_capacity(4 + data.len());
    c.extend_from_slice(typ);
    c.extend_from_slice(data);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}

pub fn write_rgba_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
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

fn rd32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

/// 读 8bit 非交错 PNG，统一返回 RGBA8。支持 PNG filter 0..4。
pub fn read_rgba_png(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let d = fs::read(path).map_err(|e| format!("读 PNG 失败 {}: {}", path.display(), e))?;
    if d.len() < 8 || &d[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(format!("不是 PNG：{}", path.display()));
    }
    let mut o = 8usize;
    let mut w = 0u32;
    let mut h = 0u32;
    let mut depth = 0u8;
    let mut ctype = 0u8;
    let mut interlace = 0u8;
    let mut idat: Vec<u8> = Vec::new();
    while o + 12 <= d.len() {
        let len = rd32(&d, o) as usize;
        if o + 12 + len > d.len() {
            return Err("PNG chunk 越界".into());
        }
        let typ = &d[o + 4..o + 8];
        let data = &d[o + 8..o + 8 + len];
        match typ {
            b"IHDR" => {
                if data.len() < 13 {
                    return Err("IHDR 太短".into());
                }
                w = rd32(data, 0);
                h = rd32(data, 4);
                depth = data[8];
                ctype = data[9];
                interlace = data[12];
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        o += 12 + len;
    }
    if w == 0 || h == 0 || depth != 8 || interlace != 0 || (ctype != 6 && ctype != 2) {
        return Err(format!(
            "不支持的 PNG：{}x{} depth={} ctype={} interlace={}（只支持 8bit RGB/RGBA 非交错）",
            w, h, depth, ctype, interlace
        ));
    }
    let bpp = if ctype == 6 { 4usize } else { 3usize };
    let rowlen = w as usize * bpp;
    let mut dec = ZlibDecoder::new(&idat[..]);
    let mut raw: Vec<u8> = Vec::new();
    dec.read_to_end(&mut raw)
        .map_err(|e| format!("PNG zlib 解压失败: {}", e))?;
    if raw.len() < (rowlen + 1) * h as usize {
        return Err(format!(
            "PNG 像素数据不足：{} 字节，需要 {}",
            raw.len(),
            (rowlen + 1) * h as usize
        ));
    }
    let mut out = vec![0u8; w as usize * h as usize * 4];
    let mut prev = vec![0u8; rowlen];
    for y in 0..h as usize {
        let base = y * (rowlen + 1);
        let f = raw[base];
        let src = &raw[base + 1..base + 1 + rowlen];
        let mut cur = vec![0u8; rowlen];
        for x in 0..rowlen {
            let a = if x >= bpp { cur[x - bpp] } else { 0 };
            let b = prev[x];
            let c = if x >= bpp { prev[x - bpp] } else { 0 };
            cur[x] = match f {
                0 => src[x],
                1 => src[x].wrapping_add(a),
                2 => src[x].wrapping_add(b),
                3 => src[x].wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => src[x].wrapping_add(paeth(a, b, c)),
                _ => return Err(format!("未知 PNG filter {}", f)),
            };
        }
        for x in 0..w as usize {
            let q = (y * w as usize + x) * 4;
            let s = x * bpp;
            out[q] = cur[s];
            out[q + 1] = cur[s + 1];
            out[q + 2] = cur[s + 2];
            out[q + 3] = if ctype == 6 { cur[s + 3] } else { 255 };
        }
        prev = cur;
    }
    Ok((w, h, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip_has_valid_signature() {
        let p = std::env::temp_dir().join("supfast_pngutil_round.png");
        write_rgba_png(&p, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 128]).unwrap();
        let d = fs::read(&p).unwrap();
        assert_eq!(&d[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&d[12..16], b"IHDR");
        let (w, h, rgba) = read_rgba_png(&p).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 255, 0, 128]);
        let _ = fs::remove_file(&p);
    }
}
