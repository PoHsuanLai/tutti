//! The token a test passes where a plugin entry wants the main thread.

use tutti_rt::MainThreadMarker;

/// A main-thread token for the calling thread, which stands in for the main
/// thread. The test harness runs every test on a thread it spawned, never the
/// process's main thread, so [`MainThreadMarker::new`] refuses there and a test mints
/// its proof here. Each call makes a token; a test that drives a plugin from
/// two threads makes one on each.
#[must_use]
pub fn main_thread() -> MainThreadMarker {
    #[expect(
        unsafe_code,
        reason = "test-only: a test thread stands in for the main thread"
    )]
    // SAFETY: a test passes the token to entries it calls from this thread, and
    // runs no other thread's main-thread entry of the same plugin meanwhile.
    unsafe {
        MainThreadMarker::new_unchecked()
    }
}

/// Runs `cases` as a `harness = false` test target does, on the process's main
/// thread: the default harness spawns a thread per test, and only the process's
/// main thread holds a `MainThreadMarker` from the OS.
///
/// It answers the command line nextest drives a libtest binary with: `--list`
/// prints one `<case>: test` line per case and runs nothing, and `<case>
/// --exact` runs that case alone, exiting non-zero when it panics. With no case
/// named every case runs, as `cargo test` does.
pub fn main_thread_cases(cases: &[(&str, fn())]) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            for (name, _) in cases {
                println!("{name}: test");
            }
        }
        return;
    }
    let exact = args.iter().any(|a| a == "--exact");
    let filters: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect();
    for (name, case) in cases {
        let chosen = filters.is_empty()
            || filters
                .iter()
                .any(|f| if exact { name == f } else { name.contains(f) });
        if chosen {
            case();
            println!("test {name} ... ok");
        }
    }
}

/// The list [`main_thread_cases`] takes: each function named, paired with its own
/// name from `stringify!`. `cases!["module"; a, b]` prefixes every name with
/// `module::`, for a target that gathers the cases of several modules. An entry
/// may carry `#[cfg(..)]`.
#[macro_export]
macro_rules! cases {
    ($prefix:literal; $($(#[$meta:meta])* $case:ident),* $(,)?) => {
        &[$($(#[$meta])* (concat!($prefix, "::", stringify!($case)), $case as fn())),*]
    };
    ($($(#[$meta:meta])* $case:ident),* $(,)?) => {
        &[$($(#[$meta])* (stringify!($case), $case as fn())),*]
    };
}
