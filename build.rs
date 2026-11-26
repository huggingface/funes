fn main() {
    // The BLAS backend's macOS seam calls Accelerate (cblas_sgemm + vForce). Link the framework
    // only when that backend is actually compiled: feature on AND target is macOS. A default build
    // (feature off) links nothing here; the Linux seam is pure Rust (faer) and needs no link.
    let blas = std::env::var("CARGO_FEATURE_BLAS").is_ok();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if blas && target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=Accelerate");
    }
    // A command's future runs on the main thread, and an unoptimized `funes index` overflows the
    // 1 MiB Windows gives it where Linux and macOS give 8; an optimized one fits, by a margin
    // nothing promises. The stack is reserved in the executable — funes's own and the benchmarks'
    // — since RUST_MIN_STACK reaches spawned threads only.
    if target_os == "windows" {
        let reserve = 16 * 1024 * 1024;
        let arg = match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            Ok("msvc") => format!("/STACK:{reserve}"),
            _ => format!("-Wl,--stack,{reserve}"),
        };
        println!("cargo:rustc-link-arg-bins={arg}");
        println!("cargo:rustc-link-arg-examples={arg}");
    }
}
