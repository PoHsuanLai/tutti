//! `clap.thread-pool`: giving the plugin a pool to run `request_exec` tasks
//! on, and the snapshot the Processor lends once per block.

use std::sync::Arc;

use tutti_clap_ffi::{GraphPool, PoolHandle, TaskPool};
use tutti_graph::WorkerPool;

use super::ClapLoaded;

impl ClapLoaded {
    /// Whether the plugin implements `clap.thread-pool` (it has tasks it can
    /// spread over a host's pool).
    #[must_use]
    pub fn supports_thread_pool(&self) -> bool {
        self.plugin.supports_thread_pool()
    }

    /// Serve the plugin's `clap.thread-pool` requests on `pool`, or refuse
    /// them (`None`, the default). Returns the pool it replaces.
    ///
    /// A request made from the plugin's `process` then runs its tasks on
    /// `pool` — the calling thread taking part — and returns once all have
    /// run. When `pool` is busy (the call comes from a participant of a job
    /// already running on it, a graph block), the request is refused
    /// ([`tutti_graph::Pool::try_run`]) rather than waiting on that job. A pool of one
    /// participant is treated as none: the plugin's own fallback serves it as
    /// well.
    ///
    /// With no pool every request returns `false`, which `ext/thread-pool.h`
    /// tells the plugin to answer by running the tasks itself; that is
    /// never worse than this host running them inline, and a plugin with
    /// threads of its own may do better.
    ///
    /// Control thread. Dropping the returned pool may join its threads.
    pub fn set_thread_pool(&mut self, pool: Option<Arc<WorkerPool>>) -> Option<Arc<GraphPool>> {
        self.set_task_pool(pool.map(|pool| Arc::new(GraphPool::new(pool))))
    }

    /// [`set_thread_pool`](Self::set_thread_pool) with the served pool built by
    /// the caller, for a test that gates a wrapped pool.
    #[doc(hidden)]
    pub fn set_task_pool(&mut self, pool: Option<Arc<GraphPool>>) -> Option<Arc<GraphPool>> {
        std::mem::replace(&mut self.thread_pool, pool)
    }

    /// What the Processor serves: the pool and the plugin's `exec`, `None`
    /// without a pool of two or more participants or without an `exec`.
    pub(crate) fn pool_snapshot(&self) -> Option<PoolSnapshot> {
        let pool = self.thread_pool.clone()?;
        // One participant is the caller alone: a request would run the same
        // tasks on the same thread, only after a handshake.
        if pool.participants() < 2 {
            return None;
        }
        let handle = self.plugin.pool_handle(pool)?;
        Some(PoolSnapshot { handle })
    }
}

/// A pool paired with the plugin entry it serves, published to the Processor.
pub(crate) struct PoolSnapshot {
    pub(crate) handle: PoolHandle,
}

/// What the Processor's `Reader` lends: the snapshot, if any.
#[derive(Default)]
pub(crate) struct PoolCell(pub(crate) Option<PoolSnapshot>);
