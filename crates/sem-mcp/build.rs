fn main() {
    // libgit2-sys requires advapi32 on Windows for registry/security APIs
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=dylib=advapi32");
    }
}
