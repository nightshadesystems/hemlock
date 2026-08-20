//! Generates SAI FFI bindings when the `real-sai` feature is enabled.
//!
//! The header set is a build-time selection so each platform image can pin
//! its own SAI API era: HEMLOCK_SAI_HEADERS names a directory under
//! vendor/sai-headers/ (default v1.7.1, the Helix4-era API).

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=HEMLOCK_SAI_HEADERS");
    #[cfg(feature = "real-sai")]
    generate_bindings();
}

#[cfg(feature = "real-sai")]
fn generate_bindings() {
    use std::path::PathBuf;

    let version =
        std::env::var("HEMLOCK_SAI_HEADERS").unwrap_or_else(|_| String::from("v1.7.1"));
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let inc = manifest_dir
        .join("../../vendor/sai-headers")
        .join(&version)
        .join("inc");
    assert!(
        inc.is_dir(),
        "SAI headers not found at {} (HEMLOCK_SAI_HEADERS={version})",
        inc.display()
    );
    println!("cargo:rerun-if-changed={}", inc.display());

    let bindings = bindgen::Builder::default()
        .header(manifest_dir.join("wrapper.h").display().to_string())
        .clang_arg(format!("-I{}", inc.display()))
        .allowlist_function("sai_.*")
        .allowlist_type("sai_.*")
        .allowlist_var("SAI_.*")
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .layout_tests(false)
        .generate()
        .expect("bindgen over SAI headers");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    bindings
        .write_to_file(out.join("sai_bindings.rs"))
        .expect("write sai_bindings.rs");
}
