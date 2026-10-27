fn main() {
    println!("cargo:rerun-if-changed=assets/sidetone.ico");
    println!("cargo:rerun-if-changed=noise-cancellation/native/src/lib.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if std::env::var_os("CARGO_FEATURE_NOISE_CANCELLATION").is_some() {
            let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
            let status = std::process::Command::new(std::env::var_os("RUSTC").unwrap())
                .args([
                    "--crate-name",
                    "sidetone_noise_cancellation",
                    "--crate-type",
                    "cdylib",
                    "--edition=2024",
                    "--target",
                    &std::env::var("TARGET").unwrap(),
                    "-C",
                    "opt-level=2",
                    "-C",
                    "target-feature=+crt-static",
                    "-C",
                    "debuginfo=0",
                    "noise-cancellation/native/src/lib.rs",
                    "-o",
                ])
                .arg(output.join("sidetone-noise-cancellation.node"))
                .status()
                .expect("compile embedded Noise cancellation addon");
            assert!(
                status.success(),
                "Noise cancellation addon compilation failed"
            );
        }
        winres::WindowsResource::new()
            .set_icon("assets/sidetone.ico")
            .set("ProductName", "Sidetone")
            .set("FileDescription", "Sidetone audio monitor")
            .compile()
            .expect("compile Windows icon resource");
    }
}
