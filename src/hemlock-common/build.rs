fn main() {
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
        .compile_protos(&protos, &["proto"])
        .expect("compile hemlock protos");
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
}
