//! `clap.thread-pool`: which pool an instance serves its plugin's requests
//! on. The serving itself is `host/thread_pool.rs`.

use std::sync::Arc;

use tutti_graph::Pool;

use super::ClapLoaded;
use crate::host::{ExecFn, PoolRequest};

impl ClapLoaded {
    /// Whether the plugin implements `clap.thread-pool` (it has tasks it can
    /// spread over a host's pool).
    #[must_use]
    pub fn supports_thread_pool(&self) -> bool {
        self.thread_pool_exec().is_some()
    }

    /// Serve the plugin's `clap.thread-pool` requests on `pool`, or refuse
    /// them (`None`, the default). Returns the pool it replaces.
    ///
    /// A request made from the plugin's `process` then runs its tasks on
    /// `pool` — the calling thread taking part — and returns once all have
    /// run. When `pool` is busy (the call comes from a participant of a job
    /// already running on it, a graph block), the request is refused
    /// ([`Pool::try_run`]) rather than waiting on that job. A pool of one
    /// participant is treated as none: the plugin's own fallback serves it as
    /// well.
    ///
    /// With no pool every request returns `false`, which `ext/thread-pool.h`
    /// tells the plugin to answer by running the tasks itself; that is
    /// never worse than this host running them inline, and a plugin with
    /// threads of its own may do better.
    ///
    /// Control thread. Dropping the returned pool may join its threads.
    pub fn set_thread_pool(&mut self, pool: Option<Arc<dyn Pool>>) -> Option<Arc<dyn Pool>> {
        std::mem::replace(&mut self.thread_pool, pool)
    }

    /// What the Processor serves requests with, fixed at activation: the pool
    /// and the plugin's `exec`, or `None` when there is nothing to serve them
    /// with (no pool, a pool of one participant, or no `exec`).
    pub(crate) fn pool_snapshot(&self) -> Option<PoolSnapshot> {
        let pool = self.thread_pool.clone()?;
        if pool.participants() < 2 {
            return None;
        }
        let exec = self.thread_pool_exec()?;
        Some(PoolSnapshot { pool, exec })
    }

    /// The plugin's `clap_plugin_thread_pool::exec`, if it has one.
    fn thread_pool_exec(&self) -> Option<ExecFn> {
        let ext = self.extensions.system.thread_pool;
        if ext.is_null() {
            return None;
        }
        // SAFETY: a non-null extension pointer from the plugin's
        // `get_extension` stays valid for the instance's life.
        unsafe { (*ext).exec }
    }
}

/// The pool a Processor serves `clap.thread-pool` requests on, and the plugin's
/// `exec`.
pub(crate) struct PoolSnapshot {
    pool: Arc<dyn Pool>,
    exec: ExecFn,
}

/// What the Controller publishes for the Processor to lend once per block: the
/// pool to serve requests on, or nothing to refuse them.
#[derive(Default)]
pub(crate) struct PoolCell(pub(crate) Option<PoolSnapshot>);

impl PoolCell {
    /// The request one `process` call serves around the plugin's `process`, or
    /// `None` when there is nothing to serve requests with.
    pub(crate) fn request(
        &self,
        host_state: &crate::host::HostState,
        plugin: *const clap_sys::plugin::clap_plugin,
    ) -> Option<PoolRequest> {
        let snapshot = self.0.as_ref()?;
        Some(PoolRequest::new(
            host_state,
            &*snapshot.pool,
            plugin,
            snapshot.exec,
        ))
    }
}
