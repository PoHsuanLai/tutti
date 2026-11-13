//! Where the platform has no descriptors to wait on (CLAP's posix fds and
//! VST3's run loop are POSIX and Linux), a plugin registers none, and a look
//! finds nothing ready.

use super::{Ready, Watch};

/// Nothing to look at.
pub(super) fn poll_now(_watches: &[Watch], _ready: &mut Vec<Ready>) {}
