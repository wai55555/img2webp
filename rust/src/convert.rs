//! Still-image and animation conversion pipeline (`convert_image`).

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::animation;
use crate::color::{self, Pixels, SourceMode};
use crate::config::Config;
use crate::interrupt::Interrupt;
use crate::metadata;
use crate::paths::path_suffix;
use crate::probe::{Format, Probe};
use crate::report::JobError;
use crate::stealth;
use crate::webp_encode::{self, EncodeSettings, OutputMetadata};

pub enum Decoded {
    Pixels(Pixels, SourceMode),
    Cmyk(Vec<u8>),
}

fn expected_format(source: &Path) -> Option<Format> {
    match path_suffix(source).to_lowercase().as_str() {
        ".png" => Some(Format::Png),
        ".jpg" | ".jpeg" => Some(Format::Jpeg),
        ".gif" => Some(Format::Gif),
        _ => None,
    }
}

fn open_reader(path: &Path) -> Result<BufReader<File>, JobError> {
    let file = File::open(path).map_err(|err| JobError::from_io(&err, path))?;
    Ok(BufReader::with_capacity(64 * 1024, file))
}

fn gray8_to_rgb(image: &image::GrayImage) -> Pixels {
    let mut out = image::RgbImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        out.put_pixel(x, y, image::Rgb([pixel[0], pixel[0], pixel[0]]));
    }
    Pixels::Rgb(out)
}

fn sixteen_to_rgb(image: &image::ImageBuffer<image::Rgb<u16>, Vec<u16>>) -> Pixels {
    let mut out = image::RgbImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        out.put_pixel(
            x,
            y,
            image::Rgb([
                (pixel[0] >> 8) as u8,
                (pixel[1] >> 8) as u8,
                (pixel[2] >> 8) as u8,
            ]),
        );
    }
    Pixels::Rgb(out)
}

fn sixteen_to_rgba(image: &image::ImageBuffer<image::Rgba<u16>, Vec<u16>>) -> Pixels {
    let mut out = image::RgbaImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        out.put_pixel(
            x,
            y,
            image::Rgba([
                (pixel[0] >> 8) as u8,
                (pixel[1] >> 8) as u8,
                (pixel[2] >> 8) as u8,
                (pixel[3] >> 8) as u8,
            ]),
        );
    }
    Pixels::Rgba(out)
}

fn sixteen_luma_to_rgb(image: &image::ImageBuffer<image::Luma<u16>, Vec<u16>>) -> Pixels {
    let mut out = image::RgbImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        // Pillow opens 16-bit greyscale as mode "I;16" and its RGB conversion
        // clips at 255 instead of taking the high byte (Phase 0 finding).
        let value = pixel[0].min(255) as u8;
        out.put_pixel(x, y, image::Rgb([value, value, value]));
    }
    Pixels::Rgb(out)
}

fn sixteen_luma_alpha_to_rgba(image: &image::ImageBuffer<image::LumaA<u16>, Vec<u16>>) -> Pixels {
    let mut out = image::RgbaImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        let value = (pixel[0] >> 8) as u8;
        out.put_pixel(
            x,
            y,
            image::Rgba([value, value, value, (pixel[1] >> 8) as u8]),
        );
    }
    Pixels::Rgba(out)
}

fn png_to_dynamic<R: std::io::BufRead + std::io::Seek>(
    reader: R,
    path: &Path,
) -> Result<image::DynamicImage, JobError> {
    let decoder = image::codecs::png::PngDecoder::new(reader)
        .map_err(|err| JobError::from_image(&err, path))?;
    image::DynamicImage::from_decoder(decoder).map_err(|err| JobError::from_image(&err, path))
}

