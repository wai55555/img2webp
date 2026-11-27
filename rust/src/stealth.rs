//! Stealth PNG Info extraction (column-major LSB steganography).
//!
//! Port of the Python `stealth_text()` including the asymmetric failure
//! semantics between the 8-bit and 16-bit paths (design spec §1.7).

use std::io::Read;
use std::path::Path;

use crate::color::Pixels;
use crate::interrupt::Interrupt;
use crate::png_raw::{self, Raw16};
use crate::probe::{Format, Probe};

pub const MAX_STEALTH_BYTES: usize = 8 * 1024 * 1024;
const MAX_SIDE: u32 = 16384;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Alpha,
    Red,
    Green,
    Blue,
}

struct Mode {
    slots: &'static [Slot],
    prefix: &'static [u8],
}

const ALPHA_MODE: Mode = Mode {
    slots: &[Slot::Alpha],
    prefix: b"stealth_png",
};
const RGB_MODE: Mode = Mode {
    slots: &[Slot::Red, Slot::Green, Slot::Blue],
    prefix: b"stealth_rgb",
};

trait Sampler {
    fn sample(&self, slot: Slot, x: usize, y: usize) -> u8;
}

struct PixelSampler<'a> {
    pixels: &'a Pixels,
}

impl Sampler for PixelSampler<'_> {
    fn sample(&self, slot: Slot, x: usize, y: usize) -> u8 {
        match self.pixels {
            Pixels::Rgb(image) => {
                let pixel = image.get_pixel(x as u32, y as u32);
                match slot {
                    Slot::Red => pixel[0],
                    Slot::Green => pixel[1],
                    Slot::Blue => pixel[2],
                    Slot::Alpha => 255,
                }
            }
            Pixels::Rgba(image) => {
                let pixel = image.get_pixel(x as u32, y as u32);
                match slot {
                    Slot::Red => pixel[0],
                    Slot::Green => pixel[1],
                    Slot::Blue => pixel[2],
                    Slot::Alpha => pixel[3],
                }
            }
        }
    }
}

struct RawSampler<'a> {
    raw: &'a Raw16,
}

impl Sampler for RawSampler<'_> {
    fn sample(&self, slot: Slot, x: usize, y: usize) -> u8 {
        let channel = match slot {
            Slot::Red => 0,
            Slot::Green => 1,
            Slot::Blue => 2,
            Slot::Alpha => self.raw.channels - 1,
        };
        self.raw.sample(x, y, channel)
    }
}

fn read_bits(
    sampler: &dyn Sampler,
    slots: usize,
    _width: usize,
    height: usize,
    start: usize,
    count: usize,
    interrupt: &Interrupt,
) -> Option<Vec<u8>> {
    if count % 8 != 0 {
        return None;
    }
    let bytes = count / 8;
    let mut out = Vec::with_capacity(bytes);
    let mut value: u8 = 0;
    for index in start..start + count {
        if index % (1 << 20) == 0 && interrupt.requested() {
            return None;
        }
        let pixel = index / slots;
        let channel = index % slots;
        let x = pixel / height;
        let y = pixel % height;
        let slot = match channel {
            0 if slots == 1 => Slot::Alpha,
            0 => Slot::Red,
            1 => Slot::Green,
            2 => Slot::Blue,
            _ => return None,
        };
        value = (value << 1) | (sampler.sample(slot, x, y) & 1);
        if (index - start) % 8 == 7 {
            out.push(value);
            value = 0;
        }
    }
    Some(out)
}

fn decompress(payload: &[u8]) -> Option<Vec<u8>> {
    // `zlib.decompressobj(47)` accepts zlib and gzip headers.
    use flate2::read::{GzDecoder, ZlibDecoder};
    let mut out = Vec::new();
    let mut decoder = ZlibDecoder::new(payload);
    if decoder.read_to_end(&mut out).is_ok() && out.len() <= MAX_STEALTH_BYTES {
        return Some(out);
    }
    out.clear();
    let mut gz = GzDecoder::new(payload);
    gz.read_to_end(&mut out).ok()?;
    if out.len() > MAX_STEALTH_BYTES {
        return None;
    }
    Some(out)
}

/// Extracts the Stealth PNG Info text anchored at the already-open first frame.
pub fn stealth_text(
    probe: &Probe,
    path: &Path,
    first_frame: Option<&Pixels>,
    interrupt: &Interrupt,
) -> String {
    let _ = path;
    if probe.format != Format::Png {
        return String::new();
    }
    if probe.bit_depth == 16 {
        return stealth_text_16(path, probe, interrupt);
    }
    let modes: Vec<&Mode> = match probe.color_type {
        4 => vec![&ALPHA_MODE],
        6 => vec![&ALPHA_MODE, &RGB_MODE],
        2 => vec![&RGB_MODE],
        _ => return String::new(),
    };
    let Some(pixels) = first_frame else {
        return String::new();
    };
    let width = probe.width as usize;
    let height = probe.height as usize;
    if width == 0 || height == 0 || width > MAX_SIDE as usize || height > MAX_SIDE as usize {
        return String::new();
    }
    let bands = match probe.color_type {
        2 => 3usize,
        4 => 2,
        6 => 4,
        _ => return String::new(),
    };
    if width.saturating_mul(height).saturating_mul(bands) > png_raw::MAX_PNG_BYTES {
        return String::new();
    }
    let sampler = PixelSampler { pixels };
    run_modes(&sampler, &modes, width, height, interrupt)
}

