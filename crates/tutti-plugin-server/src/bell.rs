//! [`Bell`]: how a plugin's call on any thread makes the main loop run now.
//!
//! The main loop sleeps in one wait over the control socket, the plugin's own
//! descriptors and the next tick. A plugin's `request_process` may be called
//! from its audio thread, which can neither take a lock nor write the control
//! socket, yet its answer (`ProcessRequested`) must leave the server at once,
//! not on the next tick. [`Bell::ring`] sets a flag and makes the platform's
//! wait end; it takes no lock, allocates nothing and never waits. The main
//! loop clears the flag with [`Bell::take`] after each wait. How the wait ends
//! is the platform module's: this `mod` line carries the bell's only platform
//! `cfg`.

use std::sync::OnceLock;

#[cfg(unix)]
#[path = "bell/unix.rs"]
mod sys;
#[cfg(not(unix))]
#[path = "bell/windows.rs"]
mod sys;

pub(crate) use sys::Bell;

/// The one bell of this server process: one plugin is served, and the plugin's
/// host callbacks reach it through no other path.
pub(crate) fn bell() -> &'static Bell {
    static BELL: OnceLock<Bell> = OnceLock::new();
    BELL.get_or_init(Bell::new)
}
