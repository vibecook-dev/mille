//! mille-binding — napi-rs surface for mille-core.
//!
//! Per SPEC §4.1, this crate contains only marshaling. No engine logic.
//! Phase 5 fleshes out the `FileExplorer` `#[napi]` class and related
//! `#[napi(object)]` mirror types.

#[macro_use]
extern crate napi_derive;

pub mod bulk;
pub mod cancel;
pub mod error;
pub mod events;
pub mod explorer;
// The git engine (SPEC mille-git §18) — absent from the default build, so
// FileTree-only embedders never link gix.
#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "git")]
pub mod git_types;
pub mod io;
pub mod journal;
pub mod mutations;
pub mod snapshot;
pub mod stream;
pub mod types;
mod watch_runtime;

/// Module version string. Present so the native module has at least one
/// exported symbol and can be `require()`d end-to-end during Phase 0.
#[napi(catch_unwind)]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Runtime identity for the native artifact that Node actually loaded.
///
/// The npm package and Rust crate are versioned independently today, so a
/// version string alone cannot tell a benchmark whether it is exercising a
/// fresh local debug build or a published release binary. Keep this payload
/// deliberately small and stable so test/benchmark reports can always record
/// the implementation under test.
#[napi(object)]
pub struct NativeBuildInfo {
    pub crate_version: String,
    pub profile: String,
    pub target: String,
    /// `"unwind"` or `"abort"`. A binary built with `panic = "abort"` cannot
    /// convert a Rust panic into a JS exception under any circumstances: the
    /// process takes SIGABRT and the embedding app dies with it. Exposed so a
    /// test can assert this about the artifact that actually ships, which is
    /// not the artifact the test suite normally loads.
    pub panic_strategy: String,
    /// Optional cargo features compiled in (`"git"`, `"panic-probe"`). Lets a
    /// test or a host tell a git-enabled `.node` from the default one without
    /// probing for classes — `isNativeGitAvailable()` must agree with it.
    pub features: Vec<String>,
}

#[napi(js_name = "buildInfo", catch_unwind)]
pub fn build_info() -> NativeBuildInfo {
    NativeBuildInfo {
        crate_version: env!("CARGO_PKG_VERSION").to_string(),
        profile: if cfg!(debug_assertions) {
            "debug".to_string()
        } else {
            "release".to_string()
        },
        target: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        panic_strategy: if cfg!(panic = "abort") {
            "abort".to_string()
        } else {
            "unwind".to_string()
        },
        features: [
            (cfg!(feature = "git"), "git"),
            (cfg!(feature = "panic-probe"), "panic-probe"),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| name.to_string())
        .collect(),
    }
}

/// Deliberate panic behind a sync entry point. Feature-gated: this exists so
/// `panic-boundary.test.mjs` can prove a panic reaches JS as a catchable
/// error rather than aborting. Enabled only by `build:napi:probe`.
#[cfg(feature = "panic-probe")]
#[napi(js_name = "__panicProbeSync", catch_unwind)]
pub fn panic_probe_sync() -> u32 {
    panic!("probe: synchronous panic across the napi boundary");
}

/// Same, from inside an async entry point. Worth probing separately: the
/// future runs on the tokio pool, so the panic unwinds through the runtime's
/// task machinery rather than straight out of the `extern "C"` shim.
#[cfg(feature = "panic-probe")]
#[napi(js_name = "__panicProbeAsync", catch_unwind)]
pub async fn panic_probe_async() -> napi::Result<u32> {
    panic!("probe: asynchronous panic across the napi boundary");
}
