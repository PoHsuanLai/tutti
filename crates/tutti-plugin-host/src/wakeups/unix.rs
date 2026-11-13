//! The unix look: `poll(2)` through the process edge's one OS layer.

use tutti_plugin_types::{FdId, Interest, Ready, Wakeups};

/// Looks, without waiting, for which of the plugin's descriptors are ready,
/// into `ready` (replacing its contents).
pub(super) fn poll_now(wakeups: &Wakeups, ready: &mut Vec<Ready>) {
    ready.clear();
    if wakeups.watches().is_empty() {
        return;
    }
    let asked: Vec<(i32, u8)> = wakeups
        .watches()
        .iter()
        .map(|w| (w.fd.0, w.interest.bits()))
        .collect();
    let mut got = Vec::new();
    tutti_plugin_ipc_ffi::poll_now(&asked, &mut got);
    ready.extend(got.into_iter().map(|(fd, bits)| Ready {
        fd: FdId(fd),
        events: Interest::from_bits(bits),
    }));
}
