//! Generates SAI FFI bindings when the `real-sai` feature is enabled.
//!
//! The header set is a build-time selection so each platform image can pin
//! its own SAI API era: HEMLOCK_SAI_HEADERS names a directory under
//! vendor/sai-headers/ (default v1.11.0, matching the SONiC 202305-era libsaibcm 8.4.x builds).

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=HEMLOCK_SAI_HEADERS");
    #[cfg(feature = "real-sai")]
    generate_bindings();
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
    bindings
        .write_to_file(out.join("sai_bindings.rs"))
        .expect("write sai_bindings.rs");
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