/// Rebuilds a PNG with correct chunk CRCs and returns the damaged chunk names.
///
/// `png`-based decoding (unlike Pillow) rejects chunks whose CRC does not
/// match, so this is only used to keep parity with the Python version.
fn repair_png_crcs(path: &Path) -> Option<(Vec<u8>, Vec<String>)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let data = std::fs::read(path).ok()?;
    if data.len() < 8 || data[..8] != SIGNATURE {
        return None;
    }
    let mut out = data[..8].to_vec();
    let mut offset = 8usize;
    let mut damaged: Vec<String> = Vec::new();
    let mut saw_end = false;
    while offset + 12 <= data.len() {
        let length =
            u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
                as usize;
        let type_at = offset + 4;
        let data_at = offset + 8;
        let data_end = data_at.checked_add(length)?;
        let crc_at = data_end;
        let end = crc_at.checked_add(4)?;
        if end > data.len() {
            return None;
        }
        let kind = &data[type_at..data_at];
        let mut crc = flate2::Crc::new();
        crc.update(&data[type_at..data_end]);
        let computed = crc.sum();
        let stored = u32::from_be_bytes([
            data[crc_at],
            data[crc_at + 1],
            data[crc_at + 2],
            data[crc_at + 3],
        ]);
        out.extend_from_slice(&data[offset..data_end]);
        if stored == computed {
            out.extend_from_slice(&data[crc_at..end]);
        } else {
            damaged.push(String::from_utf8_lossy(kind).to_string());
            out.extend_from_slice(&computed.to_be_bytes());
        }
        offset = end;
        if kind == b"IEND" {
            saw_end = true;
            break;
        }
    }
    if !saw_end || damaged.is_empty() {
        return None;
    }
    Some((out, damaged))
}

fn luma_alpha_to_rgba(image: &image::ImageBuffer<image::LumaA<u8>, Vec<u8>>) -> Pixels {
    let mut out = image::RgbaImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        out.put_pixel(x, y, image::Rgba([pixel[0], pixel[0], pixel[0], pixel[1]]));
    }
    Pixels::Rgba(out)
}

/// Decodes the image as it is opened (frame 0, no orientation applied).
pub fn decode_still(
    path: &Path,
    probe: &Probe,
    interrupt: &Interrupt,
    warnings: &mut Vec<String>,
) -> Result<Decoded, JobError> {
    match probe.format {
        Format::Png => {
            let dynamic = match png_to_dynamic(open_reader(path)?, path) {
                Ok(dynamic) => dynamic,
                Err(error) => {
                    // Pillow does not verify chunk CRCs, so a file with a bad
                    // CRC still converts there. Recompute the CRCs, decode the
                    // repaired bytes and say so (intentional compatibility,
                    // design spec §5).
                    match repair_png_crcs(path) {
                        Some((bytes, damaged)) => {
                            warnings.push(format!(
                                "PNG のチャンク CRC が不正でした（{}）。Pillow と同じく無視して変換します",
                                damaged.join(", ")
                            ));
                            png_to_dynamic(std::io::Cursor::new(bytes), path)?
                        }
                        None => return Err(error),
                    }
                }
            };
            let mode = match probe.color_type {
                0 => SourceMode::L,
                2 => SourceMode::Rgb,
                3 => SourceMode::Palette {
                    transparency: probe.transparency,
                },
                4 => SourceMode::La,
                6 => SourceMode::Rgba,
                other => {
                    return Err(JobError::Conversion(format!(
                        "PNG の色タイプ {other} は未対応です"
                    )))
                }
            };
            let pixels = match dynamic {
                image::DynamicImage::ImageLuma8(image) => gray8_to_rgb(&image),
                image::DynamicImage::ImageLumaA8(image) => luma_alpha_to_rgba(&image),
                image::DynamicImage::ImageRgb8(image) => Pixels::Rgb(image),
                image::DynamicImage::ImageRgba8(image) => Pixels::Rgba(image),
                image::DynamicImage::ImageLuma16(image) => sixteen_luma_to_rgb(&image),
                image::DynamicImage::ImageLumaA16(image) => sixteen_luma_alpha_to_rgba(&image),
                image::DynamicImage::ImageRgb16(image) => sixteen_to_rgb(&image),
                image::DynamicImage::ImageRgba16(image) => sixteen_to_rgba(&image),
                other => Pixels::Rgba(other.to_rgba8()),
            };
            Ok(Decoded::Pixels(pixels, mode))
        }
        Format::Jpeg => {
            if probe.components == 4 {
                let file = File::open(path).map_err(|err| JobError::from_io(&err, path))?;
                let mut decoder = jpeg_decoder::Decoder::new(std::io::BufReader::new(file));
                let cmyk = decoder.decode().map_err(|err| {
                    JobError::Conversion(format!("JPEG をデコードできません: {err}"))
                })?;
                return Ok(Decoded::Cmyk(cmyk));
            }
            let reader = open_reader(path)?;
            let decoder = image::codecs::jpeg::JpegDecoder::new(reader)
                .map_err(|err| JobError::from_image(&err, path))?;
            let dynamic = image::DynamicImage::from_decoder(decoder)
                .map_err(|err| JobError::from_image(&err, path))?;
            match dynamic {
                image::DynamicImage::ImageLuma8(image) => {
                    Ok(Decoded::Pixels(gray8_to_rgb(&image), SourceMode::L))
                }
                image::DynamicImage::ImageRgb8(image) => {
                    Ok(Decoded::Pixels(Pixels::Rgb(image), SourceMode::Rgb))
                }
                other => Ok(Decoded::Pixels(
                    Pixels::Rgb(other.to_rgb8()),
                    SourceMode::Rgb,
                )),
            }
        }
        Format::Gif => {
            let frames = animation::decode_frames(path, probe, interrupt, None)?;
            let first = frames.into_iter().next().ok_or_else(|| {
                JobError::Conversion("保存できるフレームがありません".to_string())
            })?;
            Ok(Decoded::Pixels(
                first,
                SourceMode::Palette {
                    transparency: probe.transparency,
                },
            ))
        }
        Format::Unknown => Err(JobError::Unidentified(format!(
            "cannot identify image file '{}'",
            path.display()
        ))),
    }
}

