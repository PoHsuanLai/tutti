//! Host-local configuration for a plugin bridge. Not wire data —
//! controls how *this* process connects to and manages a plugin-server.

use crate::protocol::SampleFormat;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// A socket path no other bridge will pick, and no other user can guess.
///
/// Every bridge needs its own: the path is a rendezvous point for exactly one
/// host/server pair, and `bind` unlinks whatever is already there. Two bridges
/// sharing one path means the second unlinks the first's *live* socket, and
/// then either `ProcessGuard::drop` deletes the survivor's socket out from
/// under it. The symptom is a plugin that dies when an unrelated plugin is
/// unloaded — far from the cause.
///
/// The pid and the counter keep it unique; the random part keeps it secret.
/// The directory is the shared temporary one, so a name another local user
/// can predict is a name they can claim first: a socket of theirs there is a
/// plugin wire they speak.
///
/// The directory is the temporary one when the path fits a Unix socket
/// address there (`socket_path_in`), so a long `TMPDIR` (a deep worktree, or
/// macOS's per-user `/var/folders/…/T/`) moves the socket rather than failing
/// the launch.
///
/// # Panics
///
/// If the operating system has no random source to read (`getrandom` fails).
#[must_use]
pub fn unique_socket_path() -> PathBuf {
    socket_path_in(
        &std::env::temp_dir(),
        &format!("{}.sock", unique_name("tutti-bridge")),
    )
}

/// The longest socket path every Unix takes: `sun_path` is 104 bytes on macOS
/// and the BSDs and 108 on Linux, and the path needs a terminating NUL in it.
/// One figure for all, so a path that fits on Linux fits on macOS too.
const SOCKET_PATH_MAX: usize = 103;

/// `name` in the first of `temp`, `$XDG_RUNTIME_DIR` and `/tmp` where the whole
/// path fits [`SOCKET_PATH_MAX`]. `/tmp` always fits a name
/// [`unique_socket_path`] makes. On Windows the socket is a named pipe named by
/// `name` alone ([`socket_name`](crate::transport::control::socket_name)), so
/// the directory is `temp`'s whatever its length.
///
/// `$XDG_RUNTIME_DIR` comes before `/tmp` because it is the user's own (mode
/// 0700); `temp` comes first because it is the one the user chose.
fn socket_path_in(temp: &std::path::Path, name: &str) -> PathBuf {
    let fits = |p: &PathBuf| cfg!(windows) || p.as_os_str().len() <= SOCKET_PATH_MAX;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute());
    std::iter::once(temp.to_path_buf())
        .chain(runtime)
        .chain(std::iter::once(PathBuf::from("/tmp")))
        .map(|dir| dir.join(name))
        .find(fits)
        .unwrap_or_else(|| temp.join(name))
}

/// `stem`, this process's id, a count shared by every name this process
/// makes, and [`unguessable`]: a name for something in a shared namespace
/// (the control socket, the audio slab) that is unique and not predictable.
/// One count for both, so no two names this process makes are alike
/// whatever their stems.
///
/// # Panics
///
/// If the operating system has no random source to read.
pub(crate) fn unique_name(stem: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{stem}-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        unguessable(),
    )
}

/// Sixteen hex digits from the operating system's random source, for a name in
/// a shared directory that must not be predictable (the control socket here,
/// the audio slab in `subprocess::launch`). Sixty-four bits: enough that
/// guessing is not an attack, short enough that a macOS socket path (104 bytes,
/// under a long per-user temporary directory) still fits.
///
/// # Panics
///
/// If the operating system has no random source to read.
fn unguessable() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system's random source is readable");
    format!("{:016x}", u64::from_ne_bytes(bytes))
}

/// Low-level settings for one plugin-server connection.
///
/// Most hosts never build one: [`AudioConfig`](crate::AudioConfig)
/// covers the settings an application tunes, and each load derives a
/// `BridgeConfig` from it. `tutti-plugin-server` takes one to know where to
/// listen.
///
/// The default uses a fresh, unique socket path, the `tutti_audio_` slab
/// prefix, 8192-frame blocks, a 5000 ms timeout and 32-bit float samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// Local-socket path this bridge's host/server pair meet on (on Windows,
    /// its final component names a named pipe).
    ///
    /// Must be unique per bridge: binding unlinks whatever is already there,
    /// so two bridges sharing a path break each other. [`Default`] generates a
    /// unique one per call.
    pub socket_path: PathBuf,
    /// Prefix for the shared-memory audio slab's OS name.
    pub shm_prefix: String,
    /// Largest block, in **frames**, the plugin will be asked to process. Sizes
    /// the slab, so a later block may not exceed it.
    pub max_buffer_size: usize,
    /// How long to wait on a subprocess reply before erroring, in milliseconds.
    pub timeout_ms: u64,
    /// Sample format to request; the subprocess replies with what it negotiated.
    #[serde(default)]
    pub preferred_format: SampleFormat,
}

