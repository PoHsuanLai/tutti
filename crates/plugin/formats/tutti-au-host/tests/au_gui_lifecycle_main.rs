//! The `au_gui_lifecycle` tests, run on the **main thread**.
//!
//! ## Why this exists
//!
//! `AuEditor::open`/`close`/`editor_size` are Cocoa/AppKit calls, and macOS
//! requires AppKit on the process main thread. Cargo's harness runs every
//! `#[test]` on a worker thread with no way to request the main one, so under
//! that harness these tests are `#[ignore]`d — driving an AU editor there would
//! be undefined behaviour, not a test.
//!
//! This target sets `harness = false`: it owns `main()`, which *is* the main
//! thread, and calls the test functions directly. The bodies live in
//! `support/gui_lifecycle.rs`, shared with `au_gui_lifecycle.rs`, so the two
//! cannot drift.
//!
//! `main` also calls [`tutti_plugin_types::mark_main_thread`] before anything
//! else, which **arms** the affinity guard: `AuEditor` calls
//! `assert_main_thread()` on `open` and `close`, and that is a `debug_assert`
//! which is a no-op until a main thread has been marked. Without the mark this
//! suite would exercise the editor without proving anything about thread
//! correctness; with it, any call that strays off this thread is a located
//! panic.
//!
//! ## Running
//!
//! ```bash
//! cargo test --manifest-path crates/bevy-tutti/Cargo.toml -p tutti-au-host \
//!   --test au_gui_lifecycle_main
//! ```
//!
//! No env vars, no SDK, no display: the corpus ships with macOS, and
//! `AuEditor::open(unit, None)` instantiates the plugin's view without
//! attaching it to a window hierarchy. A missing AU is a hard failure, not a
//! skip — see `support/corpus.rs`.

#![cfg(target_os = "macos")]

/// The main-thread build of each shared test: a plain function, so `main` can
/// call it. `au_gui_lifecycle.rs` defines the same macro with `#[test]`.
///
/// The macro has to wrap the *whole* function rather than emit attributes
/// beside it: rustc strips `#[test]`/`#[ignore]` out of a `harness = false`
/// binary, so a function defined with them would not exist for `main` to call.
macro_rules! gui_test {
    ($(#[$doc:meta])* fn $name:ident() $body:block) => {
        $(#[$doc])*
        fn $name() $body
    };
}

mod support;

// The tests themselves, plus their helpers — the same file the default-harness
// target includes, so the two can never drift.
include!("support/gui_lifecycle.rs");

/// Run one test, catching a panic so the remaining tests still run.
///
/// Without this the first failure would hide every later one, and these tests
/// each cover a different `AuEditor` promise — knowing which subset broke is
/// most of the diagnostic.
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
    // Arm the main-thread affinity guard, on the thread that actually is the
    // main one. `AuEditor::open`/`close` call `assert_main_thread()`, which is
    // a no-op until this runs — so without it the suite would drive the editor
    // without proving anything about thread correctness. Must be first: a mark
    // set after the first `open` would leave that call unchecked.
    tutti_plugin_types::mark_main_thread();

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
        has_editor_agrees_with_opening_one,
        open_size_close_is_idempotent_and_reports_zero_after,
        view_ptr_is_non_null_only_while_open,
        repeated_open_close_cycles_stay_balanced,
        an_au_without_a_cocoa_view_is_refused_cleanly,
        dropping_without_closing_is_safe,
        two_editors_on_one_unit_are_independent,
    );

    eprintln!("\n{}/{} passed", total - failed.len(), total);
    if !failed.is_empty() {
        eprintln!("failed: {}", failed.join(", "));
        std::process::exit(1);
    }
}
