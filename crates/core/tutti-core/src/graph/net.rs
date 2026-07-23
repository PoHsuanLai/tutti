//! `GraphNet` — thin facade over [`fundsp::net::Net`] that adds typed downcast
//! helpers.
//!
//! Callers use [`GraphNet::inner`] / [`GraphNet::inner_mut`] to reach fundsp's
//! `Net` directly for all pure graph operations (push, connect, remove, etc.).
//! The rest of the API is intentionally minimal.

use fundsp::net::{Net, NodeId};
use fundsp::prelude::AudioUnit;
use fundsp::realnet::NetBackend;

pub struct GraphNet {
    net: Net,
}

impl GraphNet {
    /// Build a new net with the given input/output port counts.
    pub fn new(inputs: usize, outputs: usize) -> Self {
        Self {
            net: Net::new(inputs, outputs),
        }
    }

    /// Consumes a live backend that can be plugged into an audio callback
    /// processor. Can only be called once (panics on second call).
    pub fn backend(&mut self) -> NetBackend {
        self.net.backend()
    }

    /// Shared access to the underlying fundsp `Net` for queries.
    pub fn inner(&self) -> &Net {
        &self.net
    }

    /// Mutable access to the underlying fundsp `Net`.
    ///
    /// All pure graph operations (push, connect, remove, replace, set_sample_rate, ...)
    /// go through this escape hatch.
    pub fn inner_mut(&mut self) -> &mut Net {
        &mut self.net
    }

    /// Get a typed reference to a node. Returns `None` if the node is not of type `T`.
    pub fn downcast<T: AudioUnit + 'static>(&self, id: NodeId) -> Option<&T> {
        <dyn AudioUnit>::as_any(self.net.node(id)).downcast_ref::<T>()
    }

    /// Get a typed mutable reference to a node. Returns `None` if the node is not of type `T`.
    pub fn downcast_mut<T: AudioUnit + 'static>(&mut self, id: NodeId) -> Option<&mut T> {
        <dyn AudioUnit>::as_any_mut(self.net.node_mut(id)).downcast_mut::<T>()
    }

    /// Publish pending changes to the audio thread.
    pub fn commit(&mut self) {
        self.net.commit();
    }
}
