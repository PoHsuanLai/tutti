//! The AU Cocoa editor-lifecycle tests, run on the **main thread**.
//!
//! `AuEditor::open`/`close`/`editor_size` are Cocoa/AppKit calls, and macOS
//! requires AppKit on the process main thread. Cargo's harness runs every
//! `#[test]` on a worker thread with no way to request the main one, so this
//! target sets `harness = false`: it owns `main()`, which *is* the main thread,
//! and calls the test functions directly. The bodies live in
//! `support/gui_lifecycle.rs`.
//!
//! ## The protocol
//!
//! `main` answers the command line a libtest binary answers, so nextest runs it
//! like any other: `--list --format terse` prints one `<case>: test` line per
//! case and runs nothing (`--ignored` lists nothing: no case is ignored), and
//! `<case> --exact` runs that case alone, exiting non-zero when it panics. With
//! no case named every case runs, which is what `cargo test` does. The command
//! line is read before any window or plugin work. A missing plugin is a panic
//! in the case, never a skip.
//!
//! `AuEditor` asks the OS whether it is on the main thread (`pthread_main_np`)
//! on `open` and `close`, so no token has to be armed here: running on the
//! process main thread is what makes the OS check pass, and any call that
//! strays off it is refused or skipped by the editor itself.
//!
//! ## Running
//!
//! ```bash
//! cargo nextest run -p tutti-au-host --test au_gui_lifecycle_main
//! ```
//!
//! No env vars, no SDK, no display: the corpus ships with macOS, and
//! `AuEditor::open(unit, None)` instantiates the plugin's view without
//! attaching it to a window hierarchy. A missing AU is a hard failure, not a
//! skip — see `support/corpus.rs`.

// NOTE: the macOS gate is `#[cfg]` on the *items* below, deliberately not `#![cfg]` on the file. This target is
// `harness = false`, so cargo links it as a plain binary and requires a `main`
// on **every** platform — a file-level cfg deletes `main` along with everything
// else, and the target then fails to compile elsewhere. `main` is unconditional and lists and runs no case there.

/// The main-thread build of each shared test: a plain function, so `main` can
/// call it.
///
/// The macro has to wrap the *whole* function rather than emit attributes
/// beside it: rustc strips `#[test]`/`#[ignore]` out of a `harness = false`
/// binary, so a function defined with them would not exist for `main` to call.
#[cfg(target_os = "macos")]
macro_rules! gui_test {
    ($(#[$doc:meta])* fn $name:ident() $body:block) => {
        $(#[$doc])*
        fn $name() $body
    };
}

#[cfg(target_os = "macos")]
mod support;

// The tests themselves, plus their helpers.
#[cfg(target_os = "macos")]
include!("support/gui_lifecycle.rs");

/// What the command line asks of this binary.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// Print the case names and run nothing.
    List { ignored: bool },
    /// Run the cases whose name matches one of `filters` (all, with none), by
    /// equality with `--exact` and by substring otherwise.
    Run {
        filters: Vec<String>,
        exact: bool,
        ignored: bool,
    },
}

/// Reads the libtest command line: the flags that take a value, and the
/// positional case names.
fn parse(args: &[String]) -> Request {
    let (mut list, mut exact, mut ignored) = (false, false, false);
    let mut filters = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => list = true,
            "--exact" => exact = true,
            "--ignored" => ignored = true,
            "--format" | "--test-threads" | "--color" | "--skip" | "--logfile" => {
                args.next();
            }
            flag if flag.starts_with('-') => {}
            name => filters.push(name.to_owned()),
        }
    }
    if list {
        Request::List { ignored }
    } else {
        Request::Run {
            filters,
            exact,
            ignored,
        }
    }
}

/// Whether `name` is selected by `filters`.
fn selected(name: &str, filters: &[String], exact: bool) -> bool {
    filters.is_empty()
        || filters.iter().any(|f| {
            if exact {
                name == f
            } else {
                name.contains(f.as_str())
            }
        })
}

/// The cases, each by the name it is listed under.
#[cfg(target_os = "macos")]
fn cases() -> Vec<(&'static str, fn())> {
    macro_rules! cases {
        ($($t:path),* $(,)?) => { vec![$((stringify!($t), $t as fn())),*] };
    }
    cases!(
        has_editor_agrees_with_opening_one,
        open_size_close_is_idempotent_and_reports_zero_after,
        view_ptr_is_non_null_only_while_open,
        repeated_open_close_cycles_stay_balanced,
        an_au_without_a_cocoa_view_is_refused_cleanly,
        dropping_without_closing_is_safe,
        two_editors_on_one_unit_are_independent,
    )
}

/// On every other target this suite has no case: `AuEditor` is Cocoa, and the
/// AU corpus ships with the OS. The target still has to link, so `main` stays,
/// and lists and runs nothing.
#[cfg(not(target_os = "macos"))]
fn cases() -> Vec<(&'static str, fn())> {
    Vec::new()
}

/// Run one case, catching a panic so the remaining cases still run: knowing
/// which subset broke is most of the diagnostic.
fn run(name: &str, f: fn()) -> bool {
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cases = cases();
    match parse(&args) {
        Request::List { ignored } => {
            if !ignored {
                for (name, _) in &cases {
                    println!("{name}: test");
                }
            }
        }
        Request::Run {
            filters,
            exact,
            ignored,
        } => {
            if ignored {
                return;
            }
            let mut failed = Vec::new();
            let mut total = 0;
            for (name, case) in &cases {
                if selected(name, &filters, exact) {
                    total += 1;
                    if !run(name, *case) {
                        failed.push(*name);
                    }
                }
            }
            eprintln!("\n{}/{} passed", total - failed.len(), total);
            if !failed.is_empty() {
                eprintln!("failed: {}", failed.join(", "));
                std::process::exit(1);
            }
        }
    }
}
