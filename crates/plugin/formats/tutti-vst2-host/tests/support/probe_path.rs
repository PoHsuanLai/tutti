// Resolve the reference VST2 plugin (`tutti-vst2-test-plugin`) that the
// conformance tests load.
//
// The rules — absence panics rather than skipping, and the *newest* candidate
// wins so a stale uplifted copy cannot shadow a fresh one — live in
// `tutti-fixture-resolve`, along with what each cost when it was broken. They
// used to be restated in three near-identical copies of this file.

use std::path::PathBuf;
use std::sync::OnceLock;

/// `;`-separated candidate paths, from `build.rs`.
const CANDIDATES: &str = env!("TUTTI_VST2_PROBE_CANDIDATES");

/// Absolute path to the freshest built copy of the reference plugin.
///
/// Returns `&'static PathBuf` — it derefs to `&Path` for `Vst2Instance::load`
/// and is accepted by `Path::new`, which is what the call sites here want. The
/// CLAP side returns `&str`, which is what `libloading` wants.
///
/// # Panics
///
/// If none of the candidates exists — deliberate; see above.
pub fn probe_path() -> &'static PathBuf {
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        PathBuf::from(tutti_fixture_resolve::resolve_or_panic(
            CANDIDATES,
            "tutti-vst2-test-plugin",
        ))
    })
}
