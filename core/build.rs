fn main() {
    println!("cargo:rustc-env=WORKSPACE_DIR={}", std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-env=CARGO_PROFILE={}", std::env::var("PROFILE").unwrap());
}
