//! Orientation application and colour normalization (ICC handling).
//!
//! Mirrors the Python `orient()` / `normalize_color()` semantics, including the
//! distinct failure messages for mismatched colour spaces (design spec §1.6,
//! §3.5, decision D-CMS).

use image::{GrayImage, RgbImage, RgbaImage};

#[derive(Clone)]
pub enum Pixels {
    Rgb(RgbImage),
    Rgba(RgbaImage),
}

impl Pixels {
    pub fn width(&self) -> u32 {
        match self {
            Pixels::Rgb(image) => image.width(),
            Pixels::Rgba(image) => image.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Pixels::Rgb(image) => image.height(),
            Pixels::Rgba(image) => image.height(),
        }
    }

    pub fn has_alpha(&self) -> bool {
        matches!(self, Pixels::Rgba(_))
    }

    pub fn channels(&self) -> u32 {
        if self.has_alpha() {
            4
        } else {
            3
        }
    }

    pub fn to_rgba(&self) -> RgbaImage {
        match self {
            Pixels::Rgba(image) => image.clone(),
            Pixels::Rgb(image) => {
                let mut out = RgbaImage::new(image.width(), image.height());
                for (x, y, pixel) in image.enumerate_pixels() {
                    out.put_pixel(x, y, image::Rgba([pixel[0], pixel[1], pixel[2], 255]));
                }
                out
            }
        }
    }

    pub fn into_rgb(self) -> RgbImage {
        match self {
            Pixels::Rgb(image) => image,
            Pixels::Rgba(image) => {
                let mut out = RgbImage::new(image.width(), image.height());
                for (x, y, pixel) in image.enumerate_pixels() {
                    out.put_pixel(x, y, image::Rgb([pixel[0], pixel[1], pixel[2]]));
                }
                out
            }
        }
    }

    pub fn to_rgb(&self) -> RgbImage {
        self.clone().into_rgb()
    }

