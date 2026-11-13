//! `plugin-probe <path> <result-file> [--measure [--state <file>]]`: load a plugin file just far
//! enough to read the descriptor of every plugin it holds, write the outcome to
//! the result file and exit.
//!
//! With `--measure` the probe then activates each plugin and renders it: the
//! tail and latency it reports against what it makes of an impulse (doc 023
//! section 6, `tutti_plugin_probe::measure`). The figures are in each
//! descriptor's `measured`. A plugin that crashes or hangs while rendering is a
//! probe that crashed or hung, to the host that spawned it. `--state <file>`
//! puts the plugin in the state the file holds (what the plugin's own
//! `save_state` wrote) before it is measured, so the figure is of that patch:
//! the record carries a hash of it.
//!
//! The result file is the only wire (`tutti_plugin_types::write_result`: a
//! staging file renamed into place). Plugins print from their init code, so
//! this process's stdout and stderr carry nothing; the host that spawns it
//! points them at the null device. A file at `<result-file>` means the probe
//! finished, with a descriptor or a typed failure; no file means it crashed or
//! hung, which the host tells from the process.
//!
//! Exit status: `0` once a result is written (a plugin that would not load is a
//! result, not an error), `2` for a bad command line, `3` if the result could
//! not be written.
//!
//! Formats are cargo features (`clap`, `vst3`, `vst2`, and `au` on macOS); a probe
//! built without the one a path names reports that as a failure.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;
use tutti_plugin_probe::{describe, describe_measured};
use tutti_plugin_types::{MainThread, ProbeOutcome, write_result};

fn main() -> ExitCode {
    const USAGE: &str =
        "usage: plugin-probe <plugin-path> <result-file> [--measure [--state <file>]]";
    let mut args = std::env::args_os().skip(1);
    let (Some(path), Some(result)) = (args.next(), args.next()) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest: Vec<_> = args.collect();
    let (measure, state) = match rest.as_slice() {
        [] => (false, None),
        [m] if m == "--measure" => (true, None),
        [m, s, file] if m == "--measure" && s == "--state" => (true, Some(file)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let state = match state.map(std::fs::read).transpose() {
        Ok(state) => state,
        Err(e) => {
            // A bad `--state` is the caller's, not a plugin's: a result, not a crash.
            return finish(
                &result,
                ProbeOutcome::Failed(format!("cannot read the state: {e}")),
            );
        }
    };
    // This process runs one probe on one thread, which is its main thread.
    let Ok(main) = MainThread::claim() else {
        eprintln!("plugin-probe: the main thread is already claimed");
        return ExitCode::from(3);
    };
    let described = if measure {
        describe_measured(&main, Path::new(&path), state.as_deref())
    } else {
        describe(&main, Path::new(&path))
    };
    let outcome = match described {
        Ok(plugins) if plugins.is_empty() => {
            ProbeOutcome::Failed("the file holds no plugin".to_string())
        }
        Ok(plugins) => ProbeOutcome::Described(plugins),
        Err(message) => ProbeOutcome::Failed(message),
    };
    finish(&result, outcome)
}

/// Writes the outcome to the result file.
fn finish(result: &std::ffi::OsStr, outcome: ProbeOutcome) -> ExitCode {
    match write_result(Path::new(result), &outcome) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "plugin-probe: cannot write {}: {e}",
                Path::new(result).display()
            );
            ExitCode::from(3)
        }
    }
}
