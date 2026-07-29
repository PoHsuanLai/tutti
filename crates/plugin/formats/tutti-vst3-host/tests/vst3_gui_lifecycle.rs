//! VST3 editor lifecycle conformance — the `IPlugView` checks the process-path
//! harness cannot reach.
//!
//! HostChecker wraps the plugin's own `IPlugView` and records how the host
//! drives it: `attached`/`removed` pairing, whether `onSize` arrives before
//! attach, whether it arrives synchronously during a `resizeView`, and which
//! optional entry points (`canResize`, `checkSizeConstraint`, `setFrame`,
//! `setContentScaleFactor`) the host uses at all. Two of those are
//! error-severity and describe host bugs:
//!
//! - `IPlugView::attached is called without removed first!`
//! - `IPlugView::removed is called without attached first!`
//!
//! Both are ordering mistakes that leak a native view per open — the kind of
//! thing that survives manual testing because the editor still appears.
//!
//! ## Why this is a separate test binary
//!
//! It needs a real X11/Cocoa window, so it is gated on a display being present
//! and skips cleanly without one. Keeping it out of `vst3_conformance` means
//! the main harness stays runnable headless.
//!
//! ## Running
//!
//! ```bash
//! VST3_SAMPLE_PLUGIN_DIR=/path/to/build/VST3/Release \
//! xvfb-run -a cargo test -p tutti-vst3-host --features conformance \
//!   --test vst3_gui_lifecycle -- --ignored
//! ```
//!
//! `#[ignore]` by default: a test that silently needs a display is worse than
//! one you opt into.

#![cfg(feature = "conformance")]

/// The harness build of each shared test: attach `#[test]`/`#[ignore]`.
///
/// The main-thread target defines this same macro to emit a plain function,
/// which is what lets one copy of the bodies serve both. The attributes cannot
/// simply be unconditional: rustc strips an `#[ignore]` function out of a
/// `harness = false` binary, so the runner could not call it.
macro_rules! gui_test {
    ($(#[$doc:meta])* fn $name:ident() $body:block) => {
        $(#[$doc])*
        #[test]
        #[ignore = "needs a display; run under xvfb-run -a"]
        fn $name() $body
    };
}

// The tests themselves live in `support/`, shared with the main-thread target.
// See that file's header for why.
include!("support/gui_lifecycle.rs");
