fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let profile = std::path::Path::new(&out_dir)
        .iter()
        .skip_while(|c| *c != std::ffi::OsStr::new("target"))
        .nth(1)
        .and_then(|s| s.to_str())
        .unwrap_or(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }).to_string();

    println!("cargo:rustc-env=WORKSPACE_DIR={}", std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-env=CARGO_PROFILE={}", profile);
}
