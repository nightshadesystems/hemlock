//! Generates SAI FFI bindings when the `real-sai` feature is enabled.
//!
//! The header set is a build-time selection so each platform image can pin
//! its own SAI API era: HEMLOCK_SAI_HEADERS names a directory under
//! vendor/sai-headers/ (default v1.11.0, matching the SONiC 202305-era libsaibcm 8.4.x builds).

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=HEMLOCK_SAI_HEADERS");
    // Copper cable diagnostics arrived in SAI 1.13; the pinned 1.11
    // headers do not declare the attributes, and their numeric ids are
    // not ours to guess. The vendor backend compiles the real
    // implementation only where the selected header set defines them,
    // and reports the capability absent otherwise — so pinning newer
    // headers via HEMLOCK_SAI_HEADERS lights the feature up with no
    // other change.
    println!("cargo:rustc-check-cfg=cfg(sai_cable_diag)");
    #[cfg(feature = "real-sai")]
    generate_bindings();
    #[cfg(feature = "openbcm")]
    build_openbcm_stub();
}

/// Build the test stub shim into a shared library under OUT_DIR.
///
/// The real `libhemlockbcm.so` is built inside a Broadcom OpenBCM tree
/// with an ARM cross toolchain, so CI can never produce one. The stub
/// implements the same ABI over a fake switch, and the `openbcm` module's
/// tests dlopen it — which is what keeps the Rust half of the boundary
/// (version handshake, vtable marshalling, NULL slots) honest without
/// hardware. Its path is handed to the tests as HEMLOCK_OPENBCM_STUB.
///
/// Note this is a *cdylib built by hand*: `cc` builds static archives, so
/// the objects are compiled with it and then linked into a shared object
/// by the same compiler driver.
#[cfg(feature = "openbcm")]
fn build_openbcm_stub() {
    use std::path::PathBuf;

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let shim_dir = manifest_dir.join("openbcm-shim");
    let source = shim_dir.join("hemlockbcm_stub.c");
    let header = shim_dir.join("hemlockbcm.h");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed={}", header.display());

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let lib_name = if cfg!(windows) {
        "hemlockbcm_stub.dll"
    } else if cfg!(target_os = "macos") {
        "libhemlockbcm_stub.dylib"
    } else {
        "libhemlockbcm_stub.so"
    };
    let lib_path = out.join(lib_name);

    let mut build = cc::Build::new();
    build.file(&source).include(&shim_dir).warnings(true);
    let compiler = build
        .try_get_compiler()
        .expect("a C compiler for the openbcm test stub");

    let mut command = compiler.to_command();
    if compiler.is_like_msvc() {
        command
            .arg("/LD")
            .arg(&source)
            .arg(format!("/I{}", shim_dir.display()))
            .arg(format!("/Fe{}", lib_path.display()))
            .arg(format!("/Fo{}\\", out.display()));
    } else {
        command
            .arg("-shared")
            .arg("-fPIC")
            .arg("-I")
            .arg(&shim_dir)
            .arg(&source)
            .arg("-o")
            .arg(&lib_path);
    }
    let status = command
        .status()
        .expect("spawning the C compiler for the openbcm test stub");
    assert!(status.success(), "building the openbcm test stub failed");

    println!(
        "cargo:rustc-env=HEMLOCK_OPENBCM_STUB={}",
        lib_path.display()
    );
}

#[cfg(feature = "real-sai")]
fn generate_bindings() {
    use std::path::PathBuf;

    let version = std::env::var("HEMLOCK_SAI_HEADERS").unwrap_or_else(|_| String::from("v1.11.0"));
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let headers_root = manifest_dir.join("../../vendor/sai-headers").join(&version);
    let inc = headers_root.join("inc");
    assert!(
        inc.is_dir(),
        "SAI headers not found at {} (HEMLOCK_SAI_HEADERS={version})",
        inc.display()
    );
    println!("cargo:rerun-if-changed={}", inc.display());

    // When cross-targeting (see --target below), libclang does not add its
    // own builtin headers (stdint.h, stddef.h, ...); locate them via clang.
    let builtin_include = clang_builtin_include();

    let mut builder = bindgen::Builder::default();
    if let Some(dir) = &builtin_include {
        builder = builder.clang_arg(format!("-isystem{dir}"));
    }
    let bindings = builder
        .header(manifest_dir.join("wrapper.h").display().to_string())
        .clang_arg(format!("-I{}", inc.display()))
        // saiextensions.h pulls in the experimental headers.
        .clang_arg(format!("-I{}", headers_root.join("experimental").display()))
        // libsai is a Linux library; generate Linux-ABI bindings regardless
        // of the build host so output is identical everywhere.
        .clang_arg("--target=x86_64-unknown-linux-gnu")
        // Shim legacy <sys/types.h>; see compat/sys/types.h.
        .clang_arg(format!("-I{}", manifest_dir.join("compat").display()))
        .allowlist_function("sai_.*")
        .allowlist_type("sai_.*")
        .allowlist_var("SAI_.*")
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .layout_tests(false)
        .generate()
        .expect("bindgen over SAI headers");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let path = out.join("sai_bindings.rs");
    bindings
        .write_to_file(&path)
        .expect("write sai_bindings.rs");

    // See main(): the cable-diagnostics implementation is compiled only
    // against headers that declare both attributes.
    let generated = std::fs::read_to_string(&path).expect("read sai_bindings.rs");
    if generated.contains("SAI_PORT_ATTR_CABLE_PAIR_STATE")
        && generated.contains("SAI_PORT_ATTR_CABLE_PAIR_LENGTH")
    {
        println!("cargo:rustc-cfg=sai_cable_diag");
    }
}

#[cfg(feature = "real-sai")]
fn clang_builtin_include() -> Option<String> {
    // Prefer a clang next to libclang (LIBCLANG_PATH), then PATH.
    let candidates: Vec<std::path::PathBuf> = std::env::var_os("LIBCLANG_PATH")
        .map(|dir| {
            let dir = std::path::PathBuf::from(dir);
            vec![dir.join("clang.exe"), dir.join("clang")]
        })
        .unwrap_or_default()
        .into_iter()
        .chain([std::path::PathBuf::from("clang")])
        .collect();
    for clang in candidates {
        let Ok(output) = std::process::Command::new(&clang)
            .arg("--print-resource-dir")
            .output()
        else {
            continue;
        };
        if output.status.success() {
            let resource_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let include = std::path::Path::new(&resource_dir).join("include");
            if include.is_dir() {
                return Some(include.display().to_string());
            }
        }
    }
    None
}
