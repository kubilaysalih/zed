fn main() {
    // RustEmbed tracks existing files, but new bundled assets also need to invalidate its cache.
    println!("cargo:rerun-if-changed=../../assets");
}