/// Pillow's effective animation background colour (design spec §3.3 bgcolor
/// note, Phase 0 measurement).
///
/// The Python pipeline passes RGB/RGBA frames to Pillow, whose palette is gone
/// by then, so the background index collapses to a grey value with alpha 255.
/// Without a global colour table Pillow keeps the default transparent black.
fn gif_background(probe: &Probe) -> [u8; 4] {
    if probe.format != Format::Gif || probe.global_palette.is_empty() {
        return [0, 0, 0, 0];
    }
    let index = probe.background_index;
    [index, index, index, 255]
}

fn source_mode(probe: &Probe) -> SourceMode {
    match probe.format {
        Format::Png => match probe.color_type {
            0 => SourceMode::L,
            2 => SourceMode::Rgb,
            3 => SourceMode::Palette {
                transparency: probe.transparency,
            },
            4 => SourceMode::La,
            _ => SourceMode::Rgba,
        },
        Format::Gif => SourceMode::Palette {
            transparency: probe.transparency,
        },
        _ => SourceMode::Rgb,
    }
}

fn encode_settings(config: &Config) -> EncodeSettings {
    EncodeSettings {
        quality: config.quality,
        lossless: config.lossless,
        method_still: config.method_still(),
        method_animation: config.method_animation(),
        exact: config.exact,
    }
}

