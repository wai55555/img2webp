//! Minimal TIFF/Exif reader and writer.
//!
//! The Python version keeps Exif through `piexif`'s dict conversion. This port
//! keeps every IFD entry's raw value bytes so unknown tags survive a
//! read/write round trip (design spec §3.5, decision D-EXIF). Only the tags the
//! pipeline itself edits (Orientation, UserComment, XPComment) are interpreted.

pub const TAG_ORIENTATION: u16 = 0x0112;
pub const TAG_EXIF_IFD: u16 = 0x8769;
pub const TAG_GPS_IFD: u16 = 0x8825;
pub const TAG_INTEROP_IFD: u16 = 0xa005;
pub const TAG_USER_COMMENT: u16 = 0x9286;
pub const TAG_XP_COMMENT: u16 = 0x9c9c;
pub const TAG_THUMB_OFFSET: u16 = 0x0201;
pub const TAG_THUMB_LENGTH: u16 = 0x0202;

pub const TYPE_BYTE: u16 = 1;
pub const TYPE_ASCII: u16 = 2;
pub const TYPE_SHORT: u16 = 3;
pub const TYPE_LONG: u16 = 4;
pub const TYPE_RATIONAL: u16 = 5;
pub const TYPE_UNDEFINED: u16 = 7;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Entry {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    pub value: Vec<u8>,
}

impl Entry {
    pub fn byte(tag: u16, value: &[u8]) -> Self {
        Entry {
            tag,
            typ: TYPE_BYTE,
            count: value.len() as u32,
            value: value.to_vec(),
        }
    }

