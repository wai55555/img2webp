//! Stage-1 header scanning.
//!
//! Only chunk/segment headers and the small metadata blocks are read; pixel
//! data is skipped with seeks (PNG/JPEG) or sub-block reads (GIF). The result
//! carries everything the dispatcher needs before the budget is granted
//! (design spec §3.4) and the source metadata used by the conversion
//! (design spec §3.5).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Unknown,
    Png,
    Jpeg,
    Gif,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Png => "PNG",
            Format::Jpeg => "JPEG",
            Format::Gif => "GIF",
            Format::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    /// PNG bit depth or JPEG sample precision. GIF is always 8.
    pub bit_depth: u16,
    /// PNG color type (2/3/4/6) or JPEG component count.
    pub color_type: u8,
    pub components: u8,
    /// Raw frame count: PNG `acTL.num_frames`, GIF image descriptors, JPEG 1.
    pub frames: u32,
    pub is_apng: bool,
    pub apng_default_image: bool,
    pub durations_ms: Vec<f64>,
    pub loop_count: Option<i64>,
    /// PNG text chunks in chunk order with duplicates resolved like Pillow's
    /// info dict (last value wins, first position kept).
    pub text: Vec<(String, String)>,
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    pub comment: Option<Vec<u8>>,
    pub transparency: bool,
    /// GIF global colour table and background index (Pillow exposes the index
    /// as `info["background"]` and the WebP writer turns it into an RGBA word).
    pub global_palette: Vec<[u8; 3]>,
    pub background_index: u8,
    pub mpo: bool,
    /// True when the header walk finished without hitting a read error.
    pub complete: bool,
}

impl Probe {
    pub fn channels(&self) -> u8 {
        match self.format {
            Format::Png => match self.color_type {
                0 => 1,
                2 => 3,
                3 => 1,
                4 => 2,
                6 => 4,
                _ => 0,
            },
            _ => self.components,
        }
    }

    /// The values compared between stage 1 and stage 2 (design spec §3.8).
    pub fn signature_matches(&self, other: &Probe) -> bool {
        self.format == other.format
            && self.width == other.width
            && self.height == other.height
            && self.bit_depth == other.bit_depth
            && self.color_type == other.color_type
            && self.frames == other.frames
    }
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| *b as char).collect()
}

fn utf8_lossy_text(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec()).ok()
}

fn record_text(entries: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some(existing) = entries.iter_mut().find(|(name, _)| *name == key) {
        existing.1 = value;
    } else {
        entries.push((key, value));
    }
}

fn inflate(bytes: &[u8]) -> Option<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    let mut out = Vec::new();
    let mut decoder = ZlibDecoder::new(bytes);
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

pub fn probe(path: &Path) -> std::io::Result<Probe> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut magic = [0u8; 8];
    let read = reader.read(&mut magic)?;
    reader.seek(SeekFrom::Start(0))?;
    if read >= 8 && magic == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return probe_png(&mut reader);
    }
    if read >= 2 && magic[0] == 0xff && magic[1] == 0xd8 {
        return probe_jpeg(&mut reader);
    }
    if read >= 6 && (&magic[..6] == b"GIF87a" || &magic[..6] == b"GIF89a") {
        return probe_gif(&mut reader);
    }
    Ok(Probe {
        format: Format::Unknown,
        complete: true,
        ..Default::default()
    })
}

