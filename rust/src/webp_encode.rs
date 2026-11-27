//! WebP encoding through libwebp (direct FFI) and RIFF metadata muxing.
//!
//! Encoding goes through `libwebp-sys` 0.14, which vendors libwebp 1.6.0 — the
//! same version Pillow 13 bundles — so pixel output matches the Python version
//! for the same settings (design spec §3.3, decision D-ENC).
//!
//! Metadata cannot be muxed through libwebp's simple API, so the RIFF container
//! is rewritten here, including the VP8X feature flags (design spec §3.5,
//! decision D-MUX).

use std::cell::Cell;

use libwebp_sys as sys;

use crate::color::Pixels;

thread_local! {
    /// Set while a libwebp FFI call is in progress on this thread. A panic
    /// observed while this is true cannot be recovered safely (design spec
    /// §3.6). Thread local: jobs run on separate threads and a panic in one
    /// job must not be classified by another job's FFI call.
    static FFI_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

pub fn ffi_active() -> bool {
    FFI_ACTIVE.with(|flag| flag.get())
}

fn set_ffi_active(value: bool) {
    FFI_ACTIVE.with(|flag| flag.set(value));
}

#[derive(Clone, Copy, Debug)]
pub struct EncodeSettings {
    pub quality: i64,
    pub lossless: bool,
    pub method_still: i64,
    pub method_animation: i64,
    pub exact: bool,
}

pub const VP8X_ICC: u8 = 0x20;
pub const VP8X_ALPHA: u8 = 0x10;
pub const VP8X_EXIF: u8 = 0x08;
pub const VP8X_XMP: u8 = 0x04;
pub const VP8X_ANIMATION: u8 = 0x02;

/// Runtime libwebp version, e.g. `1.6.0`.
pub fn libwebp_version() -> String {
    let packed = unsafe { sys::WebPGetEncoderVersion() };
    let major = (packed >> 16) & 0xff;
    let minor = (packed >> 8) & 0xff;
    let patch = packed & 0xff;
    format!("{major}.{minor}.{patch}")
}

fn init_config(
    quality: f64,
    lossless: bool,
    method: i64,
    exact: bool,
) -> Result<sys::WebPConfig, String> {
    let mut config: sys::WebPConfig = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        sys::WebPConfigInitInternal(
            &mut config,
            sys::WebPPreset::WEBP_PRESET_DEFAULT,
            quality as f32,
            sys::WEBP_ENCODER_ABI_VERSION as i32,
        )
    };
    if ok == 0 {
        return Err("WebP エンコーダの初期化に失敗しました".to_string());
    }
    // The settings Pillow applies in `_webp.c` (`WebPEncode` /
    // `WebPAnimEncoder.add`); everything else keeps the libwebp defaults.
    config.quality = quality as f32;
    config.lossless = i32::from(lossless);
    config.method = method as i32;
    config.exact = i32::from(exact);
    config.alpha_quality = 100;
    config.thread_level = 0;
    let valid = unsafe { sys::WebPValidateConfig(&config) };
    if valid == 0 {
        return Err("WebP エンコーダ設定が不正です".to_string());
    }
    Ok(config)
}

struct Picture {
    picture: sys::WebPPicture,
}

impl Picture {
    fn new(pixels: &Pixels) -> Result<Self, String> {
        let mut picture: sys::WebPPicture = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            sys::WebPPictureInitInternal(&mut picture, sys::WEBP_ENCODER_ABI_VERSION as i32)
        };
        if ok == 0 {
            return Err("WebP ピクチャの初期化に失敗しました".to_string());
        }
        picture.width = pixels.width() as i32;
        picture.height = pixels.height() as i32;
        // Pillow imports every frame as ARGB with `use_argb = 1` ("don't
        // convert RGB pixels to YUV"); matching that keeps the encoder's
        // internal conversion path identical (Phase 0 finding).
        picture.use_argb = 1;
        let allocated = unsafe { sys::WebPPictureAlloc(&mut picture) };
        if allocated == 0 {
            let code = picture.error_code;
            unsafe { sys::WebPPictureFree(&mut picture) };
            return Err(format!("WebP ピクチャの作成に失敗しました: {code:?}"));
        }
        let stride = picture.argb_stride as usize;
        match pixels {
            Pixels::Rgb(image) => {
                for (x, y, pixel) in image.enumerate_pixels() {
                    let value = u32::from(pixel[0]) << 16
                        | u32::from(pixel[1]) << 8
                        | u32::from(pixel[2])
                        | 0xff00_0000;
                    unsafe {
                        *picture.argb.add(y as usize * stride + x as usize) = value;
                    }
                }
            }
            Pixels::Rgba(image) => {
                for (x, y, pixel) in image.enumerate_pixels() {
                    let value = u32::from(pixel[0]) << 16
                        | u32::from(pixel[1]) << 8
                        | u32::from(pixel[2])
                        | u32::from(pixel[3]) << 24;
                    unsafe {
                        *picture.argb.add(y as usize * stride + x as usize) = value;
                    }
                }
            }
        }
        Ok(Picture { picture })
    }

    fn as_mut(&mut self) -> &mut sys::WebPPicture {
        &mut self.picture
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        unsafe { sys::WebPPictureFree(&mut self.picture) };
    }
}