    pub fn undefined(tag: u16, value: &[u8]) -> Self {
        Entry {
            tag,
            typ: TYPE_UNDEFINED,
            count: value.len() as u32,
            value: value.to_vec(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Ifd {
    pub entries: Vec<Entry>,
}

impl Ifd {
    pub fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.tag == tag)
    }

    pub fn contains(&self, tag: u16) -> bool {
        self.get(tag).is_some()
    }

    pub fn set(&mut self, entry: Entry) {
        match self.entries.iter_mut().find(|item| item.tag == entry.tag) {
            Some(existing) => *existing = entry,
            None => {
                self.entries.push(entry);
                self.entries.sort_by_key(|item| item.tag);
            }
        }
    }

    pub fn remove(&mut self, tag: u16) {
        self.entries.retain(|entry| entry.tag != tag);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exif {
    pub ifd0: Ifd,
    pub exif: Ifd,
    pub gps: Ifd,
    pub interop: Ifd,
    pub ifd1: Ifd,
    pub thumbnail: Option<Vec<u8>>,
    pub little_endian: bool,
}

impl Default for Exif {
    fn default() -> Self {
        Exif {
            ifd0: Ifd::default(),
            exif: Ifd::default(),
            gps: Ifd::default(),
            interop: Ifd::default(),
            ifd1: Ifd::default(),
            thumbnail: None,
            little_endian: true,
        }
    }
}

fn type_size(typ: u16) -> Option<u32> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

struct Reader<'a> {
    data: &'a [u8],
    little: bool,
}

impl Reader<'_> {
    fn u16(&self, offset: usize) -> Option<u16> {
        let bytes = self.data.get(offset..offset + 2)?;
        Some(if self.little {
            u16::from_le_bytes([bytes[0], bytes[1]])
        } else {
            u16::from_be_bytes([bytes[0], bytes[1]])
        })
    }

    fn u32(&self, offset: usize) -> Option<u32> {
        let bytes = self.data.get(offset..offset + 4)?;
        Some(if self.little {
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
    }
}

impl Exif {
    #[allow(clippy::type_complexity)]
    pub fn parse(raw: &[u8]) -> Result<Exif, String> {
        let data = if raw.len() >= 6 && &raw[..6] == b"Exif\0\0" {
            &raw[6..]
        } else {
            raw
        };
        if data.len() < 8 {
            return Err("Exif データが短すぎます".to_string());
        }
        let little = match &data[..2] {
            b"II" => true,
            b"MM" => false,
            _ => return Err("Exif のバイトオーダーが不正です".to_string()),
        };
        let reader = Reader { data, little };
        if reader.u16(2) != Some(42) {
            return Err("Exif のヘッダーが不正です".to_string());
        }
        let mut exif = Exif {
            little_endian: little,
            ..Default::default()
        };
        let ifd0_offset = reader
            .u32(4)
            .ok_or_else(|| "Exif の IFD オフセットが不正です".to_string())?
            as usize;
        if ifd0_offset == 0 {
            return Ok(exif);
        }
        let (ifd0, next) = parse_ifd(&reader, ifd0_offset)?;
        exif.ifd0 = ifd0;
        if let Some(entry) = exif.ifd0.get(TAG_EXIF_IFD).cloned() {
            let offset = inline_u32(&reader, &entry).ok_or("Exif IFD のオフセットが不正です")?;
            if offset > 0 {
                exif.exif = parse_ifd(&reader, offset as usize)?.0;
            }
        }
        if let Some(entry) = exif.ifd0.get(TAG_GPS_IFD).cloned() {
            let offset = inline_u32(&reader, &entry).ok_or("GPS IFD のオフセットが不正です")?;
            if offset > 0 {
                exif.gps = parse_ifd(&reader, offset as usize)?.0;
            }
        }
        if let Some(entry) = exif.exif.get(TAG_INTEROP_IFD).cloned() {
            let offset = inline_u32(&reader, &entry).ok_or("Interop IFD のオフセットが不正です")?;
            if offset > 0 {
                exif.interop = parse_ifd(&reader, offset as usize)?.0;
            }
        }
        if next > 0 {
            let (ifd1, _) = parse_ifd(&reader, next as usize)?;
            exif.ifd1 = ifd1;
            let offset = exif
                .ifd1
                .get(TAG_THUMB_OFFSET)
                .and_then(|entry| inline_u32(&reader, entry));
            let length = exif
                .ifd1
                .get(TAG_THUMB_LENGTH)
                .and_then(|entry| inline_u32(&reader, entry));
            if let (Some(offset), Some(length)) = (offset, length) {
                let start = offset as usize;
                let end = start.saturating_add(length as usize);
                if let Some(bytes) = data.get(start..end) {
                    exif.thumbnail = Some(bytes.to_vec());
                }
            }
        }
        exif.ifd0.remove(TAG_EXIF_IFD);
        exif.ifd0.remove(TAG_GPS_IFD);
        exif.exif.remove(TAG_INTEROP_IFD);
        exif.ifd0.entries.sort_by_key(|entry| entry.tag);
        exif.exif.entries.sort_by_key(|entry| entry.tag);
        exif.gps.entries.sort_by_key(|entry| entry.tag);
        exif.interop.entries.sort_by_key(|entry| entry.tag);
        exif.ifd1.entries.sort_by_key(|entry| entry.tag);
        Ok(exif)
    }

    pub fn has_data(&self) -> bool {
        !self.ifd0.is_empty()
            || !self.exif.is_empty()
            || !self.gps.is_empty()
            || !self.interop.is_empty()
            || !self.ifd1.is_empty()
            || self.thumbnail.is_some()
    }

    pub fn orientation(&self) -> Option<u16> {
        self.ifd0
            .get(TAG_ORIENTATION)
            .and_then(|entry| inline_u16(entry, self.little_endian))
    }

    /// Serializes the IFDs into a TIFF byte stream (no `Exif\0\0` prefix).
    pub fn serialize(&self) -> Vec<u8> {
        let little = self.little_endian;
        let mut ifd0 = self.ifd0.clone();
        let mut exif_ifd = self.exif.clone();
        let mut ifd1 = self.ifd1.clone();
        ifd0.remove(TAG_EXIF_IFD);
        ifd0.remove(TAG_GPS_IFD);
        exif_ifd.remove(TAG_INTEROP_IFD);
        ifd1.remove(TAG_THUMB_OFFSET);
        ifd1.remove(TAG_THUMB_LENGTH);

        let has_exif_ifd = !exif_ifd.is_empty();
        let has_gps = !self.gps.is_empty();
        let has_interop = !self.interop.is_empty();
        let thumbnail = self.thumbnail.clone();
        let has_ifd1 = !ifd1.is_empty() || thumbnail.is_some();

        let placeholder_u32 = |tag: u16| Entry {
            tag,
            typ: TYPE_LONG,
            count: 1,
            value: vec![0; 4],
        };
        if has_exif_ifd {
            ifd0.set(placeholder_u32(TAG_EXIF_IFD));
        }
        if has_gps {
            ifd0.set(placeholder_u32(TAG_GPS_IFD));
        }
        if has_interop {
            exif_ifd.set(placeholder_u32(TAG_INTEROP_IFD));
        }
        if thumbnail.is_some() {
            ifd1.set(placeholder_u32(TAG_THUMB_OFFSET));
            ifd1.set(Entry {
                tag: TAG_THUMB_LENGTH,
                typ: TYPE_LONG,
                count: 1,
                value: (thumbnail.as_ref().map(|t| t.len()).unwrap_or(0) as u32)
                    .to_le_bytes()
                    .to_vec(),
            });
        }
        for ifd in [&mut ifd0, &mut ifd1, &mut exif_ifd] {
            ifd.entries.sort_by_key(|entry| entry.tag);
        }

        let block_size = |count: usize| -> u32 { 2 + 12 * count as u32 + 4 };
        let mut offset: u32 = 8;
        let ifd0_offset = offset;
        offset += block_size(ifd0.entries.len());
        let ifd1_offset = if has_ifd1 {
            let current = offset;
            offset += block_size(ifd1.entries.len());
            current
        } else {
            0
        };
        let exif_offset = if has_exif_ifd {
            let current = offset;
            offset += block_size(exif_ifd.entries.len());
            current
        } else {
            0
        };
        let gps_offset = if has_gps {
            let current = offset;
            offset += block_size(self.gps.entries.len());
            current
        } else {
            0
        };
        let interop_offset = if has_interop {
            let current = offset;
            offset += block_size(self.interop.entries.len());
            current
        } else {
            0
        };
        let gps_ifd = self.gps.clone();
        let interop_ifd = self.interop.clone();
        let ifds: [&Ifd; 5] = [&ifd0, &ifd1, &exif_ifd, &gps_ifd, &interop_ifd];
        let block_offsets = [
            ifd0_offset,
            ifd1_offset,
            exif_offset,
            gps_offset,
            interop_offset,
        ];

        // Out-of-line values are laid out after all IFD blocks, in IFD order.
        let base_of_data = offset;
        let mut value_offsets: Vec<Vec<Option<u32>>> = ifds
            .iter()
            .map(|ifd| vec![None; ifd.entries.len()])
            .collect();
        let mut cursor = 0u32;
        for (ifd_index, ifd) in ifds.iter().enumerate() {
            for (entry_index, entry) in ifd.entries.iter().enumerate() {
                if entry.value.len() > 4 {
                    value_offsets[ifd_index][entry_index] = Some(base_of_data + cursor);
                    cursor += entry.value.len() as u32;
                    if cursor % 2 == 1 {
                        cursor += 1;
                    }
                }
            }
        }
        let mut data_area = vec![0u8; cursor as usize];

        let thumbnail_offset = base_of_data + cursor;
        let thumbnail_length = thumbnail.as_ref().map(|t| t.len() as u32).unwrap_or(0);
        let mut thumb_patch_position: Option<usize> = None;

        let mut out: Vec<u8> = Vec::new();
        if little {
            out.extend_from_slice(b"II\x2a\x00");
        } else {
            out.extend_from_slice(b"MM\x00\x2a");
        }
        write_u32(&mut out, ifd0_offset, little);

        for (ifd_index, ifd) in ifds.iter().enumerate() {
            if block_offsets[ifd_index] == 0 {
                continue;
            }
            assert_eq!(out.len(), block_offsets[ifd_index] as usize);
            write_u16(&mut out, ifd.entries.len() as u16, little);
            for (entry_index, entry) in ifd.entries.iter().enumerate() {
                write_u16(&mut out, entry.tag, little);
                write_u16(&mut out, entry.typ, little);
                write_u32(&mut out, entry.count, little);
                let pointer_target = match entry.tag {
                    TAG_EXIF_IFD if ifd_index == 0 => Some(exif_offset),
                    TAG_GPS_IFD if ifd_index == 0 => Some(gps_offset),
                    TAG_INTEROP_IFD if ifd_index == 2 => Some(interop_offset),
                    _ => None,
                };
                if let Some(target) = pointer_target {
                    write_u32(&mut out, target, little);
                    continue;
                }
                if entry.tag == TAG_THUMB_OFFSET && ifd_index == 1 && thumbnail.is_some() {
                    thumb_patch_position = Some(out.len());
                    write_u32(&mut out, 0, little);
                    continue;
                }
                if entry.value.len() <= 4 {
                    let mut bytes = entry.value.clone();
                    bytes.resize(4, 0);
                    out.extend_from_slice(&bytes);
                } else {
                    let value_offset =
                        value_offsets[ifd_index][entry_index].expect("out-of-line offset assigned");
                    write_u32(&mut out, value_offset, little);
                    let start = (value_offset - base_of_data) as usize;
                    data_area[start..start + entry.value.len()].copy_from_slice(&entry.value);
                }
            }
            // Next-IFD pointer: only IFD0 chains to IFD1.
            if ifd_index == 0 && has_ifd1 {
                write_u32(&mut out, ifd1_offset, little);
            } else {
                write_u32(&mut out, 0, little);
            }
        }
        out.extend_from_slice(&data_area);
        if let Some(thumbnail) = &thumbnail {
            out.extend_from_slice(thumbnail);
        }
        if let Some(position) = thumb_patch_position {
            let bytes = if little {
                thumbnail_offset.to_le_bytes()
            } else {
                thumbnail_offset.to_be_bytes()
            };
            out[position..position + 4].copy_from_slice(&bytes);
        }
        let _ = thumbnail_length;
        out
    }
}

fn parse_ifd(reader: &Reader, offset: usize) -> Result<(Ifd, u32), String> {
    let count = reader
        .u16(offset)
        .ok_or_else(|| "Exif の IFD が破損しています".to_string())? as usize;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let base = offset + 2 + index * 12;
        let tag = reader.u16(base).ok_or("Exif エントリが破損しています")?;
        let typ = reader
            .u16(base + 2)
            .ok_or("Exif エントリが破損しています")?;
        let count = reader
            .u32(base + 4)
            .ok_or("Exif エントリが破損しています")?;
        let size = type_size(typ)
            .ok_or_else(|| format!("不明な Exif 型です: {typ}"))?
            .checked_mul(count)
            .ok_or_else(|| "Exif の値サイズが不正です".to_string())?;
        let value = if size <= 4 {
            let bytes = reader
                .data
                .get(base + 8..base + 8 + size as usize)
                .ok_or("Exif エントリが破損しています")?;
            bytes.to_vec()
        } else {
            let value_offset = reader
                .u32(base + 8)
                .ok_or("Exif エントリが破損しています")? as usize;
            let bytes = reader
                .data
                .get(value_offset..value_offset + size as usize)
                .ok_or_else(|| "Exif の値が範囲外です".to_string())?;
            bytes.to_vec()
        };
        entries.push(Entry {
            tag,
            typ,
            count,
            value,
        });
    }
    let next = reader
        .u32(offset + 2 + count * 12)
        .ok_or("Exif の次 IFD ポインタが破損しています")?;
    Ok((Ifd { entries }, next))
}

fn inline_u32(reader: &Reader, entry: &Entry) -> Option<u32> {
    let bytes = entry.value.get(..4)?;
    Some(if reader.little {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    })
}

pub fn inline_u16(entry: &Entry, little: bool) -> Option<u16> {
    let bytes = entry.value.get(..2)?;
    Some(if little {
        u16::from_le_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_be_bytes([bytes[0], bytes[1]])
    })
}

fn write_u16(out: &mut Vec<u8>, value: u16, little: bool) {
    if little {
        out.extend_from_slice(&value.to_le_bytes());
    } else {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn write_u32(out: &mut Vec<u8>, value: u32, little: bool) {
    if little {
        out.extend_from_slice(&value.to_le_bytes());
    } else {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

/// UTF-16LE payload used for `XPComment`.
pub fn utf16le(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// UTF-16BE payload used for `UserComment` (piexif and the EXIF spec).
pub fn utf16be(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_unknown_tags() {
        let mut exif = Exif::default();
        exif.ifd0.set(Entry {
            tag: 0x010f,
            typ: TYPE_ASCII,
            count: 6,
            value: b"Canon\0".to_vec(),
        });
        exif.ifd0.set(Entry {
            tag: 0x0112,
            typ: TYPE_SHORT,
            count: 1,
            value: vec![6, 0],
        });
        exif.exif.set(Entry::undefined(
            TAG_USER_COMMENT,
            &[b"UNICODE\0".as_slice(), &utf16le("hello")].concat(),
        ));
        exif.ifd0.set(Entry::byte(
            TAG_XP_COMMENT,
            &[b"a\0b\0".as_slice()].concat(),
        ));
        exif.gps.set(Entry {
            tag: 0x0001,
            typ: TYPE_ASCII,
            count: 2,
            value: b"N\0".to_vec(),
        });
        let bytes = exif.serialize();
        let parsed = Exif::parse(&bytes).unwrap();
        assert_eq!(parsed.ifd0.entries.len(), 3);
        assert_eq!(parsed.orientation(), Some(6));
        assert_eq!(parsed.gps.get(0x0001).unwrap().value, b"N\0".to_vec());
        assert!(parsed.exif.get(TAG_USER_COMMENT).is_some());
        assert!(parsed.ifd0.get(TAG_XP_COMMENT).is_some());
        // Round trip again is byte stable.
        assert_eq!(parsed.serialize(), bytes);
    }

    #[test]
    fn big_endian_round_trip() {
        let mut exif = Exif {
            little_endian: false,
            ..Default::default()
        };
        exif.ifd0.set(Entry {
            tag: 0x0112,
            typ: TYPE_SHORT,
            count: 1,
            value: vec![0, 3],
        });
        let bytes = exif.serialize();
        assert_eq!(&bytes[..4], b"MM\x00\x2a");
        let parsed = Exif::parse(&bytes).unwrap();
        assert_eq!(parsed.orientation(), Some(3));
    }

    #[test]
    fn thumbnail_is_preserved() {
        let mut exif = Exif::default();
        exif.ifd0.set(Entry {
            tag: 0x010e,
            typ: TYPE_ASCII,
            count: 4,
            value: b"abc\0".to_vec(),
        });
        exif.thumbnail = Some(vec![0xff, 0xd8, 0xff, 0xd9]);
        let bytes = exif.serialize();
        let parsed = Exif::parse(&bytes).unwrap();
        assert_eq!(
            parsed.thumbnail.as_deref(),
            Some(&[0xff, 0xd8, 0xff, 0xd9][..])
        );
    }
}
