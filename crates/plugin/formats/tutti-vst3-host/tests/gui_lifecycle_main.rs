//! The `vst3_gui_lifecycle` tests, run on the **main thread**.
//!
//! ## Why this exists
//!
//! These tests need a real native window, and macOS requires Cocoa's event
//! loop on the process main thread. Cargo's harness runs every `#[test]` on a
//! worker thread with no way to ask for the main one, so there the macOS arm of
//! `build_event_loop` can only skip — X11, Wayland and Windows escape via
//! `with_any_thread`, macOS has no equivalent.
//!
//! So this target sets `harness = false`: it owns `main()`, which *is* the main
//! thread, and calls the test functions directly. On macOS that turns 8 skips
//! into 8 real assertions. The bodies live in `support/gui_lifecycle.rs`, shared
//! with `vst3_gui_lifecycle.rs`, so the two cannot drift.
//!
//! Not macOS-only: the main thread is where a real host drives an editor, so
//! this is the more faithful configuration everywhere. The harness target stays
//! because plain `cargo test` finds it with no extra arguments.
//!
//! ## Running
//!
//! ```bash
//! VST3_SAMPLE_PLUGIN_DIR=/path/to/build/VST3/Release \
//! cargo test -p tutti-vst3-host --features conformance --test gui_lifecycle_main
//! ```
//!
//! On Linux, wrap it in `xvfb-run -a` as with the harness version. No
//! `-- --ignored` — the `#[ignore]` attributes belong to the other target, and
//! this one runs everything it is given.
//!
//! Skips (no display, plugin not built) are reported as skips and do not fail
//! the run; only a real assertion failure or a panic does.

#![cfg(feature = "conformance")]

/// The main-thread build of each shared test: a plain function, so `main` can
/// call it. `vst3_gui_lifecycle.rs` defines the same macro with `#[test]`.
macro_rules! gui_test {
    ($(#[$doc:meta])* fn $name:ident() $body:block) => {
        $(#[$doc])*
        fn $name() $body
    };
}

// The tests themselves, plus their helpers — the same file the default-harness
// target includes, so the two can never drift.
include!("support/gui_lifecycle.rs");

/// Run one test, catching a panic so the remaining tests still run.
///
/// A failure here is a genuine assertion failure: the skip paths inside the
/// tests return early and print their reason rather than panicking.
fn run(name: &str, f: impl FnOnce() + std::panic::UnwindSafe) -> bool {
    eprintln!("\n── {name} ──");
    match std::panic::catch_unwind(f) {
        Ok(()) => {
            eprintln!("   ok");
            true
        }
        Err(_) => {
            eprintln!("   FAILED");
            false
        }
    }
}

fn main() {
    // The window is a process-lifetime singleton built on first use; touching
    // it here means the whole run shares one, and that it is created on this
    // thread — the point of the whole target.
    if TestWindow::get().is_none() {
        eprintln!(
            "no usable window on this platform/display; every GUI test would \
             skip. Nothing to do."
        );
        return;
    }

    let mut failed = Vec::new();
    let mut total = 0;

    macro_rules! cases {
        ($($t:path),* $(,)?) => {
            $(
                total += 1;
                if !run(stringify!($t), || $t()) {
                    failed.push(stringify!($t));
                }
            )*
        };
    }

    cases!(
        editor_open_close_pairs_attach_and_remove,
        closing_an_unopened_editor_is_a_noop,
        editor_resize_respects_capabilities,
        editorless_plugin_is_refused_cleanly,
        supported_platform_type_still_opens,
        open_editor_run_loop_is_pumped,
        has_editor_agrees_with_opening_one,
        a_closed_editor_reports_no_resize_request,
    );

    eprintln!("\n{}/{} passed", total - failed.len(), total);
    if !failed.is_empty() {
        eprintln!("failed: {}", failed.join(", "));
        std::process::exit(1);
    }
}