pub fn encode_still(pixels: &Pixels, settings: &EncodeSettings) -> Result<Vec<u8>, String> {
    let config = init_config(
        settings.quality as f64,
        settings.lossless,
        settings.method_still,
        settings.exact,
    )?;
    let mut picture = Picture::new(pixels)?;
    let mut writer: sys::WebPMemoryWriter = unsafe { std::mem::zeroed() };
    unsafe { sys::WebPMemoryWriterInit(&mut writer) };
    {
        let raw = picture.as_mut();
        raw.writer = Some(sys::WebPMemoryWrite);
        raw.custom_ptr = (&mut writer) as *mut sys::WebPMemoryWriter as *mut std::ffi::c_void;
    }
    set_ffi_active(true);
    let ok = unsafe { sys::WebPEncode(&config, picture.as_mut()) };
    set_ffi_active(false);
    if ok == 0 {
        let code = picture.as_mut().error_code;
        unsafe { sys::WebPMemoryWriterClear(&mut writer) };
        return Err(format!("WebP エンコードに失敗しました: {code:?}"));
    }
    let bytes = unsafe { std::slice::from_raw_parts(writer.mem, writer.size) }.to_vec();
    unsafe { sys::WebPMemoryWriterClear(&mut writer) };
    Ok(bytes)
}

pub fn encode_animation(
    frames: &[(Pixels, i64)],
    loop_count: i64,
    bgcolor: [u8; 4],
    settings: &EncodeSettings,
) -> Result<Vec<u8>, String> {
    if frames.is_empty() {
        return Err("保存できるフレームがありません".to_string());
    }
    let (width, height) = (frames[0].0.width(), frames[0].0.height());
    if width == 0 || height == 0 {
        return Err("保存できるフレームがありません".to_string());
    }
    let config = init_config(
        settings.quality as f64,
        settings.lossless,
        settings.method_animation,
        false,
    )?;
    let mut options: sys::WebPAnimEncoderOptions = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        sys::WebPAnimEncoderOptionsInitInternal(&mut options, sys::WEBP_MUX_ABI_VERSION as i32)
    };
    if ok == 0 {
        return Err("WebP アニメーション設定の初期化に失敗しました".to_string());
    }
    // Values Pillow passes to `WebPAnimEncoder` (design spec §3.3).
    options.anim_params.bgcolor = ((bgcolor[3] as u32) << 24)
        | ((bgcolor[0] as u32) << 16)
        | ((bgcolor[1] as u32) << 8)
        | bgcolor[2] as u32;
    options.anim_params.loop_count = loop_count.clamp(0, i32::MAX as i64) as i32;
    options.minimize_size = 0;
    options.kmin = if settings.lossless { 9 } else { 3 };
    options.kmax = if settings.lossless { 17 } else { 5 };
    options.allow_mixed = 0;
    options.verbose = 0;
    let encoder = unsafe {
        sys::WebPAnimEncoderNewInternal(
            width as i32,
            height as i32,
            &options,
            sys::WEBP_MUX_ABI_VERSION as i32,
        )
    };
    if encoder.is_null() {
        return Err("WebP アニメーションエンコーダを作成できません".to_string());
    }
    let mut timestamp: i64 = 0;
    for (pixels, duration) in frames {
        if pixels.width() != width || pixels.height() != height {
            unsafe { sys::WebPAnimEncoderDelete(encoder) };
            return Err("フレームの寸法が一致しません".to_string());
        }
        let mut picture = match Picture::new(pixels) {
            Ok(picture) => picture,
            Err(error) => {
                unsafe { sys::WebPAnimEncoderDelete(encoder) };
                return Err(error);
            }
        };
        set_ffi_active(true);
        let added = unsafe {
            sys::WebPAnimEncoderAdd(
                encoder,
                picture.as_mut(),
                timestamp.min(i32::MAX as i64) as i32,
                &config,
            )
        };
        set_ffi_active(false);
        if added == 0 {
            let message = encoder_error(encoder);
            unsafe { sys::WebPAnimEncoderDelete(encoder) };
            return Err(format!(
                "WebP アニメーションのエンコードに失敗しました: {message}"
            ));
        }
        timestamp = timestamp.saturating_add((*duration).max(0));
    }
    // Flush: a null frame with the total duration (Pillow does the same).
    let flushed = unsafe {
        sys::WebPAnimEncoderAdd(
            encoder,
            std::ptr::null_mut(),
            timestamp.min(i32::MAX as i64) as i32,
            std::ptr::null(),
        )
    };
    if flushed == 0 {
        let message = encoder_error(encoder);
        unsafe { sys::WebPAnimEncoderDelete(encoder) };
        return Err(format!(
            "WebP アニメーションの確定に失敗しました: {message}"
        ));
    }
    let mut data: sys::WebPData = unsafe { std::mem::zeroed() };
    set_ffi_active(true);
    let assembled = unsafe { sys::WebPAnimEncoderAssemble(encoder, &mut data) };
    set_ffi_active(false);
    if assembled == 0 {
        let message = encoder_error(encoder);
        unsafe { sys::WebPAnimEncoderDelete(encoder) };
        return Err(format!(
            "WebP アニメーションの出力に失敗しました: {message}"
        ));
    }
    let bytes = if data.bytes.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data.bytes, data.size) }.to_vec()
    };
    if !data.bytes.is_null() {
        unsafe { sys::WebPFree(data.bytes as *mut std::ffi::c_void) };
    }
    unsafe { sys::WebPAnimEncoderDelete(encoder) };
    Ok(bytes)
}

