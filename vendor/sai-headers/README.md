# Vendored SAI headers

API headers from [opencomputeproject/SAI](https://github.com/opencomputeproject/SAI)
(Apache-2.0), committed in-tree so `hemlock-sai --features real-sai` builds
hermetically. Each subdirectory is one pinned API version; `COMMIT` records
the exact upstream commit.

| Version | Why |
|---|---|
| `v1.7.1` | Helix4-era API matching the SONiC 202205/202211 `libsaibcm` builds (cel-e1031) |

The header version used by bindgen is a build-time selection: set
`HEMLOCK_SAI_HEADERS=<version-dir>` (default `v1.7.1`). New platforms that
pin a newer SAI add a new directory here rather than upgrading this one —
version pinning is per-platform, never global.
