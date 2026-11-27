//! img2webp: convert PNG/GIF (and optionally JPEG) images to WebP.

use img2webp::dispatch;
use img2webp::interrupt;

fn main() {
    let options = dispatch::Options::default();
    // The handler is installed as early as possible; it only sets a flag.
    let _ = interrupt::install_handler(&options.interrupt);
    let code = dispatch::run(options);
    std::process::exit(code);
}
