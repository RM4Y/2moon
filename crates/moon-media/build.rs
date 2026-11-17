// Direct NVENC encoder shim (csrc/nvenc_shim.c, Linux only): only the vendored
// MIT-licensed nvEncodeAPI.h is needed; the NVIDIA libraries are loaded at runtime.
fn main() {
    println!("cargo:rerun-if-changed=csrc/nvenc_shim.c");
    println!("cargo:rerun-if-changed=csrc/nvEncodeAPI.h");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        cc::Build::new().file("csrc/nvenc_shim.c").include("csrc").flag_if_supported("-std=c11").warnings(true).compile("moon_nvenc");
        println!("cargo:rustc-link-lib=dl");
    }
}
