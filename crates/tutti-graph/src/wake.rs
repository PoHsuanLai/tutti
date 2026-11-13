//! [`Wake`]: the handle a node gives to whatever thread must wake it from
//! park.

use std::sync::Arc;
use tutti_rt::WakeFlag;

/// A handle that wakes one node the executor parked.
///
/// A node receives its handle in [`Node::bind_wake`](crate::Node::bind_wake)
/// and hands a clone to any thread
/// that will need to wake it (a plugin's host callback, a worker, a device
/// thread). `Wake` is `Clone + Send + Sync`.
///
/// [`raise`](Self::raise) is wait-free and never allocates or locks, so any
/// thread may call it, an audio callback included. A raise **counts as input**:
/// a node parked on [`Status::Idle`](crate::Status::Idle) (or on its silent
/// output) is called on the next block the executor runs after the raise. A
/// raise while the node is running, or not parked, is harmless: the next
/// block consumes it and the node is called as it would have been. Several
/// raises between two blocks wake the node once.
///
/// The handle belongs to one placement of the node: a node that replaces it
/// ([`Editor::replace`](crate::Editor::replace)) or a fork's copy of it
/// ([`Editor::fork`](crate::Editor::fork)) is a different node with its own
/// handle, and a raise on the old one wakes nothing of the new.
///
/// The flag's storage is shared by every clone and by the executor's copy;
/// the executor's copy is dropped on the control side with the node
/// (`NodeBox`), so the audio thread never frees it.
#[derive(Clone, Debug, Default)]
pub struct Wake(Arc<WakeFlag>);

impl Wake {
    /// A handle on a fresh flag. Control thread: allocates.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Wakes the node on its next block, from any thread. See the type docs.
    pub fn raise(&self) {
        self.0.raise();
    }

    /// Whether the handle was raised since the last take, clearing it. The
    /// executor's call, once per node per block.
    pub(crate) fn take(&self) -> bool {
        self.0.take()
    }
}