    pub fn pixels(&self) -> u64 {
        self.width() as u64 * self.height() as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    L,
    La,
    Rgb,
    Rgba,
    Palette { transparency: bool },
}

/// Pillow's `ORIENTATION_OPERATIONS` (Exif orientation 2..=8).
pub fn apply_orientation(pixels: Pixels, orientation: Option<u16>) -> Pixels {
    let Some(orientation) = orientation else {
        return pixels;
    };
    if !(2..=8).contains(&orientation) {
        return pixels;
    }
    match pixels {
        Pixels::Rgb(image) => Pixels::Rgb(transform_rgb(image, orientation)),
        Pixels::Rgba(image) => Pixels::Rgba(transform_rgba(image, orientation)),
    }
}

fn target_coordinate(orientation: u16, x: u32, y: u32, width: u32, height: u32) -> (u32, u32) {
    match orientation {
        2 => (width - 1 - x, y),
        3 => (width - 1 - x, height - 1 - y),
        4 => (x, height - 1 - y),
        5 => (y, x),
        6 => (height - 1 - y, x),
        7 => (height - 1 - y, width - 1 - x),
        8 => (y, width - 1 - x),
        _ => (x, y),
    }
}

fn transform_rgb(image: RgbImage, orientation: u16) -> RgbImage {
    let (width, height) = image.dimensions();
    let (new_width, new_height) = if matches!(orientation, 5..=8) {
        (height, width)
    } else {
        (width, height)
    };
    let mut out = RgbImage::new(new_width, new_height);
    for y in 0..height {
        for x in 0..width {
            let pixel = *image.get_pixel(x, y);
            let (nx, ny) = target_coordinate(orientation, x, y, width, height);
            out.put_pixel(nx, ny, pixel);
        }
    }
    out
}

fn transform_rgba(image: RgbaImage, orientation: u16) -> RgbaImage {
    let (width, height) = image.dimensions();
    let (new_width, new_height) = if matches!(orientation, 5..=8) {
        (height, width)
    } else {
        (width, height)
    };
    let mut out = RgbaImage::new(new_width, new_height);
    for y in 0..height {
        for x in 0..width {
            let pixel = *image.get_pixel(x, y);
            let (nx, ny) = target_coordinate(orientation, x, y, width, height);
            out.put_pixel(nx, ny, pixel);
        }
    }
    out
}

fn profile_color_space(icc: &[u8]) -> Result<lcms2::ColorSpaceSignature, String> {
    let profile = lcms2::Profile::new_icc(icc)
        .map_err(|err| format!("ICC プロファイルを読めません: {err}"))?;
    Ok(profile.color_space())
}

thread_local! {
    /// The sRGB profile used for CMS output, created once per thread (PERF-06).
    /// `lcms2::Profile` is not `Sync`, so the cache is thread local.
    static SRGB_PROFILE: lcms2::Profile = lcms2::Profile::new_srgb();
}

pub fn srgb_profile_bytes() -> Vec<u8> {
    SRGB_PROFILE.with(|profile| profile.icc().unwrap_or_default())
}

fn transform_to_rgb(
    input: &[u8],
    profile: &lcms2::Profile,
    input_format: lcms2::PixelFormat,
) -> Result<Vec<u8>, String> {
    SRGB_PROFILE.with(|srgb| {
        let transform = lcms2::Transform::new(
            profile,
            input_format,
            srgb,
            lcms2::PixelFormat::RGB_8,
            lcms2::Intent::Perceptual,
        )
        .map_err(|err| format!("ICC を使った色変換に失敗しました: {err}"))?;
        let bytes_per_pixel = input_format.bytes_per_pixel();
        let pixels = input.len() / bytes_per_pixel;
        let mut out = vec![0u8; pixels * 3];
        transform.transform_pixels(input, &mut out);
        Ok(out)
    })
}

fn raw_to_rgb(raw: &[u8], width: u32, height: u32) -> RgbImage {
    let mut image = RgbImage::new(width, height);
    for (index, pixel) in image.pixels_mut().enumerate() {
        let start = index * 3;
        if start + 2 < raw.len() {
            *pixel = image::Rgb([raw[start], raw[start + 1], raw[start + 2]]);
        }
    }
    image
}

/// Normalizes a decoded frame that is not CMYK to RGB/RGBA.
///
/// Returns the converted pixels and the ICC profile to write into the output.
pub fn normalize_color(
    pixels: Pixels,
    mode: SourceMode,
    icc: Option<&[u8]>,
) -> Result<(Pixels, Option<Vec<u8>>), String> {
    let color_space = match icc {
        Some(profile) => Some(profile_color_space(profile)?),
        None => None,
    };
    if matches!(mode, SourceMode::L | SourceMode::La) && icc.is_some() {
        if color_space != Some(lcms2::ColorSpaceSignature::GrayData) {
            return Err("画像の色空間と ICC プロファイルが一致しません".to_string());
        }
        let profile = lcms2::Profile::new_icc(icc.unwrap())
            .map_err(|err| format!("ICC プロファイルを読めません: {err}"))?;
        let width = pixels.width();
        let height = pixels.height();
        let (gray, alpha): (Vec<u8>, Option<Vec<u8>>) = match mode {
            SourceMode::La => {
                let owned = pixels.to_rgba();
                (
                    owned.pixels().map(|pixel| pixel[0]).collect(),
                    Some(owned.pixels().map(|pixel| pixel[3]).collect()),
                )
            }
            _ => (
                match &pixels {
                    Pixels::Rgb(image) => image.pixels().map(|pixel| pixel[0]).collect(),
                    Pixels::Rgba(image) => image.pixels().map(|pixel| pixel[0]).collect(),
                },
                None,
            ),
        };
        let rgb = transform_to_rgb(&gray, &profile, lcms2::PixelFormat::GRAY_8)?;
        let image = raw_to_rgb(&rgb, width, height);
        let converted = match alpha {
            Some(alpha) => {
                let mut out = RgbaImage::new(width, height);
                for (index, pixel) in out.pixels_mut().enumerate() {
                    let source = image.get_pixel((index as u32) % width, (index as u32) / width);
                    *pixel = image::Rgba([
                        source[0],
                        source[1],
                        source[2],
                        alpha.get(index).copied().unwrap_or(255),
                    ]);
                }
                Pixels::Rgba(out)
            }
            None => Pixels::Rgb(image),
        };
        return Ok((converted, Some(srgb_profile_bytes())));
    }
    match mode {
        SourceMode::Rgb | SourceMode::Rgba => {
            if let Some(space) = color_space {
                if space != lcms2::ColorSpaceSignature::RgbData {
                    return Err("画像の色空間と ICC プロファイルが一致しません".to_string());
                }
            }
            Ok((pixels, icc.map(|profile| profile.to_vec())))
        }
        _ => {
            let has_alpha = matches!(mode, SourceMode::La)
                || matches!(mode, SourceMode::Palette { transparency: true })
                || pixels.has_alpha();
            let converted = if has_alpha {
                Pixels::Rgba(pixels.to_rgba())
            } else {
                Pixels::Rgb(pixels.into_rgb())
            };
            if let Some(space) = color_space {
                if space != lcms2::ColorSpaceSignature::RgbData {
                    return Err(
                        "変換後の RGB 画像に元の ICC プロファイルを適用できません".to_string()
                    );
                }
            }
            Ok((converted, icc.map(|profile| profile.to_vec())))
        }
    }
}

/// CMYK JPEG normalization: CMS when a profile is present, Pillow's naive
/// conversion otherwise.
pub fn normalize_cmyk(
    cmyk: &[u8],
    width: u32,
    height: u32,
    icc: Option<&[u8]>,
) -> Result<(Pixels, Option<Vec<u8>>), String> {
    match icc {
        Some(profile_bytes) => {
            if profile_color_space(profile_bytes)? != lcms2::ColorSpaceSignature::CmykData {
                return Err("画像の色空間と ICC プロファイルが一致しません".to_string());
            }
            let profile = lcms2::Profile::new_icc(profile_bytes)
                .map_err(|err| format!("ICC プロファイルを読めません: {err}"))?;
            let rgb = transform_to_rgb(cmyk, &profile, lcms2::PixelFormat::CMYK_8)?;
            Ok((
                Pixels::Rgb(raw_to_rgb(&rgb, width, height)),
                Some(srgb_profile_bytes()),
            ))
        }
        None => Ok((Pixels::Rgb(cmyk_to_rgb_naive(cmyk, width, height)), None)),
    }
}

/// Pillow's naive CMYK -> RGB conversion (`255 - (channel + key)` clipped).
pub fn cmyk_to_rgb_naive(cmyk: &[u8], width: u32, height: u32) -> RgbImage {
    let mut out = RgbImage::new(width, height);
    for (index, pixel) in out.pixels_mut().enumerate() {
        let start = index * 4;
        if start + 3 >= cmyk.len() {
            break;
        }
        let k = cmyk[start + 3] as i32;
        let c = cmyk[start] as i32;
        let m = cmyk[start + 1] as i32;
        let y = cmyk[start + 2] as i32;
        *pixel = image::Rgb([
            (255 - (c + k)).clamp(0, 255) as u8,
            (255 - (m + k)).clamp(0, 255) as u8,
            (255 - (y + k)).clamp(0, 255) as u8,
        ]);
    }
    out
}

pub fn gray_from_luma(image: GrayImage) -> Pixels {
    let mut out = RgbImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        out.put_pixel(x, y, image::Rgb([pixel[0], pixel[0], pixel[0]]));
    }
    Pixels::Rgb(out)
}