/// Full conversion of one input file to `destination`.
pub fn convert(
    source: &Path,
    destination: &Path,
    config: &Config,
    probe: &Probe,
    interrupt: &Interrupt,
    budget: Option<u64>,
) -> Result<Vec<String>, JobError> {
    let expected = expected_format(source)
        .ok_or_else(|| JobError::Conversion("変換対象外の拡張子です".to_string()))?;
    if probe.mpo {
        return Err(JobError::Conversion(format!(
            "拡張子は {} ですが、実際の形式は MPO です",
            expected.name()
        )));
    }
    if probe.format == Format::Unknown {
        return Err(JobError::Unidentified(format!(
            "cannot identify image file '{}'",
            source.display()
        )));
    }
    if probe.format != expected {
        return Err(JobError::Conversion(format!(
            "拡張子は {} ですが、実際の形式は {} です",
            expected.name(),
            probe.format.name()
        )));
    }
    if expected == Format::Jpeg && probe.bit_depth != 8 {
        return Err(JobError::Conversion(format!(
            "JPEG のサンプル精度 {} は未対応です",
            probe.bit_depth
        )));
    }

    let mut warnings: Vec<String> = Vec::new();
    let decoded = decode_still(source, probe, interrupt, &mut warnings)?;
    let first_pixels = match &decoded {
        Decoded::Pixels(pixels, _) => Some(pixels.clone()),
        Decoded::Cmyk(_) => None,
    };
    let hidden = if !config.stealth {
        warnings.push(
            "Stealth PNG Info の読み取りが設定で無効になっているため、隠しテキストは引き継ぎません"
                .to_string(),
        );
        String::new()
    } else if skip_stealth_scan(probe, config, &mut warnings) {
        String::new()
    } else {
        stealth::stealth_text(probe, source, first_pixels.as_ref(), interrupt)
    };
    let prepared = metadata::prepare(probe, config, &hidden).map_err(JobError::Conversion)?;
    let start_index = if probe.format == Format::Png && probe.is_apng && probe.apng_default_image {
        1
    } else {
        0
    };

    let is_apng_animation = probe.format == Format::Png
        && probe.is_apng
        && (probe.frames > 1 || probe.apng_default_image);
    if is_apng_animation && probe.durations_ms.is_empty() {
        // acTL exists but there is no animation frame (only a default image),
        // which the Python version also rejects.
        return Err(JobError::Conversion(
            "保存できるフレームがありません".to_string(),
        ));
    }
    let animation_frames = if probe.format == Format::Gif || is_apng_animation {
        animation::decode_frames(source, probe, interrupt, budget)?
    } else {
        Vec::new()
    };
    if is_apng_animation && animation_frames.is_empty() {
        return Err(JobError::Conversion(
            "保存できるフレームがありません".to_string(),
        ));
    }
    let animated = animation_frames.len() > 1;

    let mut frames: Vec<Pixels> = Vec::new();
    let mut durations: Vec<i64> = Vec::new();
    let mut output_icc: Option<Vec<u8>> = None;

    if animated {
        let loop_count = match probe.loop_count {
            Some(value) => value,
            None => {
                if probe.format == Format::Gif {
                    1
                } else {
                    0
                }
            }
        };
        if loop_count < 0 {
            return Err(JobError::Conversion(
                "アニメーションのループ回数が不正です".to_string(),
            ));
        }
        for (index, frame) in animation_frames.into_iter().enumerate() {
            if interrupt.requested() {
                return Err(JobError::Conversion("中断しました".to_string()));
            }
            let duration = probe
                .durations_ms
                .get(index)
                .copied()
                .unwrap_or(config.missing_duration_ms as f64);
            if duration.is_nan() || duration < 0.0 {
                return Err(JobError::Conversion(format!(
                    "フレーム {} の表示時間が不正です",
                    start_index + index
                )));
            }
            let frame = color::apply_orientation(frame, prepared.orientation);
            let (frame, frame_icc) =
                color::normalize_color(frame, source_mode(probe), probe.icc.as_deref())
                    .map_err(JobError::Conversion)?;
            output_icc = frame_icc;
            frames.push(frame);
            durations.push(animation::python_round(duration));
        }
        let (width, height) = (frames[0].width(), frames[0].height());
        let settings = encode_settings(config);
        let pairs: Vec<(Pixels, i64)> = frames.into_iter().zip(durations.iter().copied()).collect();
        let bgcolor = gif_background(probe);
        let data = webp_encode::encode_animation(&pairs, loop_count, bgcolor, &settings)
            .map_err(JobError::Conversion)?;
        finish(
            data,
            (width, height),
            &prepared,
            output_icc,
            config,
            destination,
            &warnings,
        )
    } else if let Some(first) = animation_frames.into_iter().next() {
        let frame = color::apply_orientation(first, prepared.orientation);
        let (frame, frame_icc) =
            color::normalize_color(frame, source_mode(probe), probe.icc.as_deref())
                .map_err(JobError::Conversion)?;
        let single = frame;
        let _ = &mut output_icc;
        output_icc = frame_icc;
        if let Some(duration) = probe.durations_ms.first().copied() {
            if duration.is_nan() || duration < 0.0 {
                return Err(JobError::Conversion(
                    "フレーム 0 の表示時間が不正です".to_string(),
                ));
            }
        }
        let (width, height) = (single.width(), single.height());
        let settings = encode_settings(config);
        let data = webp_encode::encode_still(&single, &settings).map_err(JobError::Conversion)?;
        finish(
            data,
            (width, height),
            &prepared,
            output_icc,
            config,
            destination,
            &warnings,
        )
    } else {
        let single = match decoded {
            Decoded::Pixels(pixels, mode) => {
                let frame = color::apply_orientation(pixels, prepared.orientation);
                let (frame, frame_icc) = color::normalize_color(frame, mode, probe.icc.as_deref())
                    .map_err(JobError::Conversion)?;
                output_icc = frame_icc;
                frame
            }
            Decoded::Cmyk(raw) => {
                let (frame, frame_icc) =
                    color::normalize_cmyk(&raw, probe.width, probe.height, probe.icc.as_deref())
                        .map_err(JobError::Conversion)?;
                output_icc = frame_icc;
                frame
            }
        };
        if let Some(duration) = probe.durations_ms.first().copied() {
            if duration.is_nan() || duration < 0.0 {
                return Err(JobError::Conversion(
                    "フレーム 0 の表示時間が不正です".to_string(),
                ));
            }
        }
        let (width, height) = (single.width(), single.height());
        let settings = encode_settings(config);
        let data = webp_encode::encode_still(&single, &settings).map_err(JobError::Conversion)?;
        finish(
            data,
            (width, height),
            &prepared,
            output_icc,
            config,
            destination,
            &warnings,
        )
    }
}

