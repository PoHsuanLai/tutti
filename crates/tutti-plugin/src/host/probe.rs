//! One-shot metadata probe: spawn `plugin-probe`, wait for it, read its result
//! file. A timeout kills the child.
//!
//! The wire is the file (`tutti_plugin_types::probe_wire`), not a socket or a
//! stream. The child's stdout and stderr go to the null device, because plugins
//! print from their init code. With the result file the outcomes are
//! unambiguous: a file means the probe finished, with a descriptor or the
//! loader's own failure; no file and a signal or a non-zero exit means it
//! crashed; no file at the deadline means it hung.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tutti_plugin_ipc::{BridgeError, Reaped, Result};
use tutti_plugin_types::{Helper, Locator, PluginDescriptor, ProbeOutcome, read_result};

/// How long a probe may take, spawn and the plugin's load included, unless the
/// caller says otherwise. A cold plugin can take seconds to `dlopen`; the old
/// socket probe allowed about twelve.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(5);

/// Reads a plugin's descriptor in a `plugin-probe` subprocess, so a plugin that
/// crashes or hangs in its init does not take the caller down. `locator` names
/// the binary (a test passes [`Locator::at`], so none writes the
/// process-global environment) and `timeout` is the deadline.
///
/// # Errors
///
/// [`BridgeError::ProbeNotFound`] if the binary cannot be located;
/// [`BridgeError::LoadFailed`] if the probe ran to its end and the plugin could
/// not be read; [`BridgeError::ProcessCrashed`] if it died without a result;
/// [`BridgeError::Timeout`] if it did not finish within `timeout` (it is killed).
pub fn probe_metadata_with(
    locator: &Locator,
    plugin_path: &Path,
    timeout: Duration,
) -> Result<PluginDescriptor> {
    let probe = locator
        .resolve(Helper::Probe)
        .ok_or(BridgeError::ProbeNotFound)?;
    // A directory of the probe's own (mode 0700), removed when it drops, on a
    // panic as well: the result path is not a name in a shared directory.
    let dir = tempfile::Builder::new()
        .prefix("tutti-probe-")
        .tempdir()
        .map_err(BridgeError::Io)?;
    let result = dir.path().join("result");

    let child = Command::new(&probe)
        .arg(plugin_path)
        .arg(&result)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(BridgeError::Io)?;
    let mut child = Reaped::new(child);
    let status = wait(&mut child, timeout);
    // The file is read even after a timeout or a bad exit: it is the only proof
    // of finishing, and a probe that finished at the deadline did.
    let file = read_result(&result).map_err(|e| BridgeError::IpcError(e.to_string()));
    let outcome = match (file, status) {
        (Ok(Some(outcome)), _) => outcome,
        (Err(e), _) => return Err(e),
        (Ok(None), Waited::Exited) => return Err(BridgeError::ProcessCrashed),
        (Ok(None), Waited::TimedOut) => {
            return Err(BridgeError::Timeout {
                operation: "probe".into(),
                duration_ms: timeout.as_millis() as u64,
                partial: false,
            });
        }
        (Ok(None), Waited::Lost(e)) => return Err(BridgeError::Io(e)),
    };
    match outcome {
        ProbeOutcome::Described(descriptor) => Ok(descriptor),
        ProbeOutcome::Failed(message) => Err(BridgeError::load_from_server(plugin_path, message)),
        // `ProbeOutcome` is `#[non_exhaustive]`: an outcome from a newer probe.
        other => Err(BridgeError::IpcError(format!(
            "unknown probe outcome: {other:?}"
        ))),
    }
}

enum Waited {
    Exited,
    TimedOut,
    Lost(std::io::Error),
}

