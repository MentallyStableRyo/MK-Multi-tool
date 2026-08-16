fn main() {
    // Expose the compilation target triple to the crate so the sidecar resolver can
    // find `binaries/<name>-<triple>` layouts during `tauri dev`.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=MK_TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-changed=build.rs");

    tauri_build::build();
}
