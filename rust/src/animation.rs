//! GIF / APNG frame decoding for the animation path.
//!
//! `image`'s GIF decoder returns disposal-composited frames and its APNG
//! decoder returns animation frames with the separate default image excluded,
//! which matches Pillow's frame model (design spec §3.4, Phase 0 findings).

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use image::AnimationDecoder;

use crate::color::Pixels;
use crate::interrupt::Interrupt;
use crate::probe::{Format, Probe};
use crate::report::JobError;

pub fn decode_frames(
    path: &Path,
    probe: &Probe,
    interrupt: &Interrupt,
    budget: Option<u64>,
) -> Result<Vec<Pixels>, JobError> {
    let file = File::open(path).map_err(|err| JobError::from_io(&err, path))?;
    let reader = BufReader::with_capacity(64 * 1024, file);
    let frames: Vec<Pixels> = match probe.format {
        Format::Gif => {
            let decoder = image::codecs::gif::GifDecoder::new(reader)
                .map_err(|err| JobError::from_image(&err, path))?;
            let mut out = Vec::new();
            for frame in decoder.into_frames() {
                if interrupt.requested() {
                    return Err(JobError::Conversion("中断しました".to_string()));
                }
                let frame = frame.map_err(|err| JobError::from_image(&err, path))?;
                let buffer = frame.into_buffer();
                check_frame_budget(budget, out.len(), &buffer)?;
                out.push(Pixels::Rgba(buffer));
            }
            out
        }
        Format::Png => {
            let decoder = image::codecs::png::PngDecoder::new(reader)
                .map_err(|err| JobError::from_image(&err, path))?;
            let apng = decoder
                .apng()
                .map_err(|err| JobError::from_image(&err, path))?;
            let mut out = Vec::new();
            for frame in apng.into_frames() {
                if interrupt.requested() {
                    return Err(JobError::Conversion("中断しました".to_string()));
                }
                let frame = frame.map_err(|err| JobError::from_image(&err, path))?;
                let buffer = frame.into_buffer();
                check_frame_budget(budget, out.len(), &buffer)?;
                out.push(Pixels::Rgba(buffer));
            }
            out
        }
        _ => Vec::new(),
    };
    Ok(frames)
}

/// Guards the provisional memory estimate: a frame is rejected before it is
/// kept when the accumulated retained pixels exceed the granted budget
/// (design spec §3.4 "保険" check).
pub(crate) fn check_frame_budget(
    budget: Option<u64>,
    frame_index: usize,
    buffer: &image::RgbaImage,
) -> Result<(), JobError> {
    let Some(budget) = budget else {
        return Ok(());
    };
    let retained = (frame_index as u64 + 1)
        .saturating_mul(buffer.width() as u64)
        .saturating_mul(buffer.height() as u64)
        .saturating_mul(4);
    if retained > budget {
        return Err(JobError::Memory(format!(
            "フレーム {frame_index} の保持量が予算の見積もり {budget} バイトを超えました"
        )));
    }
    Ok(())
}

/// Python's `round()` (round half to even) on a millisecond duration.
pub fn python_round(value: f64) -> i64 {
    let floor = value.floor();
    let diff = value - floor;
    let result = if (diff - 0.5).abs() < f64::EPSILON {
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        (value + 0.5).floor()
    };
    result as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_budget_guard_rejects_undersized_estimates() {
        let buffer = image::RgbaImage::new(100, 100); // 40,000 bytes per frame
        assert!(check_frame_budget(None, 0, &buffer).is_ok());
        assert!(check_frame_budget(Some(80_000), 1, &buffer).is_ok());
        let error = check_frame_budget(Some(80_000), 2, &buffer).unwrap_err();
        assert!(matches!(error, crate::report::JobError::Memory(_)));
    }

    #[test]
    fn rounding_matches_python() {
        assert_eq!(python_round(70.0), 70);
        assert_eq!(python_round(70.5), 70);
        assert_eq!(python_round(71.5), 72);
        assert_eq!(python_round(69.5), 70);
        assert_eq!(python_round(0.4), 0);
        assert_eq!(python_round(66.666), 67);
    }
}