/// Waits for `child` to exit, killing and reaping it at `timeout`.
fn wait(child: &mut Child, timeout: Duration) -> Waited {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Waited::Exited,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Waited::TimedOut;
            }
            Ok(None) => thread::sleep(POLL),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Waited::Lost(e);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A `plugin-probe` stand-in: a shell script given the plugin path as `$1`
    /// and the result file as `$2`, as the real one is.
    fn stand_in(dir: &TempDir, body: &str) -> Locator {
        let path = dir.path().join("plugin-probe");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Locator::at(path)
    }

    fn probe(locator: &Locator, timeout: Duration) -> Result<PluginDescriptor> {
        probe_metadata_with(locator, Path::new("/plugins/Some.clap"), timeout)
    }

    /// A probe that does not finish by the deadline is killed and reaped, and the
    /// caller is told it timed out.
    ///
    /// Mutation (run): `wait` without its `child.kill()` and `Reaped::drop`
    /// without its `kill()` block until the stand-in's 30 s sleep ends, and the
    /// elapsed-time assertion fails.
    #[test]
    fn a_probe_that_hangs_is_killed_at_the_deadline() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("pid");
        let locator = stand_in(
            &dir,
            &format!("echo $$ > {}\nexec sleep 30", pid_file.display()),
        );

        let started = Instant::now();
        // Long enough for the shell to start and write its pid before it is
        // killed: at 300 ms a cold shell on macOS sometimes had not, and the
        // pid read below failed. Still far below the 10 s a wait would take.
        let result = probe(&locator, Duration::from_secs(2));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the child was waited for, not killed: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(result, Err(BridgeError::Timeout { .. })),
            "{result:?}"
        );

        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(
            !alive,
            "the hung probe (pid {}) is still running",
            pid.trim()
        );
    }

    /// No result file and an exit, by status or by signal, is a crash.
    ///
    /// Mutation (run): the `(Ok(None), Waited::Exited)` arm answering
    /// `Timeout` fails both assertions.
    #[test]
    fn exiting_without_a_result_is_a_crash() {
        for body in ["exit 7", "exit 0", "kill -SEGV $$"] {
            let dir = TempDir::new().unwrap();
            let result = probe(&stand_in(&dir, body), Duration::from_secs(10));
            assert!(
                matches!(result, Err(BridgeError::ProcessCrashed)),
                "`{body}`: {result:?}"
            );
        }
    }

    /// **A partial result is never read as a result**: a staging file left by a
    /// probe that died mid-write is not one (it is a crash), and a cut-short
    /// file at the result path is refused as a protocol fault, not decoded.
    ///
    /// Mutation (run): `run` reading `result_tmp_path(result)` instead of
    /// `result` fails the first case (the garbage is refused, not a crash);
    /// mapping a `read_result` error to `Ok(None)` makes the second a crash.
    #[test]
    fn a_partial_result_file_is_not_a_result() {
        let dir = TempDir::new().unwrap();
        let staged = stand_in(&dir, "printf 'TPRB\\001' > \"$2.tmp\"");
        let result = probe(&staged, Duration::from_secs(10));
        assert!(
            matches!(result, Err(BridgeError::ProcessCrashed)),
            "{result:?}"
        );

        let dir = TempDir::new().unwrap();
        let cut = stand_in(&dir, "printf 'TPRB\\001' > \"$2\"");
        let result = probe(&cut, Duration::from_secs(10));
        assert!(
            matches!(result, Err(BridgeError::IpcError(_))),
            "{result:?}"
        );
    }

    /// A probe binary that is not there is `ProbeNotFound`, the one error the
    /// scan answers with filename metadata.
    #[test]
    fn a_missing_probe_binary_is_probe_not_found() {
        let missing = Locator::at("/definitely/not/plugin-probe");
        assert!(matches!(
            probe(&missing, Duration::from_secs(1)),
            Err(BridgeError::ProbeNotFound)
        ));
    }

    /// The deadline is the caller's: the same probe, one that outlasts 300 ms and
    /// not 5 s, times out under the one and is a plain crash under the other.
    ///
    /// Mutation (run): `probe_metadata_with` using a fixed 100 ms instead of
    /// `timeout` makes the second a `Timeout` and fails it.
    #[test]
    fn the_deadline_is_the_callers() {
        let dir = TempDir::new().unwrap();
        let locator = stand_in(&dir, "sleep 1");
        let short = probe(&locator, Duration::from_millis(300));
        assert!(
            matches!(short, Err(BridgeError::Timeout { .. })),
            "{short:?}"
        );
        let long = probe(&locator, Duration::from_secs(10));
        assert!(matches!(long, Err(BridgeError::ProcessCrashed)), "{long:?}");
    }

    /// The default allows a cold plugin well past the old five seconds.
    #[test]
    fn the_default_deadline_is_fifteen_seconds() {
        assert_eq!(DEFAULT_PROBE_TIMEOUT, Duration::from_secs(15));
    }

    /// The result file is in a directory of the probe's own, not a predictable
    /// name in the shared temp directory, and the directory is gone after.
    ///
    /// Mutation (run): the result path put back at
    /// `temp_dir().join("tutti-probe-<pid>.result")` fails the parent check.
    #[test]
    fn the_result_lives_in_a_private_directory_that_is_removed() {
        let dir = TempDir::new().unwrap();
        let seen = dir.path().join("seen");
        let locator = stand_in(&dir, &format!("echo \"$2\" > {}", seen.display()));
        let _ = probe(&locator, Duration::from_secs(10));
        let result = PathBuf::from(std::fs::read_to_string(&seen).unwrap().trim());
        let parent = result.parent().unwrap();
        assert_ne!(parent, std::env::temp_dir(), "a shared directory");
        assert!(
            parent
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tutti-probe-")
        );
        assert!(!parent.exists(), "the probe's directory outlived it");
    }
}
