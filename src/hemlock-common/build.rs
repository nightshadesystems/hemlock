fn main() {
    // Single source of truth for the product version: the top-level VERSION
    // file (also consumed by build/mkimage.sh for image names).
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let version_file = manifest_dir.join("../../VERSION");
    let version = std::fs::read_to_string(&version_file)
        .expect("read top-level VERSION file")
        .trim()
        .to_string();
    assert!(!version.is_empty(), "VERSION file is empty");
    println!("cargo:rustc-env=HEMLOCK_VERSION={version}");
    println!("cargo:rerun-if-changed={}", version_file.display());

    // No system protoc required: use the vendored binary.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);

    let protos = [
        "proto/hemlock/v1/common.proto",
        "proto/hemlock/v1/syncd.proto",
        "proto/hemlock/v1/pmon.proto",
        "proto/hemlock/v1/mgmtd.proto",
        "proto/hemlock/v1/orch.proto",
    ];
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        // Every message is serializable, so a state dump (the
        // tech-support bundle) can write any RPC response out as JSON
        // without a hand-written mirror of each one going stale.
        .type_attribute(".", "#[derive(serde::Serialize)]")
        .compile_protos(&protos, &["proto"])
        .expect("compile hemlock protos");
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
}
