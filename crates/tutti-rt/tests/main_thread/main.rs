//! `MainThreadMarker::new` against the OS's main thread.
//!
//! The default harness runs every test on a thread it spawns, never the
//! process's main thread, so this target sets `harness = false` and owns
//! `main()`, which is the main thread. It answers the command line nextest drives a
//! libtest binary with: `--list` prints one `<case>: test` line per case, and
//! `<case> --exact` runs that case alone and exits non-zero when it panics. (This
//! crate names no workspace crate, so it cannot use `tutti_fixtures::main_thread_cases`.)

use tutti_rt::MainThreadMarker;

/// **The process's main thread holds a token.**
fn the_main_thread_holds_a_token() {
    assert!(MainThreadMarker::new().is_some());
}

/// **A thread the process spawns holds none.**
fn a_spawned_thread_holds_no_token() {
    let held = std::thread::spawn(|| MainThreadMarker::new().is_some())
        .join()
        .expect("the probe thread runs");
    assert!(!held);
}

/// **The token is the main thread's alone, from the first query on**: asking
/// from a spawned thread first leaves the answer on the main thread unchanged.
fn the_answer_does_not_depend_on_who_asked_first() {
    let spawned = std::thread::spawn(|| MainThreadMarker::new().is_some())
        .join()
        .expect("the probe thread runs");
    assert!(!spawned);
    assert!(MainThreadMarker::new().is_some());
}

const CASES: &[(&str, fn())] = &[
    (
        "the_main_thread_holds_a_token",
        the_main_thread_holds_a_token,
    ),
    (
        "a_spawned_thread_holds_no_token",
        a_spawned_thread_holds_no_token,
    ),
    (
        "the_answer_does_not_depend_on_who_asked_first",
        the_answer_does_not_depend_on_who_asked_first,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            for (name, _) in CASES {
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
    for (name, case) in CASES {
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
