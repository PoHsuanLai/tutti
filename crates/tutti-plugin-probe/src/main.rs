//! `plugin-probe <path> <result-file>`: load a plugin just far enough to read its
//! descriptor, write the outcome to the result file and exit.
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
//! Formats are cargo features (`clap`, `vst3`, `vst2`); a probe built without
//! the one a path names reports that as a failure. Audio Units join when their
//! host implements `Loaded` (design 021, step 4c).

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;
use tutti_plugin_probe::describe;
use tutti_plugin_types::{MainThread, ProbeOutcome, write_result};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), Some(result), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: plugin-probe <plugin-path> <result-file>");
        return ExitCode::from(2);
    };
    // This process runs one probe on one thread, which is its main thread.
    let Ok(main) = MainThread::claim() else {
        eprintln!("plugin-probe: the main thread is already claimed");
        return ExitCode::from(3);
    };
    let outcome = match describe(&main, Path::new(&path)) {
        Ok(descriptor) => ProbeOutcome::Described(descriptor),
        Err(message) => ProbeOutcome::Failed(message),
    };
    match write_result(Path::new(&result), &outcome) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "plugin-probe: cannot write {}: {e}",
                Path::new(&result).display()
            );
            ExitCode::from(3)
        }
    }
}