fn encoder_error(encoder: *mut sys::WebPAnimEncoder) -> String {
    let pointer = unsafe { sys::WebPAnimEncoderGetError(encoder) };
    if pointer.is_null() {
        "unknown error".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}

pub struct OutputMetadata<'a> {
    pub exif: Option<&'a [u8]>,
    pub icc: Option<&'a [u8]>,
    pub xmp: Option<&'a [u8]>,
}

impl OutputMetadata<'_> {
    pub fn is_empty(&self) -> bool {
        self.exif.is_none() && self.icc.is_none() && self.xmp.is_none()
    }
}

struct Chunk {
    id: [u8; 4],
    payload: Vec<u8>,
}

fn parse_chunks(data: &[u8]) -> Result<Vec<Chunk>, String> {
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return Err("WebP コンテナが不正です".to_string());
    }
    let mut chunks = Vec::new();
    let mut position = 12usize;
    while position + 8 <= data.len() {
        let id = [
            data[position],
            data[position + 1],
            data[position + 2],
            data[position + 3],
        ];
        let size = u32::from_le_bytes([
            data[position + 4],
            data[position + 5],
            data[position + 6],
            data[position + 7],
        ]) as usize;
        let start = position + 8;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| "WebP チャンクが破損しています".to_string())?;
        chunks.push(Chunk {
            id,
            payload: data[start..end].to_vec(),
        });
        position = end + (size & 1);
    }
    Ok(chunks)
}

