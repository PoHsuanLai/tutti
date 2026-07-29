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
//! cargo test --manifest-path crates/bevy-tutti/Cargo.toml -p tutti-au-host \
//!   --test au_gui_lifecycle_main
//! ```
//!
//! No env vars, no SDK, no display: the corpus is part of macOS, and
//! `AuEditor::open(unit, None)` instantiates a view without attaching it to a
//! window. A missing AU **fails** rather than skipping — see
//! `support/corpus.rs`.

#![cfg(target_os = "macos")]

/// The harness build of each shared test: attach `#[test]`/`#[ignore]`.
/// `au_gui_lifecycle_main.rs` defines the same macro to emit a plain function.
macro_rules! gui_test {
    ($(#[$doc:meta])* fn $name:ident() $body:block) => {
        $(#[$doc])*
        #[test]
        #[ignore = "AppKit requires the main thread; run --test au_gui_lifecycle_main"]
        fn $name() $body
    };
}

mod support;

// The tests themselves live in `support/`, shared with the main-thread target.
// See that file's header for why.
include!("support/gui_lifecycle.rs");
