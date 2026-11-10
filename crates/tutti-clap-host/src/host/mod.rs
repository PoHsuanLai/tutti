//! Host side of the CLAP protocol: [`HostState`], the latches a plugin's
//! callbacks write into, and [`ClapHost`], the `clap_host` vtable that
//! dispatches into it. The vtable and the stream adapters are the FFI crate's.

mod callbacks;
pub mod state;
pub mod streams;

pub use state::*;
pub use streams::{InputStream, OutputStream};

use std::sync::Arc;
use tutti_clap_ffi::{Host, HostCallbacks};

/// The `clap_host` handed to a plugin during instantiation, dispatching its
/// callbacks into a [`HostState`].
#[derive(Debug)]
pub struct ClapHost {
    inner: Host,
    state: Arc<HostState>,
}

impl ClapHost {
    /// Builds a host vtable backed by the given [`HostState`]. The state is
    /// shared (via `Arc`) with [`ClapLoaded`](crate::ClapLoaded) so both
    /// sides can observe plugin callbacks.
    #[must_use]
    pub fn new(state: Arc<HostState>) -> Self {
        let callbacks: Arc<dyn HostCallbacks> = state.clone();
        Self {
            inner: Host::new(callbacks),
            state,
        }
    }

    /// Shared access to the [`HostState`] this vtable dispatches callbacks into.
    #[must_use]
    pub fn state(&self) -> &Arc<HostState> {
        &self.state
    }

    /// The FFI vtable object, for a test that drives the callbacks itself.
    #[doc(hidden)]
    #[must_use]
    pub fn ffi(&self) -> &Host {
        &self.inner
    }

    /// The FFI vtable, for the loader that hands it to a plugin.
    pub(crate) fn into_inner(self) -> Host {
        self.inner
    }
}
