fn main() {
    // The BLAS backend's macOS seam calls Accelerate (cblas_sgemm + vForce). Link the framework
    // only when that backend is actually compiled: feature on AND target is macOS. A default build
    // (feature off) or a Linux build links nothing here.
    let blas = std::env::var("CARGO_FEATURE_BLAS").is_ok();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if blas && target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=Accelerate");
    }
    // Linux backend (when its seam is implemented): link the BLAS here, e.g.
    //   if blas && target_os == "linux" { println!("cargo:rustc-link-lib=openblas"); }
}
