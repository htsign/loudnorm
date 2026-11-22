//! Lossless gain changes: MP3 global_gain (mp3gain style) and the Opus header output gain,
//! plus splitting MP3 files into leading/trailing tag bytes and audio.

use anyhow::{Result, bail};

/// Byte ranges of an MP3 file: [0, audio_start) leading ID3v2 tags, [audio_end, len) APEv2/ID3v1.
pub fn mp3_layout(d: &[u8]) -> (usize, usize) {
    let mut start = 0usize;
    while d.len() >= start + 10 && &d[start..start + 3] == b"ID3" {
        let sz = ((d[start + 6] as usize & 0x7f) << 21)
            | ((d[start + 7] as usize & 0x7f) << 14)
            | ((d[start + 8] as usize & 0x7f) << 7)
            | (d[start + 9] as usize & 0x7f);
        let footer = if d[start + 5] & 0x10 != 0 { 10 } else { 0 };
        start = (start + 10 + sz + footer).min(d.len());
    }
    let mut end = d.len();
    if end >= start + 128 && &d[end - 128..end - 125] == b"TAG" {
        end -= 128;
    }
    if end >= start + 32 && &d[end - 32..end - 24] == b"APETAGEX" {
        let f = &d[end - 32..end];
        let size = u32::from_le_bytes([f[12], f[13], f[14], f[15]]) as usize;
        let flags = u32::from_le_bytes([f[20], f[21], f[22], f[23]]);
        let total = size + if flags & 0x8000_0000 != 0 { 32 } else { 0 };
        if total <= end - start {
            end -= total;
        }
    }
    (start, end)
}

struct Hdr {
    len: usize,
    mpeg1: bool,
    mono: bool,
    crc: bool,
}

fn header(b: &[u8]) -> Option<Hdr> {
    if b.len() < 4 || b[0] != 0xFF || (b[1] & 0xE0) != 0xE0 {
        return None;
    }
    let ver = (b[1] >> 3) & 3; // 0: MPEG2.5, 2: MPEG2, 3: MPEG1
    let layer = (b[1] >> 1) & 3; // 1: Layer III
    let bri = (b[2] >> 4) as usize;
    let sri = ((b[2] >> 2) & 3) as usize;
    if ver == 1 || layer != 1 || bri == 0 || bri == 15 || sri == 3 {
        return None;
    }
    const BR1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const BR2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let base = [44100u32, 48000, 32000][sri];
    let mpeg1 = ver == 3;
    let sr = base / if mpeg1 { 1 } else if ver == 2 { 2 } else { 4 };
    let br = if mpeg1 { BR1[bri] } else { BR2[bri] } * 1000;
    let pad = ((b[2] >> 1) & 1) as u32;
    let len = (if mpeg1 { 144 * br / sr } else { 72 * br / sr } + pad) as usize;
    Some(Hdr { len, mpeg1, mono: (b[3] >> 6) == 3, crc: b[1] & 1 == 0 })
}

fn get_bits(d: &[u8], bit: usize, n: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..n {
        let b = bit + i;
        v = (v << 1) | ((d[b / 8] >> (7 - b % 8)) & 1) as u32;
    }
    v
}

fn set_bits(d: &mut [u8], bit: usize, n: usize, v: u32) {
    for i in 0..n {
        let b = bit + i;
        let mask = 1u8 << (7 - b % 8);
        if (v >> (n - 1 - i)) & 1 == 1 { d[b / 8] |= mask } else { d[b / 8] &= !mask }
    }
}

fn crc16_mpeg(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in bytes {
        for i in (0..8).rev() {
            let bit = (byte >> i) & 1;
            let top = (crc >> 15) as u8 & 1;
            crc <<= 1;
            if top ^ bit == 1 {
                crc ^= 0x8005;
            }
        }
    }
    crc
}

pub struct Mp3GainStats {
    pub frames: usize,
    pub clamped: usize,
}

