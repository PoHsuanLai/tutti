//! Where the platform has no descriptors to wait on (CLAP's posix fds and
//! VST3's run loop are POSIX and Linux), a plugin registers none, and a look
//! finds nothing ready.

use tutti_plugin_types::{Ready, Wakeups};

/// Nothing to look at.
pub(super) fn poll_now(_wakeups: &Wakeups, ready: &mut Vec<Ready>) {
    ready.clear();
}
