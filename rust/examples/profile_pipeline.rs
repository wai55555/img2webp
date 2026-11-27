//! Stage profiler for the still-image pipeline (measurement only).
//!
//! Measures the stages of one still-image conversion separately so that the
//! completion conditions of the design spec can be checked with numbers:
//!
//! - Stealth negative case (design spec §4.2: "the extra cost of the Stealth
//!   negative case must stay within 10% of the conversion body"). Run the same
//!   files with `--stealth on` and `--stealth off`; on a corpus without a
//!   payload the difference is exactly the scan cost.
//! - H2 / H3 (design spec §4.1): share of decode, Stealth and verification in
//!   the per-file time.
//! - H4: `budget::estimate` per file, to see whether long animations are
//!   limited by the memory budget rather than by the encoders.
//!
//! The pipeline mirrors `convert::convert()` for still images: probe → decode
//! → Stealth → metadata preparation → orientation / colour normalisation →
//! encode → metadata mux → write → post-save verification. Animation inputs
//! are skipped (they are covered by `tests/benchmark.py`).
//!
//! Usage (from the repository root):
//!
//! ```text
//! cargo run --release --no-default-features --example profile_pipeline -- \
//!     [--stealth on|off] [--runs 5] [--config config.ini] <dir> [<dir> ...]
//! ```
//!
//! Output: one CSV row per file (medians over `runs`) and a summary block per
//! directory on stderr.

use std::path::{Path, PathBuf};
use std::time::Instant;

use img2webp::budget;
use img2webp::color;
use img2webp::config::{self, Config};
use img2webp::convert::{self, Decoded};
use img2webp::interrupt::Interrupt;
use img2webp::metadata;
use img2webp::probe::{self, Format};
use img2webp::stealth;
use img2webp::webp_encode::{self, EncodeSettings, OutputMetadata};

#[derive(Default, Clone, Copy)]
struct Stages {
    probe: f64,
    decode: f64,
    stealth: f64,
    prepare: f64,
    color: f64,
    encode: f64,
    mux: f64,
    write: f64,
    verify: f64,
    total: f64,
}

