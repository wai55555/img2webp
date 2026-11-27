//! Raw 16-bit PNG decoding for Stealth PNG Info extraction.
//!
//! Pillow reduces 16-bit PNGs to 8-bit, so the low bytes of a 16-bit sample are
//! only reachable with a dedicated decoder. This is a port of the Python
//! implementation's `png_16bit_pixels()` with the same limits (design spec
//! §1.7, §3.2 `png_raw.rs`).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::interrupt::Interrupt;

pub const MAX_PNG_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_SIDE: u32 = 16384;

pub struct Raw16 {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
}

impl Raw16 {
    #[inline]
    pub fn sample(&self, x: usize, y: usize, channel: usize) -> u8 {
        let stride = self.width * self.channels * 2 + 1;
        self.data[y * stride + 1 + x * self.channels * 2 + channel * 2 + 1]
    }
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub fn png_16bit_pixels(path: &Path, header_only: bool, interrupt: &Interrupt) -> Option<Raw16> {
    let file = File::open(path).ok()?;
    let mut stream = BufReader::with_capacity(64 * 1024, file);
    let mut header = [0u8; 33];
    stream.read_exact(&mut header).ok()?;
    if header[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        || header[8..16] != [0, 0, 0, 13, b'I', b'H', b'D', b'R']
    {
        return None;
    }
    let width = be_u32(&header[16..20]) as usize;
    let height = be_u32(&header[20..24]) as usize;
    let depth = header[24];
    let color = header[25];
    let compression = header[26];
    let filtering = header[27];
    let interlace = header[28];
    let channels = match color {
        2 => 3usize,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    if depth != 16
        || compression != 0
        || filtering != 0
        || interlace != 0
        || width == 0
        || width > MAX_SIDE as usize
        || height == 0
        || height > MAX_SIDE as usize
    {
        return None;
    }
    let stride = width * channels * 2;
    let expected = (stride + 1).checked_mul(height)?;
    if expected > MAX_PNG_BYTES {
        return None;
    }

    // Collect the IDAT stream (cumulative size limited like the Python code),
    // then inflate it with a bound of `expected + 1` bytes.
    let mut idat: Vec<u8> = Vec::new();
    let mut saw_idat = false;
    let mut ended_idat = false;
    loop {
        if interrupt.requested() {
            return None;
        }
        let mut chunk_header = [0u8; 8];
        if stream.read_exact(&mut chunk_header).is_err() {
            return None;
        }
        let length = be_u32(&chunk_header[..4]) as usize;
        let kind = &chunk_header[4..8];
        if kind == b"IDAT" {
            if ended_idat || idat.len() + length > MAX_PNG_BYTES {
                return None;
            }
            saw_idat = true;
            let mut remaining = length;
            let mut block = vec![0u8; 64 * 1024];
            while remaining > 0 {
                let take = remaining.min(block.len());
                if stream.read_exact(&mut block[..take]).is_err() {
                    return None;
                }
                idat.extend_from_slice(&block[..take]);
                remaining -= take;
            }
        } else {
            if saw_idat {
                ended_idat = true;
            }
            if stream.seek(SeekFrom::Current(length as i64)).is_err() {
                return None;
            }
        }
        if stream.read_exact(&mut [0u8; 4]).is_err() {
            return None;
        }
        if kind == b"IEND" {
            break;
        }
    }
    if !saw_idat {
        return None;
    }

    let mut decoded: Vec<u8> = Vec::with_capacity(expected + 1);
    {
        use flate2::read::ZlibDecoder;
        let mut decoder = ZlibDecoder::new(&idat[..]);
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            if interrupt.requested() {
                return None;
            }
            match decoder.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    decoded.extend_from_slice(&buffer[..count]);
                    if decoded.len() > expected {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }
    if decoded.len() != expected {
        return None;
    }

    let bpp = channels * 2;
    let rows = if header_only { height.min(152) } else { height };
    let columns = if header_only {
        width.min(152_usize.div_ceil(height))
    } else {
        width
    };
    for y in 0..rows {
        if interrupt.requested() {
            return None;
        }
        let row = y * (stride + 1);
        let method = decoded[row];
        if method > 4 {
            return None;
        }
        if method == 0 {
            continue;
        }
        for x in 0..columns * bpp {
            let i = row + 1 + x;
            let left = if x >= bpp { decoded[i - bpp] } else { 0 };
            let above = if y > 0 { decoded[i - stride - 1] } else { 0 };
            let upper_left = if y > 0 && x >= bpp {
                decoded[i - stride - 1 - bpp]
            } else {
                0
            };
            let predictor = match method {
                1 => left,
                2 => above,
                3 => ((left as u16 + above as u16) / 2) as u8,
                _ => {
                    let p = left as i32 + above as i32 - upper_left as i32;
                    let dl = (p - left as i32).abs();
                    let da = (p - above as i32).abs();
                    let du = (p - upper_left as i32).abs();
                    if dl <= da && dl <= du {
                        left
                    } else if da <= du {
                        above
                    } else {
                        upper_left
                    }
                }
            };
            decoded[i] = decoded[i].wrapping_add(predictor);
        }
    }

    Some(Raw16 {
        data: decoded,
        width,
        height,
        channels,
    })
}
