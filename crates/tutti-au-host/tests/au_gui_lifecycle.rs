//! AU Cocoa editor lifecycle — the `AuEditor` checks that need the main thread.
//!
//! What this suite proves: that the host's editor half keeps its side of the
//! AppKit contract. `AuEditor` retains a plugin-supplied `NSView` and hands the
//! raw pointer to the caller, so every promise it makes is one a host builds a
//! window from — `has_editor()` gating an Open Editor affordance,
//! `editor_size()` sizing the window, `view_ptr()` being parented, `close()`
//! and `Drop` balancing the retain. Each of those failing has a distinct
//! user-visible symptom; the per-test docs name them.
//!
//! ## Why almost everything here is `#[ignore]`
//!
//! These are Cocoa/AppKit calls, and macOS requires AppKit on the **process
//! main thread**. Cargo's harness runs every `#[test]` on a worker thread with
//! no way to ask for the main one, so running them here would be undefined
//! behaviour rather than a test — and `AuEditor` calls
//! `tutti_plugin_types::assert_main_thread()`, which fires once a main thread
//! has been marked.
//!
//! So the real run happens in `au_gui_lifecycle_main`, a `harness = false`
//! binary that owns `main()` and therefore *is* the main thread. The bodies
//! live in `support/gui_lifecycle.rs`, shared with this target so the two
//! cannot drift. This target exists so `cargo test` still lists them, and so
//! the shared file keeps being compiled under the ordinary harness.
//!
//! ## Running
//!
//! ```bash
//! # The real run — on the main thread, with the affinity assert armed:
//! cargo test -p tutti-au-host \
//!   --test au_gui_lifecycle_main
//! ```
//!
//! No env vars, no SDK, no display: the corpus is part of macOS, and
//! `AuEditor::open(unit, None)` instantiates a view without attaching it to a
//! window. A missing AU **fails** rather than skipping — see
//! `support/corpus.rs`.

#![cfg(target_os = "macos")]

/// The harness build of each shared test: attach `#[test]`/`#[ignore]`, and
/// refuse to run the body off the main thread.
/// `au_gui_lifecycle_main.rs` defines the same macro to emit a plain function.
///
/// The `#[ignore]` alone is only advisory — `cargo test -- --include-ignored`
/// overrides it and runs the body on a cargo worker thread, where the first
/// AppKit call raises an Objective-C exception that unwinds into Rust and
/// aborts the process (`fatal runtime error: Rust cannot catch foreign
/// exceptions`, SIGABRT). That killed the whole test binary, so the documented
/// "run everything" command could not be used on this crate at all.
///
/// The guard turns that abort into a skip with a message pointing at the target
/// that *can* run these. It is a runtime check because there is no attribute
/// for "this test must own the process main thread".
macro_rules! gui_test {
    ($(#[$doc:meta])* fn $name:ident() $body:block) => {
        $(#[$doc])*
        #[test]
        #[ignore = "AppKit requires the main thread; run --test au_gui_lifecycle_main"]
        fn $name() {
            if !is_main_thread() {
                eprintln!(
                    "{}: skipped — AppKit requires the process main thread and \
                     this is a cargo worker. Run `--test au_gui_lifecycle_main`.",
                    stringify!($name)
                );
                return;
            }
            $body
        }
    };
}

/// Whether the caller owns the process main thread.
///
/// `pthread_main_np` is the only way to ask: `assert_main_thread` cannot answer,
/// because it compares against a thread *marked* by `mark_main_thread`, which no
/// harness target calls — so it is a no-op here by construction.
fn is_main_thread() -> bool {
    // SAFETY: `pthread_main_np` takes no arguments, reads no memory, and is
    // available on every macOS this crate compiles for.
    unsafe extern "C" {
        fn pthread_main_np() -> std::os::raw::c_int;
    }
    unsafe { pthread_main_np() == 1 }
}

mod support;

// The tests themselves live in `support/`, shared with the main-thread target.
// See that file's header for why.
include!("support/gui_lifecycle.rs");