fn write_chunks(chunks: &[Chunk]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(b"WEBP");
    for chunk in chunks {
        body.extend_from_slice(&chunk.id);
        body.extend_from_slice(&(chunk.payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&chunk.payload);
        if chunk.payload.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn vp8x_payload(flags: u8, width: u32, height: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(10);
    payload.push(flags);
    payload.extend_from_slice(&[0, 0, 0]);
    let canvas_width = width.saturating_sub(1).min(0x00ff_ffff);
    let canvas_height = height.saturating_sub(1).min(0x00ff_ffff);
    payload.extend_from_slice(&canvas_width.to_le_bytes()[..3]);
    payload.extend_from_slice(&canvas_height.to_le_bytes()[..3]);
    payload
}

/// Inserts EXIF/ICCP/XMP chunks and keeps the VP8X feature flags in sync.
pub fn mux_metadata(
    data: Vec<u8>,
    metadata: &OutputMetadata,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    if metadata.is_empty() {
        return Ok(data);
    }
    let chunks = parse_chunks(&data)?;
    let existing_vp8x = chunks.iter().find(|chunk| &chunk.id == b"VP8X");
    let mut base_flags = 0u8;
    let mut canvas = (width.max(1), height.max(1));
    if let Some(chunk) = existing_vp8x {
        if let Some(flags) = chunk.payload.first() {
            base_flags = *flags & (VP8X_ALPHA | VP8X_ANIMATION);
        }
        if chunk.payload.len() >= 10 {
            let w =
                u32::from_le_bytes([chunk.payload[4], chunk.payload[5], chunk.payload[6], 0]) + 1;
            let h =
                u32::from_le_bytes([chunk.payload[7], chunk.payload[8], chunk.payload[9], 0]) + 1;
            canvas = (w, h);
        }
    }
    if chunks.iter().any(|chunk| &chunk.id == b"ALPH") {
        base_flags |= VP8X_ALPHA;
    }
    if chunks
        .iter()
        .any(|chunk| &chunk.id == b"ANIM" || &chunk.id == b"ANMF")
    {
        base_flags |= VP8X_ANIMATION;
    }
    let mut flags = base_flags;
    if metadata.icc.is_some() {
        flags |= VP8X_ICC;
    }
    if metadata.exif.is_some() {
        flags |= VP8X_EXIF;
    }
    if metadata.xmp.is_some() {
        flags |= VP8X_XMP;
    }

    let mut output: Vec<Chunk> = Vec::with_capacity(chunks.len() + 4);
    output.push(Chunk {
        id: *b"VP8X",
        payload: vp8x_payload(flags, canvas.0, canvas.1),
    });
    if let Some(icc) = metadata.icc {
        output.push(Chunk {
            id: *b"ICCP",
            payload: icc.to_vec(),
        });
    }
    for chunk in chunks {
        if matches!(&chunk.id, b"VP8X" | b"ICCP" | b"EXIF" | b"XMP ") {
            continue;
        }
        output.push(chunk);
    }
    if let Some(exif) = metadata.exif {
        output.push(Chunk {
            id: *b"EXIF",
            payload: exif.to_vec(),
        });
    }
    if let Some(xmp) = metadata.xmp {
        output.push(Chunk {
            id: *b"XMP ",
            payload: xmp.to_vec(),
        });
    }
    Ok(write_chunks(&output))
}

#[derive(Debug, Default)]
pub struct WebpMetadata {
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    pub vp8x_flags: Option<u8>,
}

pub fn read_metadata(data: &[u8]) -> Result<WebpMetadata, String> {
    let chunks = parse_chunks(data)?;
    let mut metadata = WebpMetadata::default();
    for chunk in chunks {
        match &chunk.id {
            b"VP8X" => metadata.vp8x_flags = chunk.payload.first().copied(),
            b"EXIF" => metadata.exif = Some(chunk.payload),
            b"ICCP" => metadata.icc = Some(chunk.payload),
            b"XMP " => metadata.xmp = Some(chunk.payload),
            _ => {}
        }
    }
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libwebp_version_is_available() {
        let version = libwebp_version();
        assert_eq!(version.matches('.').count(), 2, "{version}");
    }

    #[test]
    fn still_encode_round_trip_and_mux() {
        let image = image::RgbImage::from_pixel(4, 3, image::Rgb([10, 20, 30]));
        let settings = EncodeSettings {
            quality: 90,
            lossless: false,
            method_still: 4,
            method_animation: 0,
            exact: false,
        };
        let data = encode_still(&Pixels::Rgb(image), &settings).unwrap();
        assert_eq!(&data[..4], b"RIFF");
        let metadata = OutputMetadata {
            exif: Some(b"II\x2a\x00"),
            icc: Some(b"profile"),
            xmp: Some(b"<xmp/>"),
        };
        let muxed = mux_metadata(data, &metadata, 4, 3).unwrap();
        let read = read_metadata(&muxed).unwrap();
        assert_eq!(read.exif.as_deref(), Some(&b"II\x2a\x00"[..]));
        assert_eq!(read.icc.as_deref(), Some(&b"profile"[..]));
        assert_eq!(read.xmp.as_deref(), Some(&b"<xmp/>"[..]));
        let flags = read.vp8x_flags.unwrap();
        assert_eq!(flags & VP8X_EXIF, VP8X_EXIF);
        assert_eq!(flags & VP8X_ICC, VP8X_ICC);
        assert_eq!(flags & VP8X_XMP, VP8X_XMP);
    }

    #[test]
    fn animation_encode_has_vp8x_and_frames() {
        let red = Pixels::Rgba(image::RgbaImage::from_pixel(
            4,
            4,
            image::Rgba([255, 0, 0, 255]),
        ));
        let blue = Pixels::Rgba(image::RgbaImage::from_pixel(
            4,
            4,
            image::Rgba([0, 0, 255, 255]),
        ));
        let settings = EncodeSettings {
            quality: 90,
            lossless: false,
            method_still: 4,
            method_animation: 0,
            exact: false,
        };
        let data = encode_animation(&[(red, 70), (blue, 130)], 2, [0, 0, 0, 0], &settings).unwrap();
        let read = read_metadata(&data).unwrap();
        assert!(read.vp8x_flags.is_some());
        assert!(read.vp8x_flags.unwrap() & VP8X_ANIMATION != 0);
        // The animated WebP has ANIM and at least two ANMF chunks.
        let animation_frames = parse_chunks(&data)
            .unwrap()
            .iter()
            .filter(|chunk| &chunk.id == b"ANMF")
            .count();
        assert_eq!(animation_frames, 2);
    }
}
