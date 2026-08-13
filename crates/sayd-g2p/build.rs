// espeak-ng comes from the nix store; link against it rather than letting a
// -sys crate compile it from source.
fn main() {
    if let Ok(dir) = std::env::var("ESPEAK_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=espeak-ng");
    println!("cargo:rerun-if-env-changed=ESPEAK_LIB_DIR");
}
