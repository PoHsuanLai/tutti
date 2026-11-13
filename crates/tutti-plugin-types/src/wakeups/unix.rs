//! The unix wait: `poll(2)` through the process edge's one OS layer.

use super::{Interest, Ready, Watch};
use crate::FdId;

/// What a look reuses, so a steady watch set allocates nothing.
#[derive(Default)]
struct Scratch {
    poller: tutti_plugin_ipc_ffi::Poller,
    asked: Vec<(i32, u8)>,
    got: Vec<(i32, u8)>,
}

thread_local! {
    static SCRATCH: std::cell::Cell<Option<Box<Scratch>>> = const { std::cell::Cell::new(None) };
}

/// Looks, without waiting, for which of `watches` are ready.
pub(super) fn poll_now(watches: &[Watch], ready: &mut Vec<Ready>) {
    if watches.is_empty() {
        return;
    }
    let mut scratch = SCRATCH.take().unwrap_or_default();
    scratch.asked.clear();
    scratch
        .asked
        .extend(watches.iter().map(|w| (w.fd.0, w.interest.bits())));
    let Scratch { poller, asked, got } = &mut *scratch;
    // A failed look is none ready: the descriptors are looked at again on the
    // next pass, and a signal is not a fault.
    if poller.wait(None, asked, 0, got).is_ok() {
        ready.extend(got.iter().map(|&(fd, bits)| Ready {
            fd: FdId(fd),
            events: Interest::from_bits(bits),
        }));
    }
    SCRATCH.set(Some(scratch));
}
