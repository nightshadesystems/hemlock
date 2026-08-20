//! Platform manifests: the "platform = data, not code" layer.
//!
//! A platform is a directory under `platforms/` holding a `platform.toml`
//! manifest plus vendor data files. This crate defines the manifest schema,
//! loads and validates it, and provides the [`quirks::PlatformQuirks`] escape
//! hatch for boards that need custom CPLD/LED behavior.