impl Default for BridgeConfig {
    /// **`socket_path` is unique per call, not a constant.**
    ///
    /// A fixed path would make every `..BridgeConfig::default()` a latent
    /// collision — the second bridge to bind unlinks the first's live socket,
    /// and the symptom is a plugin dying when an unrelated plugin is unloaded.
    /// A default that is safe only when overridden is not a default: deriving it
    /// here means a caller must go out of its way to *create* a collision rather
    /// than to avoid one.
    fn default() -> Self {
        Self {
            socket_path: unique_socket_path(),
            shm_prefix: "tutti_audio_".to_string(),
            max_buffer_size: 8192,
            timeout_ms: 5000,
            preferred_format: SampleFormat::Float32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserted on `default()` itself rather than on `unique_socket_path`: the
    /// property that matters is that a *default* config does not collide, and a
    /// test of the helper alone passes even when `default()` stops calling it.
    #[test]
    fn each_default_config_gets_its_own_socket_path() {
        let a = BridgeConfig::default();
        let b = BridgeConfig::default();
        assert_ne!(
            a.socket_path, b.socket_path,
            "two default configs share a socket path: the second bridge to bind \
             will unlink the first's live socket"
        );
    }

    /// The socket's name carries sixty-four random bits, so another local user
    /// cannot predict it and bind it first (doc 027 D3). The pid and the
    /// counter alone keep names unique, which is why
    /// `each_default_config_gets_its_own_socket_path` cannot see this.
    #[test]
    fn the_socket_name_has_an_unguessable_part() {
        let path = unique_socket_path();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("a UTF-8 name");
        let random = stem.rsplit('-').next().expect("a last field");
        assert_eq!(random.len(), 16, "{stem}");
        assert!(random.chars().all(|c| c.is_ascii_hexdigit()), "{stem}");
        assert_ne!(unguessable(), unguessable(), "two draws should differ");
    }

    /// A temporary directory too deep for a socket address moves the socket
    /// to a short one, which a listener binds and a client connects to; one
    /// that fits keeps it. A deep `TMPDIR` (a worktree's scratch directory,
    /// macOS's `/var/folders/…/T/`) failed the plugin server's launch with
    /// "local socket name length exceeds capacity of sun_path".
    #[cfg(unix)]
    #[test]
    fn a_deep_temporary_directory_moves_the_socket() {
        use interprocess::local_socket::ListenerOptions;
        use interprocess::local_socket::traits::Listener as _;

        let base = tempfile::tempdir().expect("a temporary directory");
        let deep = base.path().join("d".repeat(SOCKET_PATH_MAX));
        std::fs::create_dir_all(&deep).expect("the deep directory");
        let name = format!("{}.sock", unique_name("tutti-bridge"));

        let path = socket_path_in(&deep, &name);
        assert!(!path.starts_with(&deep), "{path:?}");
        assert!(path.as_os_str().len() <= SOCKET_PATH_MAX, "{path:?}");

        let listener = ListenerOptions::new()
            .name(crate::transport::control::socket_name(&path).expect("a name"))
            .create_sync()
            .expect("the moved path binds");
        let connect = path.clone();
        let client = std::thread::spawn(move || crate::transport::control::connect(&connect));
        let _peer = listener.accept().expect("a peer");
        client
            .join()
            .expect("no panic")
            .expect("the client connects");
        drop(listener);
        let _ = std::fs::remove_file(&path);

        let root = std::path::Path::new("/");
        assert_eq!(socket_path_in(root, &name), root.join(&name));
    }

    /// A partial-update construction is the exact shape `to_bridge_config` uses,
    /// and the shape through which a shared path would propagate.
    #[test]
    fn partial_update_from_default_still_gets_a_unique_socket() {
        let a = BridgeConfig {
            timeout_ms: 1,
            ..BridgeConfig::default()
        };
        let b = BridgeConfig {
            timeout_ms: 2,
            ..BridgeConfig::default()
        };
        assert_ne!(a.socket_path, b.socket_path);
    }
}
