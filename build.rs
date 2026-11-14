//! Build-time only; nothing here runs in the app.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // ONNX Runtime's prebuilt Core ML code checks the macOS version with `@available`,
    // which calls into clang's runtime library. rustc links with -nodefaultlibs, so that
    // library has to be named explicitly.
    let macos = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos");
    if macos && std::env::var_os("CARGO_FEATURE_COREML").is_some() {
        let out = std::process::Command::new("clang")
            .arg("--print-resource-dir")
            .output()
            .expect("clang (from the Xcode Command Line Tools) is needed to build for macOS");
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        println!("cargo:rustc-link-search=native={dir}/lib/darwin");
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
    }
}
