//! Shared by the suites that run on both graph backends (design doc 013,
//! Phase 3 PR 11: "both backends run the same suites").
//!
//! A test that takes the backend as a parameter,
//!
//! ```ignore
//! fn a_declared_source_reaches_the_graph(backend: GraphBackend) { … }
//! both_backends!(a_declared_source_reaches_the_graph);
//! ```
//!
//! becomes two tests, `a_declared_source_reaches_the_graph::net` and
//! `…::native`, each failing on its own. The body builds its graph with
//! `AudioGraphRes::headless_with(backend, …)` / `unattached_with`, and nothing
//! else changes: the suite is the same suite on either runtime.
//!
//! A test that runs on one backend only is a plain `#[test]` and says why.

/// One `#[test]` per graph backend for `fn $name(backend: GraphBackend)`: a
/// module `$name` holding `net` and `native`. A function and a module may
/// share a name (they live in different namespaces), so the test keeps its
/// own. Attributes on the invocation (a `#[cfg]`) apply to the module.
#[allow(unused_macros)]
macro_rules! both_backends {
    ($name:ident) => {
        mod $name {
            #[test]
            fn net() {
                super::$name(bevy_tutti::graph::GraphBackend::Net)
            }

            #[test]
            fn native() {
                super::$name(bevy_tutti::graph::GraphBackend::Native)
            }
        }
    };
}

/// The reference CLAP plugin and its server, for suites that load one. Not
/// every suite that pulls `common` in does, hence the `dead_code` allowance.
#[cfg(feature = "plugin")]
#[allow(dead_code)]
pub mod plugin;
