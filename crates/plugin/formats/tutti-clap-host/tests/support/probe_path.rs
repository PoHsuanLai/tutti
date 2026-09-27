// Resolve the reference CLAP plugin (`tutti-clap-test-plugin`) that the
// conformance tests load.
//
// The rules — absence panics rather than skipping, and the *newest* candidate
// wins so a stale uplifted copy cannot shadow a fresh one — live in
// `tutti-fixture-resolve`, along with what each costs when it is broken.

use std::sync::OnceLock;

/// `;`-separated candidate paths, from `build.rs`.
const CANDIDATES: &str = env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES");

/// Absolute path to the freshest built copy of the reference plugin.
///
/// # Panics
///
/// If none of the candidates exists — deliberate; see above.
pub fn probe_path() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            tutti_fixture_resolve::resolve_or_panic(CANDIDATES, "tutti-clap-test-plugin")
        })
        .as_str()
}