fn probe_png<R: Read + Seek>(reader: &mut R) -> std::io::Result<Probe> {
    let mut probe = Probe {
        format: Format::Png,
        frames: 1,
        ..Default::default()
    };
    let mut signature = [0u8; 8];
    reader.read_exact(&mut signature)?;
    let mut seen_idat = false;
    let mut first_fctl_before_idat = false;
    let mut seen_fctl = false;
    let mut icc_parts: Vec<u8> = Vec::new();
    loop {
        let mut header = [0u8; 8];
        if reader.read_exact(&mut header).is_err() {
            break;
        }
        let length = be_u32(&header[..4]) as u64;
        let kind: [u8; 4] = [header[4], header[5], header[6], header[7]];
        match &kind[..] {
            b"IHDR" => {
                let mut payload = vec![0u8; length as usize];
                reader.read_exact(&mut payload)?;
                reader.seek(SeekFrom::Current(4))?;
                if payload.len() >= 13 {
                    probe.width = be_u32(&payload[0..4]);
                    probe.height = be_u32(&payload[4..8]);
                    probe.bit_depth = payload[8] as u16;
                    probe.color_type = payload[9];
                    probe.components = probe.channels();
                }
            }
            b"acTL" => {
                let mut payload = vec![0u8; length.min(8) as usize];
                reader.read_exact(&mut payload)?;
                if length > payload.len() as u64 {
                    reader.seek(SeekFrom::Current((length - payload.len() as u64) as i64))?;
                }
                reader.seek(SeekFrom::Current(4))?;
                if payload.len() >= 8 {
                    let frames = be_u32(&payload[0..4]);
                    probe.is_apng = frames > 0;
                    if frames > 0 {
                        probe.frames = frames;
                    }
                    probe.loop_count = Some(be_u32(&payload[4..8]) as i64);
                }
            }
            b"fcTL" => {
                let mut payload = vec![0u8; length.min(26) as usize];
                reader.read_exact(&mut payload)?;
                if length > payload.len() as u64 {
                    reader.seek(SeekFrom::Current((length - payload.len() as u64) as i64))?;
                }
                reader.seek(SeekFrom::Current(4))?;
                if !seen_fctl {
                    seen_fctl = true;
                    first_fctl_before_idat = !seen_idat;
                }
                if payload.len() >= 24 {
                    let delay_num = u16::from_be_bytes([payload[20], payload[21]]) as f64;
                    let mut delay_den = u16::from_be_bytes([payload[22], payload[23]]) as f64;
                    if delay_den == 0.0 {
                        delay_den = 100.0;
                    }
                    probe.durations_ms.push(delay_num / delay_den * 1000.0);
                }
            }
            b"IDAT" => {
                if !seen_idat {
                    seen_idat = true;
                    if probe.is_apng && !first_fctl_before_idat {
                        probe.apng_default_image = true;
                    }
                }
                reader.seek(SeekFrom::Current(length as i64 + 4))?;
            }
            b"IEND" => break,
            b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf" | b"iCCP" => {
                let mut payload = vec![0u8; length as usize];
                reader.read_exact(&mut payload)?;
                reader.seek(SeekFrom::Current(4))?;
                match &kind[..] {
                    b"tEXt" => {
                        let (key, value) = split_nul(&payload, 1);
                        if !key.is_empty() {
                            record_text(&mut probe.text, latin1(key), latin1(value));
                        }
                    }
                    b"zTXt" => {
                        let Some(sep) = payload.iter().position(|b| *b == 0) else {
                            continue;
                        };
                        if sep + 2 <= payload.len() {
                            let key = &payload[..sep];
                            if !key.is_empty() {
                                if let Some(plain) = inflate(&payload[sep + 2..]) {
                                    record_text(&mut probe.text, latin1(key), latin1(&plain));
                                }
                            }
                        }
                    }
                    b"iTXt" => {
                        if let Some((key, value)) = parse_itxt(&payload) {
                            record_text(&mut probe.text, key, value);
                        }
                    }
                    b"eXIf" => {
                        let tiff = strip_exif_prefix(&payload);
                        if probe.exif.is_none() && !tiff.is_empty() {
                            probe.exif = Some(tiff);
                        }
                    }
                    b"iCCP" => {
                        let Some(sep) = payload.iter().position(|b| *b == 0) else {
                            continue;
                        };
                        if sep + 2 <= payload.len() {
                            if let Some(profile) = inflate(&payload[sep + 2..]) {
                                icc_parts = profile;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                reader.seek(SeekFrom::Current(length as i64 + 4))?;
            }
        }
        if seen_idat && &kind[..] == b"IEND" {
            break;
        }
    }
    if !icc_parts.is_empty() {
        probe.icc = Some(icc_parts);
    }
    // Pillow exposes the XMP packet through `info["xmp"]` and keeps the text
    // chunk itself in the info dict.
    if let Some((_, value)) = probe
        .text
        .iter()
        .find(|(key, _)| key == "XML:com.adobe.xmp")
    {
        probe.xmp = Some(value.as_bytes().to_vec());
    }
    probe.complete = probe.width > 0 && probe.height > 0;
    Ok(probe)
}

fn split_nul(payload: &[u8], limit: usize) -> (&[u8], &[u8]) {
    match payload.iter().position(|b| *b == 0) {
        Some(index) => (&payload[..index], &payload[index + 1..]),
        None if limit == 1 => (payload, &[]),
        None => (payload, &[]),
    }
}

fn parse_itxt(payload: &[u8]) -> Option<(String, String)> {
    let key_end = payload.iter().position(|b| *b == 0)?;
    let keyword = &payload[..key_end];
    let rest = &payload[key_end + 1..];
    if rest.len() < 2 {
        return None;
    }
    let compressed = rest[0] != 0;
    let method = rest[1];
    let rest = &rest[2..];
    let lang_end = rest.iter().position(|b| *b == 0)?;
    let rest = &rest[lang_end + 1..];
    let translated_end = rest.iter().position(|b| *b == 0)?;
    let text = &rest[translated_end + 1..];
    let text = if compressed {
        if method != 0 {
            return None;
        }
        inflate(text)?
    } else {
        text.to_vec()
    };
    let keyword = latin1(keyword);
    let value = utf8_lossy_text(&text)?;
    Some((keyword, value))
}

pub(crate) fn strip_exif_prefix(data: &[u8]) -> Vec<u8> {
    if data.len() >= 6 && &data[..6] == b"Exif\0\0" {
        data[6..].to_vec()
    } else {
        data.to_vec()
    }
}

fn probe_jpeg<R: Read + Seek>(reader: &mut R) -> std::io::Result<Probe> {
    let mut probe = Probe {
        format: Format::Jpeg,
        frames: 1,
        bit_depth: 8,
        ..Default::default()
    };
    let mut icc_parts: Vec<u8> = Vec::new();
    let mut buf = [0u8; 2];
    while reader.read_exact(&mut buf).is_ok() {
        if buf[0] != 0xff {
            // Resynchronise: read until the next marker prefix.
            reader.seek(SeekFrom::Current(-1))?;
            continue;
        }
        let marker = buf[1];
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if marker == 0xd9 || marker == 0xda {
            // End of image or start of scan: the header scan is finished.
            break;
        }
        let mut length_bytes = [0u8; 2];
        if reader.read_exact(&mut length_bytes).is_err() {
            break;
        }
        let length = u16::from_be_bytes(length_bytes) as u64;
        if length < 2 {
            break;
        }
        let payload_len = length - 2;
        let is_sof =
            (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc;
        if is_sof {
            let mut payload = vec![0u8; payload_len as usize];
            reader.read_exact(&mut payload)?;
            if payload.len() >= 6 {
                probe.bit_depth = payload[0] as u16;
                probe.height = u16::from_be_bytes([payload[1], payload[2]]) as u32;
                probe.width = u16::from_be_bytes([payload[3], payload[4]]) as u32;
                probe.components = payload[5];
                probe.color_type = payload[5];
            }
            continue;
        }
        if payload_len > 8 * 1024 * 1024 {
            reader.seek(SeekFrom::Current(payload_len as i64))?;
            continue;
        }
        let mut payload = vec![0u8; payload_len as usize];
        reader.read_exact(&mut payload)?;
        match marker {
            0xe1 => {
                if payload.starts_with(b"Exif\0\0") {
                    if probe.exif.is_none() {
                        probe.exif = Some(strip_exif_prefix(&payload));
                    }
                } else if payload.starts_with(b"http://ns.adobe.com/xap/1.0/\0") {
                    probe.xmp = Some(payload[b"http://ns.adobe.com/xap/1.0/\0".len()..].to_vec());
                }
            }
            0xe2 => {
                if payload.starts_with(b"ICC_PROFILE\0") && payload.len() >= 14 {
                    icc_parts.extend_from_slice(&payload[14..]);
                } else if payload.starts_with(b"MPF\0") {
                    probe.mpo = true;
                }
            }
            0xee => {
                probe.mpo = false;
            }
            0xfe => {
                probe.comment = Some(payload);
            }
            _ => {}
        }
    }
    if !icc_parts.is_empty() {
        probe.icc = Some(icc_parts);
    }
    probe.complete = probe.width > 0 && probe.height > 0;
    Ok(probe)
}

fn read_sub_blocks<R: Read>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut size = [0u8; 1];
        reader.read_exact(&mut size)?;
        if size[0] == 0 {
            break;
        }
        let mut chunk = vec![0u8; size[0] as usize];
        reader.read_exact(&mut chunk)?;
        out.extend_from_slice(&chunk);
        if out.len() > 32 * 1024 * 1024 {
            break;
        }
    }
    Ok(out)
}

fn skip_sub_blocks<R: Read + Seek>(reader: &mut R) -> std::io::Result<()> {
    loop {
        let mut size = [0u8; 1];
        reader.read_exact(&mut size)?;
        if size[0] == 0 {
            return Ok(());
        }
        reader.seek(SeekFrom::Current(size[0] as i64))?;
    }
}

fn probe_gif<R: Read + Seek>(reader: &mut R) -> std::io::Result<Probe> {
    let mut probe = Probe {
        format: Format::Gif,
        bit_depth: 8,
        components: 4,
        ..Default::default()
    };
    let mut header = [0u8; 13];
    reader.read_exact(&mut header)?;
    probe.width = u16::from_le_bytes([header[6], header[7]]) as u32;
    probe.height = u16::from_le_bytes([header[8], header[9]]) as u32;
    let packed = header[10];
    probe.background_index = header[11];
    if packed & 0x80 != 0 {
        let entries = 1usize << ((packed & 0x07) + 1);
        let mut table = vec![0u8; entries * 3];
        if reader.read_exact(&mut table).is_ok() {
            probe.global_palette = table
                .chunks_exact(3)
                .map(|entry| [entry[0], entry[1], entry[2]])
                .collect();
        }
    }
    let mut frames = 0u32;
    loop {
        let mut introducer = [0u8; 1];
        if reader.read_exact(&mut introducer).is_err() {
            break;
        }
        match introducer[0] {
            0x3b => break,
            0x2c => {
                frames += 1;
                let mut descriptor = [0u8; 9];
                reader.read_exact(&mut descriptor)?;
                let packed = descriptor[8];
                if packed & 0x80 != 0 {
                    let table_size = 3u64 << ((packed & 0x07) + 1);
                    reader.seek(SeekFrom::Current(table_size as i64))?;
                }
                let mut lzw_min = [0u8; 1];
                reader.read_exact(&mut lzw_min)?;
                skip_sub_blocks(reader)?;
            }
            0x21 => {
                let mut label = [0u8; 1];
                reader.read_exact(&mut label)?;
                match label[0] {
                    0xf9 => {
                        let mut size = [0u8; 1];
                        reader.read_exact(&mut size)?;
                        let mut block = vec![0u8; size[0] as usize];
                        reader.read_exact(&mut block)?;
                        // Skip the block terminator.
                        let mut terminator = [0u8; 1];
                        let _ = reader.read_exact(&mut terminator);
                        if block.len() >= 4 {
                            let flags = block[0];
                            if flags & 0x01 != 0 {
                                probe.transparency = true;
                            }
                            let delay = u16::from_le_bytes([block[1], block[2]]);
                            probe.durations_ms.push(delay as f64 * 10.0);
                        }
                    }
                    0xfe => {
                        let data = read_sub_blocks(reader)?;
                        match &mut probe.comment {
                            Some(existing) => existing.extend_from_slice(&data),
                            None => probe.comment = Some(data),
                        }
                    }
                    0xff => {
                        let mut size = [0u8; 1];
                        reader.read_exact(&mut size)?;
                        let mut identifier = vec![0u8; size[0] as usize];
                        reader.read_exact(&mut identifier)?;
                        let data = read_sub_blocks(reader)?;
                        if (identifier == b"NETSCAPE2.0" || identifier == b"ANIMEXTS1.0")
                            && data.len() >= 3
                            && data[0] == 1
                        {
                            probe.loop_count = Some(u16::from_le_bytes([data[1], data[2]]) as i64);
                        }
                    }
                    _ => {
                        let mut size = [0u8; 1];
                        reader.read_exact(&mut size)?;
                        reader.seek(SeekFrom::Current(size[0] as i64))?;
                        skip_sub_blocks(reader)?;
                    }
                }
            }
            _ => break,
        }
    }
    probe.frames = frames.max(1);
    probe.complete = probe.width > 0 && probe.height > 0;
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn png_text_and_geometry() {
        let probe = probe(&fixture("text.png")).unwrap();
        assert_eq!(probe.format, Format::Png);
        assert_eq!((probe.width, probe.height), (4, 3));
        assert_eq!(probe.color_type, 6);
        assert_eq!(probe.components, 4);
        let keys: Vec<&str> = probe.text.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["parameters", "Description", "XML:com.adobe.xmp"]);
        assert_eq!(probe.text[0].1, "a cat\nSteps: 20");
        assert_eq!(probe.text[1].1, "NovelAI text");
        let exif = probe.exif.as_ref().unwrap();
        assert!(exif.starts_with(b"II") || exif.starts_with(b"MM"));
        assert!(probe.icc.as_ref().is_some_and(|icc| icc.len() > 100));
        assert!(probe.xmp.is_some());
    }

    #[test]
    fn gif_frames_loop_and_comment() {
        let probe = probe(&fixture("anim.gif")).unwrap();
        assert_eq!(probe.format, Format::Gif);
        assert_eq!((probe.width, probe.height), (6, 6));
        assert_eq!(probe.frames, 2);
        assert_eq!(probe.loop_count, Some(2));
        assert_eq!(probe.comment.as_deref(), Some(&b"gif prompt"[..]));
        assert_eq!(probe.durations_ms, vec![70.0, 130.0]);
    }

    #[test]
    fn jpeg_geometry_and_exif() {
        let probe = probe(&fixture("rotated.jpg")).unwrap();
        assert_eq!(probe.format, Format::Jpeg);
        assert_eq!((probe.width, probe.height), (2, 3));
        assert_eq!(probe.bit_depth, 8);
        assert_eq!(probe.components, 3);
        assert!(probe.exif.is_some());
    }

    #[test]
    fn apng_default_image_detection() {
        let standard = probe(&fixture("apng-standard.png")).unwrap();
        assert!(standard.is_apng);
        assert!(!standard.apng_default_image);
        assert_eq!(standard.frames, 3);
        assert_eq!(standard.durations_ms, vec![70.0, 130.0, 50.0]);
        assert_eq!(standard.loop_count, Some(2));

        let separate = probe(&fixture("apng-separate.png")).unwrap();
        assert!(separate.is_apng);
        assert!(separate.apng_default_image);
        assert_eq!(separate.frames, 2);
        assert_eq!(separate.durations_ms, vec![70.0, 130.0]);
    }

    #[test]
    fn mpo_is_detected() {
        let probe = probe(&fixture("mpo.jpg")).unwrap();
        assert!(probe.mpo);
    }

    #[test]
    fn unknown_format_is_reported() {
        let probe = probe(&fixture("broken.png")).unwrap();
        assert_eq!(probe.format, Format::Unknown);
    }
}