/// Applies the Stealth scanning policy for the expensive 16-bit path: above
/// `stealth_max_pixels` the scan is skipped and a warning is reported
/// (design spec §4.2; `0` disables the limit).
fn skip_stealth_scan(probe: &Probe, config: &Config, warnings: &mut Vec<String>) -> bool {
    if config.stealth_max_pixels <= 0 {
        return false;
    }
    if probe.format != Format::Png || probe.bit_depth != 16 {
        return false;
    }
    let pixels = probe.width as i64 * probe.height as i64;
    if pixels <= config.stealth_max_pixels {
        return false;
    }
    warnings.push(format!(
        "Stealth PNG Info の走査を省略しました（{}x{} = {} 画素は上限 {} 画素を超えています）",
        probe.width, probe.height, pixels, config.stealth_max_pixels
    ));
    true
}

fn finish(
    data: Vec<u8>,
    dimensions: (u32, u32),
    prepared: &metadata::Prepared,
    output_icc: Option<Vec<u8>>,
    config: &Config,
    destination: &Path,
    extra_warnings: &[String],
) -> Result<Vec<String>, JobError> {
    let mut warnings = extra_warnings.to_vec();
    let mut output = data;
    if config.preserve_metadata {
        let exif_bytes = prepared
            .exif
            .as_ref()
            .filter(|exif| exif.has_data())
            .map(|exif| exif.serialize());
        let metadata = OutputMetadata {
            exif: exif_bytes.as_deref(),
            icc: output_icc.as_deref(),
            xmp: prepared.xmp.as_deref(),
        };
        if !metadata.is_empty() {
            output = webp_encode::mux_metadata(output, &metadata, dimensions.0, dimensions.1)
                .map_err(JobError::Conversion)?;
        }
    }
    std::fs::write(destination, &output).map_err(|err| JobError::from_io(&err, destination))?;
    if config.preserve_metadata {
        let read =
            std::fs::read(destination).map_err(|err| JobError::from_io(&err, destination))?;
        let parsed = webp_encode::read_metadata(&read).map_err(JobError::Conversion)?;
        warnings.extend(
            metadata::verify(
                &parsed,
                prepared.exif.as_ref(),
                output_icc.as_deref(),
                prepared.xmp.as_deref(),
            )
            .map_err(JobError::Conversion)?,
        );
    }
    Ok(warnings)
}