fn stealth_text_16(path: &Path, _probe: &Probe, interrupt: &Interrupt) -> String {
    let Some(header) = png_raw::png_16bit_pixels(path, true, interrupt) else {
        return String::new();
    };
    let modes: Vec<&Mode> = match header.channels {
        2 => vec![&ALPHA_MODE],
        4 => vec![&ALPHA_MODE, &RGB_MODE],
        _ => vec![&RGB_MODE],
    };
    let width = header.width;
    let height = header.height;
    let mut full: Option<Raw16> = None;
    for (index, mode) in modes.iter().enumerate() {
        let slots = mode.slots.len();
        let capacity = width.saturating_mul(height).saturating_mul(slots);
        if capacity < 152 {
            continue;
        }
        let header_sampler = RawSampler { raw: &header };
        let Some(head) = read_bits(&header_sampler, slots, width, height, 0, 152, interrupt) else {
            continue;
        };
        let mut expected = mode.prefix.to_vec();
        expected.extend_from_slice(b"info");
        let mut compressed_expected = mode.prefix.to_vec();
        compressed_expected.extend_from_slice(b"comp");
        let head_prefix = &head[..15];
        let compressed = if head_prefix == compressed_expected.as_slice() {
            true
        } else if head_prefix == expected.as_slice() {
            false
        } else {
            continue;
        };
        let length = u32::from_be_bytes([head[15], head[16], head[17], head[18]]) as usize;
        if length % 8 != 0 || length > MAX_STEALTH_BYTES * 8 || 152 + length > capacity {
            continue;
        }
        // A validated header triggers the full decode; a failure here ends the
        // scan without trying the next mode (unlike the 8-bit path).
        if full.is_none() {
            let _ = index;
            match png_raw::png_16bit_pixels(path, false, interrupt) {
                Some(raw) => full = Some(raw),
                None => return String::new(),
            }
        }
        let payload = if length == 0 {
            Vec::new()
        } else {
            let raw = match &full {
                Some(raw) => raw,
                None => return String::new(),
            };
            let sampler = RawSampler { raw };
            match read_bits(&sampler, slots, width, height, 152, length, interrupt) {
                Some(bytes) => bytes,
                None => continue,
            }
        };
        let payload = if compressed {
            match decompress(&payload) {
                Some(bytes) => bytes,
                None => continue,
            }
        } else {
            payload
        };
        if let Ok(text) = String::from_utf8(payload) {
            return text;
        }
    }
    String::new()
}

fn run_modes(
    sampler: &dyn Sampler,
    modes: &[&Mode],
    width: usize,
    height: usize,
    interrupt: &Interrupt,
) -> String {
    for mode in modes {
        let slots = mode.slots.len();
        let capacity = width.saturating_mul(height).saturating_mul(slots);
        if capacity < 152 {
            continue;
        }
        let Some(head) = read_bits(sampler, slots, width, height, 0, 152, interrupt) else {
            continue;
        };
        let mut info_prefix = mode.prefix.to_vec();
        info_prefix.extend_from_slice(b"info");
        let mut comp_prefix = mode.prefix.to_vec();
        comp_prefix.extend_from_slice(b"comp");
        let head_prefix = &head[..15];
        let compressed = if head_prefix == comp_prefix.as_slice() {
            true
        } else if head_prefix == info_prefix.as_slice() {
            false
        } else {
            continue;
        };
        let length = u32::from_be_bytes([head[15], head[16], head[17], head[18]]) as usize;
        if length % 8 != 0 || length > MAX_STEALTH_BYTES * 8 || 152 + length > capacity {
            continue;
        }
        let payload = if length == 0 {
            Vec::new()
        } else {
            match read_bits(sampler, slots, width, height, 152, length, interrupt) {
                Some(bytes) => bytes,
                None => continue,
            }
        };
        let payload = if compressed {
            match decompress(&payload) {
                Some(bytes) => bytes,
                None => continue,
            }
        } else {
            payload
        };
        if let Ok(text) = String::from_utf8(payload) {
            return text;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::probe;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn extract(name: &str) -> String {
        let path = fixture(name);
        let probe = probe(&path).unwrap();
        let interrupt = Interrupt::new();
        let decoded = image::open(&path).unwrap().to_rgba8();
        let pixels = Pixels::Rgba(decoded);
        stealth_text(&probe, &path, Some(&pixels), &interrupt)
    }

    #[test]
    fn all_variants_are_extracted() {
        for depth in [8, 16] {
            for color in [2, 4, 6] {
                for mode in ["rgb", "alpha"] {
                    // LA (4) has only an alpha payload; RGB has no alpha plane.
                    if (color == 4 && mode == "rgb") || (color == 2 && mode == "alpha") {
                        continue;
                    }
                    if depth == 16 && color == 2 && mode == "rgb" {
                        // covered below
                    }
                    for compression in ["info", "gzip", "zlib"] {
                        for filter_type in 0..5 {
                            let name = format!(
                                "stealth-{depth}-{color}-{mode}-{compression}-{filter_type}.png"
                            );
                            let expected = format!(
                                "生成情報 {depth}-{color}-{mode}-{compression}-{filter_type}"
                            );
                            assert_eq!(extract(&name), expected, "{name}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_payloads_return_empty() {
        assert_eq!(extract("stealth-bad-length.png"), "");
        assert_eq!(extract("stealth-bad-utf8.png"), "");
        assert_eq!(extract("plain16-gray2.png"), "");
    }
}
