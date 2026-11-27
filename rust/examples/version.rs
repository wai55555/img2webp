fn main() {
    println!("libwebp {}", img2webp::webp_encode::libwebp_version());
    println!("deps {}", env!("IMG2WEBP_DEP_VERSIONS"));
    println!("features {}", env!("IMG2WEBP_FEATURES"));
}
