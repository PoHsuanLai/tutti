//! The in-process VST2 node forked, on the process's main thread.
//!
//! A fork of an in-process plugin is made on the main thread only (`MainThreadMarker::new` asks
//! the OS), and the default harness runs every test on a thread it spawns: this target sets
//! `harness = false` and owns `main()` (`tutti_fixtures::main_thread_cases`).

mod vst2_in_process_fork;

fn main() {
    tutti_fixtures::main_thread_cases(vst2_in_process_fork::CASES);
}
