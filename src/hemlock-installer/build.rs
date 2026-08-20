fn main() {
    // Version from the single top-level VERSION file.
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let version_file = manifest_dir.join("../../VERSION");
    let version = std::fs::read_to_string(&version_file)
        .expect("read top-level VERSION file")
        .trim()
        .to_string();
    println!("cargo:rustc-env=HEMLOCK_VERSION={version}");
    println!("cargo:rerun-if-changed={}", version_file.display());
}