impl Stages {
    fn sum(&self) -> f64 {
        self.probe
            + self.decode
            + self.stealth
            + self.prepare
            + self.color
            + self.encode
            + self.mux
            + self.write
            + self.verify
    }
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
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

fn run_once(
    source: &Path,
    config: &Config,
    settings: &EncodeSettings,
    stealth_on: bool,
    destination: &Path,
) -> Option<Stages> {
    let interrupt = Interrupt::new();
    let mut stages = Stages::default();
    let start = Instant::now();

    let probed = {
        let t = Instant::now();
        let probed = probe::probe(source).ok()?;
        stages.probe = ms(t);
        probed
    };
    if probed.format != Format::Png {
        return None;
    }
    if probed.frames > 1 && probed.is_apng {
        // Animations are measured by `tests/bench_memory.py`; only the memory
        // estimate is reported here so H4 can be judged.
        let estimate = budget::estimate(&probed, 0);
        eprintln!(
            "ANIMATION estimate_bytes={estimate} frames={} size={}x{} file={}",
            probed.frames,
            probed.width,
            probed.height,
            source.display()
        );
        return None;
    }
    let decoded = {
        let t = Instant::now();
        let mut decode_warnings: Vec<String> = Vec::new();
        let decoded = convert::decode_still(source, &probed, &interrupt, &mut decode_warnings)
            .ok()?;
        stages.decode = ms(t);
        decoded
    };
    let (pixels, mode) = match decoded {
        Decoded::Pixels(pixels, mode) => (pixels, mode),
        Decoded::Cmyk(_) => return None,
    };
    let first_frame = pixels.clone();
    let hidden = {
        let t = Instant::now();
        let hidden = if stealth_on {
            stealth::stealth_text(&probed, source, Some(&pixels), &interrupt)
        } else {
            String::new()
        };
        stages.stealth = ms(t);
        hidden
    };
    let prepared = {
        let t = Instant::now();
        let prepared = metadata::prepare(&probed, config, &hidden).ok()?;
        stages.prepare = ms(t);
        prepared
    };
    drop(first_frame);
    let (pixels, output_icc) = {
        let t = Instant::now();
        let pixels = color::apply_orientation(pixels, prepared.orientation);
        let (pixels, icc) = color::normalize_color(pixels, mode, probed.icc.as_deref()).ok()?;
        stages.color = ms(t);
        (pixels, icc)
    };
    let (width, height) = (pixels.width(), pixels.height());
    let encoded = {
        let t = Instant::now();
        let encoded = webp_encode::encode_still(&pixels, settings).ok()?;
        stages.encode = ms(t);
        encoded
    };
    drop(pixels);
    let muxed = {
        let t = Instant::now();
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
        let muxed = if metadata.is_empty() {
            encoded
        } else {
            webp_encode::mux_metadata(encoded, &metadata, width, height).ok()?
        };
        stages.mux = ms(t);
        muxed
    };
    {
        let t = Instant::now();
        std::fs::write(destination, &muxed).ok()?;
        stages.write = ms(t);
    }
    {
        let t = Instant::now();
        let read = std::fs::read(destination).ok()?;
        let parsed = webp_encode::read_metadata(&read).ok()?;
        let _ = metadata::verify(
            &parsed,
            prepared.exif.as_ref(),
            output_icc.as_deref(),
            prepared.xmp.as_deref(),
        )
        .ok()?;
        stages.verify = ms(t);
    }
    stages.total = ms(start);
    Some(stages)
}

fn files_in(directory: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .map(|extension| extension.eq_ignore_ascii_case("png"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let mut stealth_on = true;
    let mut runs = 5usize;
    let mut config_path = PathBuf::from("config.ini");
    let mut directories: Vec<PathBuf> = Vec::new();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--stealth" => {
                stealth_on = matches!(arguments.next().as_deref(), Some("on"));
            }
            "--runs" => {
                runs = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(5);
            }
            "--config" => {
                if let Some(value) = arguments.next() {
                    config_path = PathBuf::from(value);
                }
            }
            other => directories.push(PathBuf::from(other)),
        }
    }
    if directories.is_empty() {
        eprintln!("usage: profile_pipeline [--stealth on|off] [--runs N] [--config PATH] <dir>...");
        std::process::exit(2);
    }
    let project_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let config = match config::load(&config_path, &project_dir) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("config error: {error}");
            std::process::exit(2);
        }
    };
    let settings = encode_settings(&config);
    let scratch = std::env::temp_dir().join(format!("img2webp-profile-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch directory");
    let destination = scratch.join("out.webp");
    let mut first_row = true;

    for directory in &directories {
        let label = directory
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| directory.display().to_string());
        let files = files_in(directory);
        let mut totals = Stages::default();
        let mut count = 0usize;
        let mut estimate_total: u64 = 0;
        for file in &files {
            let mut samples: Vec<Stages> = Vec::new();
            // Warm-up run: the first iteration pays for the OS file cache.
            let _ = run_once(file, &config, &settings, stealth_on, &destination);
            for _ in 0..runs {
                if let Some(stages) = run_once(file, &config, &settings, stealth_on, &destination) {
                    samples.push(stages);
                }
            }
            if samples.is_empty() {
                continue;
            }
            let column = |pick: fn(&Stages) -> f64| {
                let mut values: Vec<f64> = samples.iter().map(pick).collect();
                median(&mut values)
            };
            let probe_ms = column(|sample| sample.probe);
            let decode_ms = column(|sample| sample.decode);
            let stealth_ms = column(|sample| sample.stealth);
            let prepare_ms = column(|sample| sample.prepare);
            let color_ms = column(|sample| sample.color);
            let encode_ms = column(|sample| sample.encode);
            let mux_ms = column(|sample| sample.mux);
            let write_ms = column(|sample| sample.write);
            let verify_ms = column(|sample| sample.verify);
            let total_ms = column(|sample| sample.total);
            let estimate = probe::probe(file)
                .map(|probe| budget::estimate(&probe, 0))
                .unwrap_or(0);
            estimate_total += estimate;
            if first_row {
                println!(
                    "dir,file,estimate_bytes,probe_ms,decode_ms,stealth_ms,prepare_ms,color_ms,encode_ms,mux_ms,write_ms,verify_ms,stage_sum_ms,total_ms"
                );
                first_row = false;
            }
            let stage_sum = probe_ms
                + decode_ms
                + stealth_ms
                + prepare_ms
                + color_ms
                + encode_ms
                + mux_ms
                + write_ms
                + verify_ms;
            println!(
                "{label},{},{estimate},{probe_ms:.3},{decode_ms:.3},{stealth_ms:.3},{prepare_ms:.3},{color_ms:.3},{encode_ms:.3},{mux_ms:.3},{write_ms:.3},{verify_ms:.3},{stage_sum:.3},{total_ms:.3}",
                file.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default(),
            );
            totals.probe += probe_ms;
            totals.decode += decode_ms;
            totals.stealth += stealth_ms;
            totals.prepare += prepare_ms;
            totals.color += color_ms;
            totals.encode += encode_ms;
            totals.mux += mux_ms;
            totals.write += write_ms;
            totals.verify += verify_ms;
            totals.total += total_ms;
            count += 1;
        }
        let body = totals.total - totals.stealth;
        eprintln!(
            "SUMMARY stealth={} dir={label} files={count} total_ms={:.3} stage_sum_ms={:.3} probe={:.3} decode={:.3} stealth={:.3} prepare={:.3} color={:.3} encode={:.3} mux={:.3} write={:.3} verify={:.3} stealth_share_of_total={:.2}% stealth_over_body={:.2}% estimate_total_bytes={estimate_total}",
            if stealth_on { "on" } else { "off" },
            totals.total,
            totals.sum(),
            totals.probe,
            totals.decode,
            totals.stealth,
            totals.prepare,
            totals.color,
            totals.encode,
            totals.mux,
            totals.write,
            totals.verify,
            if totals.total > 0.0 {
                totals.stealth / totals.total * 100.0
            } else {
                0.0
            },
            if body > 0.0 {
                totals.stealth / body * 100.0
            } else {
                0.0
            },
        );
    }
    std::fs::remove_dir_all(&scratch).ok();
}
