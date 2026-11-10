//! One-shot metadata probe: spawn `plugin-probe`, wait for it, read its result
//! file. A timeout kills the child.
//!
//! The wire is the file (`tutti_plugin_types::probe_wire`), not a socket or a
//! stream. The child's stdout and stderr go to the null device, because plugins
//! print from their init code. With the result file the outcomes are
//! unambiguous: a file means the probe finished, with a descriptor or the
//! loader's own failure; no file and a signal or a non-zero exit means it
//! crashed; no file at the deadline means it hung.

use super::locate::ProbeLocator;
use crate::error::{BridgeError, Result};
use crate::protocol::PluginDescriptor;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tutti_plugin_types::{ProbeOutcome, read_result, result_tmp_path};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(5);

/// Reads a plugin's descriptor in a `plugin-probe` subprocess, so a plugin that
/// crashes or hangs in its init does not take the caller down.
///
/// # Errors
///
/// [`BridgeError::ProbeNotFound`] if the binary cannot be located;
/// [`BridgeError::LoadFailed`] if the probe ran to its end and the plugin could
/// not be read; [`BridgeError::ProcessCrashed`] if it died without a result;
/// [`BridgeError::Timeout`] if it did not finish in five seconds (it is killed).
pub fn probe_metadata(plugin_path: &Path) -> Result<PluginDescriptor> {
    probe_metadata_with(&ProbeLocator::Search, plugin_path, PROBE_TIMEOUT)
}

/// [`probe_metadata`], with the binary and the deadline named rather than
/// searched for and fixed: the seam a test substitutes through, so none writes
/// the process-global environment.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn probe_metadata_with(
    locator: &ProbeLocator,
    plugin_path: &Path,
    timeout: Duration,
) -> Result<PluginDescriptor> {
    let probe = locator.resolve()?;
    let result = next_result_path();
    let _ = std::fs::remove_file(&result);
    let _ = std::fs::remove_file(result_tmp_path(&result));

    let outcome = run(&probe, plugin_path, &result, timeout);

    let _ = std::fs::remove_file(&result);
    let _ = std::fs::remove_file(result_tmp_path(&result));
    match outcome? {
        ProbeOutcome::Described(descriptor) => Ok(descriptor),
        ProbeOutcome::Failed(message) => Err(BridgeError::load_from_server(plugin_path, message)),
        // `ProbeOutcome` is `#[non_exhaustive]`: an outcome from a newer probe.
        other => Err(BridgeError::IpcError(format!(
            "unknown probe outcome: {other:?}"
        ))),
    }
}

fn run(probe: &Path, plugin: &Path, result: &Path, timeout: Duration) -> Result<ProbeOutcome> {
    let mut child = Command::new(probe)
        .arg(plugin)
        .arg(result)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(BridgeError::Io)?;
    let status = wait(&mut child, timeout);
    // The file is read even after a timeout or a bad exit: it is the only proof
    // of finishing, and a probe that finished at the deadline did.
    let file = read_result(result).map_err(|e| BridgeError::IpcError(e.to_string()));
    match (file, status) {
        (Ok(Some(outcome)), _) => Ok(outcome),
        (Err(e), _) => Err(e),
        (Ok(None), Waited::Exited) => Err(BridgeError::ProcessCrashed),
        (Ok(None), Waited::TimedOut) => Err(BridgeError::Timeout {
            operation: "probe".into(),
            duration_ms: timeout.as_millis() as u64,
            partial: false,
        }),
        (Ok(None), Waited::Lost(e)) => Err(BridgeError::Io(e)),
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

fn next_result_path() -> PathBuf {
    static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "tutti-probe-{}-{}.result",
        std::process::id(),
        PROBE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// A `plugin-probe` stand-in: a shell script given the plugin path as `$1`
    /// and the result file as `$2`, as the real one is.
    fn stand_in(dir: &TempDir, body: &str) -> ProbeLocator {
        let path = dir.path().join("plugin-probe");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        ProbeLocator::at(path)
    }

    fn probe(locator: &ProbeLocator, timeout: Duration) -> Result<PluginDescriptor> {
        probe_metadata_with(locator, Path::new("/plugins/Some.clap"), timeout)
    }

    /// A probe that does not finish by the deadline is killed and reaped, and the
    /// caller is told it timed out.
    ///
    /// Mutation (run): `wait` without the `child.kill()` blocks until the
    /// stand-in's 30 s sleep ends, and the elapsed-time assertion fails.
    #[test]
    fn a_probe_that_hangs_is_killed_at_the_deadline() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("pid");
        let locator = stand_in(
            &dir,
            &format!("echo $$ > {}\nexec sleep 30", pid_file.display()),
        );

        let started = Instant::now();
        let result = probe(&locator, Duration::from_millis(300));
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
        let missing = ProbeLocator::at("/definitely/not/plugin-probe");
        assert!(matches!(
            probe(&missing, Duration::from_secs(1)),
            Err(BridgeError::ProbeNotFound)
        ));
    }
}