/// Add `steps` (1.5 dB each) to every granule's global_gain. Tags and frame layout are untouched.
pub fn mp3_apply_gain(d: &mut [u8], steps: i32) -> Result<Mp3GainStats> {
    let (start, end) = mp3_layout(d);
    let mut pos = start;
    let mut st = Mp3GainStats { frames: 0, clamped: 0 };
    let mut synced = false;
    while pos + 4 <= end {
        let Some(h) = header(&d[pos..end]) else {
            pos += 1;
            synced = false;
            continue;
        };
        if pos + h.len > end {
            break;
        }
        // Accept a new sync point only when the next frame header lines up as well.
        if !synced && pos + h.len + 4 <= end && header(&d[pos + h.len..end]).is_none() {
            pos += 1;
            continue;
        }
        synced = true;
        let si = pos + 4 + if h.crc { 2 } else { 0 };
        let si_len = match (h.mpeg1, h.mono) {
            (true, true) => 17,
            (true, false) => 32,
            (false, true) => 9,
            (false, false) => 17,
        };
        if si + si_len + 4 > end {
            break;
        }
        let tag = &d[si + si_len..si + si_len + 4];
        if tag == b"Xing" || tag == b"Info" || (pos + 40 <= end && &d[pos + 36..pos + 40] == b"VBRI") {
            pos += h.len;
            continue;
        }
        let nch = if h.mono { 1 } else { 2 };
        let (ngr, first, block) = if h.mpeg1 {
            (2, if h.mono { 18 } else { 20 }, 59)
        } else {
            (1, if h.mono { 9 } else { 10 }, 63)
        };
        {
            let side = &mut d[si..si + si_len];
            for gr in 0..ngr {
                for ch in 0..nch {
                    let bit = first + (gr * nch + ch) * block + 21;
                    let g = get_bits(side, bit, 8) as i32 + steps;
                    let ng = g.clamp(0, 255);
                    if ng != g {
                        st.clamped += 1;
                    }
                    set_bits(side, bit, 8, ng as u32);
                }
            }
        }
        if h.crc {
            let mut buf = Vec::with_capacity(2 + si_len);
            buf.extend_from_slice(&d[pos + 2..pos + 4]);
            buf.extend_from_slice(&d[si..si + si_len]);
            let c = crc16_mpeg(&buf);
            d[pos + 4] = (c >> 8) as u8;
            d[pos + 5] = c as u8;
        }
        st.frames += 1;
        pos += h.len;
    }
    if st.frames == 0 {
        bail!("MP3 フレームが見つかりません");
    }
    Ok(st)
}

fn crc32_ogg(d: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in d {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// Add `gain_db` to the OpusHead output gain (Q7.8) and fix the page CRC. Returns the new header gain in dB.
pub fn opus_apply_gain(d: &mut [u8], gain_db: f64) -> Result<f64> {
    if d.len() < 28 || &d[0..4] != b"OggS" {
        bail!("Ogg ページが見つかりません");
    }
    let nseg = d[26] as usize;
    let body = 27 + nseg;
    let plen: usize = d[27..body].iter().map(|&x| x as usize).sum();
    if d.len() < body + plen || plen < 19 || &d[body..body + 8] != b"OpusHead" {
        bail!("OpusHead がありません");
    }
    let off = body + 16;
    let old = i16::from_le_bytes([d[off], d[off + 1]]) as i32;
    let new = old + (gain_db * 256.0).round() as i32;
    if !(i16::MIN as i32..=i16::MAX as i32).contains(&new) {
        bail!("ヘッダゲインの範囲を超えます");
    }
    d[off..off + 2].copy_from_slice(&(new as i16).to_le_bytes());
    d[22..26].fill(0);
    let c = crc32_ogg(&d[..body + plen]);
    d[22..26].copy_from_slice(&c.to_le_bytes());
    Ok(new as f64 / 256.0)
}
