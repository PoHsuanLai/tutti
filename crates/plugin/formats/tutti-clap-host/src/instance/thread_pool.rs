//! `clap.thread-pool`: which pool an instance serves its plugin's requests
//! on. The serving itself is `host/thread_pool.rs`.

use std::sync::Arc;

use tutti_graph::Pool;

use super::ClapLoaded;
use crate::host::PoolRequest;

impl ClapLoaded {
    /// Whether the plugin implements `clap.thread-pool` (it has tasks it can
    /// spread over a host's pool).
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

    /// The request a `process` call serves around the plugin's `process`,
    /// or `None` when there is nothing to serve requests with.
    pub(crate) fn pool_request(&self) -> Option<PoolRequest> {
        let pool = self.thread_pool.as_deref()?;
        if pool.participants() < 2 {
            return None;
        }
        let exec = self.thread_pool_exec()?;
        Some(PoolRequest::new(
            &self.host_state,
            pool,
            self.plugin.as_ptr(),
            exec,
        ))
    }

    /// The plugin's `clap_plugin_thread_pool::exec`, if it has one.
    fn thread_pool_exec(&self) -> Option<crate::host::ExecFn> {
        let ext = self.extensions.system.thread_pool;
        if ext.is_null() {
            return None;
        }
        // SAFETY: a non-null extension pointer from the plugin's
        // `get_extension` stays valid for the instance's life.
        unsafe { (*ext).exec }
    }
}
