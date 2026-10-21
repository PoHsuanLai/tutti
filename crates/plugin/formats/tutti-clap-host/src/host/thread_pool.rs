//! Serving `clap.thread-pool`: a plugin's `request_exec(host, n)` from inside
//! its `process` runs `exec(plugin, 0..n)` on the [`Pool`] the instance was
//! given (`ClapLoaded::set_thread_pool`), and returns once every task has run.
//!
//! # Which calls are served
//!
//! `ext/thread-pool.h` allows the call only from the audio thread, inside
//! `process`, and never from the pool; the host "should check that the
//! plugin is within the process call, and if not, reject". The check is a
//! thread-local: `do_process` serves a [`PoolRequest`] on its own thread
//! for exactly the span of the plugin's `process` call, and `request_exec`
//! serves a call only on the thread serving one, for the same instance. Every other call — outside `process`, on another thread (the
//! pool's or the plugin's own), from another instance, or from inside a task
//! (the request is taken while it runs) — is refused with `false`, and the
//! plugin runs its tasks itself, as the header tells it to.
//!
//! # Running the tasks
//!
//! The pool runs one job; each participant claims task indices from a shared
//! counter until none is left, so every index runs exactly once whatever
//! the number of participants, one included. [`Pool::run`] returns only
//! once no participant is inside the job, so `request_exec` returns after
//! the last task, and every task's writes happen-before the plugin reads
//! them (the pool's contract).
//!
//! The pool may be busy: in process, the caller can itself be a participant
//! of a graph block running on the same pool. The request is then refused
//! ([`Pool::try_run`]), as with no pool: the plugin's own fallback is never
//! worse than running the tasks alone on the caller, and the request never
//! waits on the job it was made from.
//!
//! A served task runs on the plugin's behalf for the audio thread, which
//! waits for it, so callbacks that hold back work on the audio thread
//! (`clap.log`) hold it back in a served task too ([`in_served_task`]), on
//! whichever thread runs it.
//!
//! Nothing here allocates, locks or frees: the request lives on
//! `do_process`'s stack, the counter on `request_exec`'s.

use std::cell::Cell;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use clap_sys::plugin::clap_plugin;
use tutti_graph::Pool;
use tutti_types::ScopedNoDenormals;

use super::HostState;

/// The plugin's `clap_plugin_thread_pool::exec`.
pub(crate) type ExecFn = unsafe extern "C" fn(plugin: *const clap_plugin, task_index: u32);

/// What one `process` call lends its `request_exec` calls: the instance's
/// pool, plugin and `exec`.
pub(crate) struct PoolRequest {
    /// The instance's host state, which `request_exec` matches against the
    /// `host_data` it is called with.
    host: *const HostState,
    pool: *const dyn Pool,
    plugin: *const clap_plugin,
    exec: ExecFn,
}

thread_local! {
    /// The request this thread's current `process` call serves, or null.
    /// `const`-initialised with no destructor, so reading it allocates
    /// nothing.
    static CURRENT: Cell<*const PoolRequest> = const { Cell::new(ptr::null()) };
    /// Whether this thread is running a served task.
    static IN_TASK: Cell<bool> = const { Cell::new(false) };
}

/// Whether the calling thread is running a task of a served `clap.thread-pool`
/// request: the audio thread waits for it, so it is held to the audio
/// thread's rules. Allocation-free.
pub(crate) fn in_served_task() -> bool {
    IN_TASK.with(Cell::get)
}

impl PoolRequest {
    /// A request for `plugin`'s `exec` on `pool`, for the instance whose
    /// host state is `host`. Every pointer must stay valid while the
    /// request is served.
    pub(crate) fn new(
        host: &HostState,
        pool: &dyn Pool,
        plugin: *const clap_plugin,
        exec: ExecFn,
    ) -> Self {
        Self {
            host,
            pool,
            plugin,
            exec,
        }
    }

    /// Run `process` (the plugin's `process` call) with this as the request
    /// `request_exec` serves on this thread, and stop serving it when
    /// `process` returns or unwinds.
    pub(crate) fn serve<R>(&self, process: impl FnOnce() -> R) -> R {
        let _served = Swap::new(self);
        process()
    }

    /// Run `exec(plugin, 0..num_tasks)` on the pool and return when all
    /// have run, or return `false` at once, having run none, when the pool
    /// is busy.
    fn run(&self, num_tasks: u32) -> bool {
        if num_tasks == 0 {
            return true;
        }
        let tasks = Tasks {
            next: AtomicU64::new(0),
            num_tasks: u64::from(num_tasks),
            plugin: self.plugin,
            exec: self.exec,
        };
        // SAFETY: the pool outlives `serve` (the instance owns it and is
        // borrowed for the whole `process` call), and `run` is reached only
        // through a request being served (`request_exec`).
        let pool = unsafe { &*self.pool };
        let wake = (num_tasks as usize).min(pool.participants());
        pool.try_run(wake, &|_| {
            // The audio thread's float mode, and its rules for callbacks, on
            // every participant: helpers run the plugin's DSP on its behalf.
            let _ftz = ScopedNoDenormals::new();
            let _task = InTask::enter();
            tasks.drain();
        })
    }
}

/// Marks this thread as running a served task until dropped.
struct InTask(bool);

impl InTask {
    fn enter() -> Self {
        Self(IN_TASK.with(|t| t.replace(true)))
    }
}

impl Drop for InTask {
    fn drop(&mut self) {
        IN_TASK.with(|t| t.set(self.0));
    }
}

/// Sets this thread's served request, restoring the previous one on drop
/// (return or unwind). Private and never forgotten: only [`PoolRequest::serve`]
/// and [`request_exec`] make one, each on its own stack for one call.
struct Swap {
    prev: *const PoolRequest,
}

impl Swap {
    fn new(to: *const PoolRequest) -> Self {
        Self {
            prev: CURRENT.with(|c| c.replace(to)),
        }
    }
}

impl Drop for Swap {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.prev));
    }
}

/// The tasks of one request, claimed by index.
struct Tasks {
    /// The next unclaimed index. 64-bit so that `num_tasks` near `u32::MAX`
    /// plus one overshoot per participant cannot wrap.
    next: AtomicU64,
    num_tasks: u64,
    plugin: *const clap_plugin,
    exec: ExecFn,
}

// SAFETY: `exec` is the entry point CLAP defines for the host's pool threads
// to call, concurrently, during the plugin's `process` (`ext/thread-pool.h`);
// `plugin` is valid for that whole call, which `request_exec` is inside. The
// counter is an atomic.
unsafe impl Sync for Tasks {}

impl Tasks {
    /// Claim and run tasks until none is left.
    fn drain(&self) {
        loop {
            // `Relaxed`: the counter only hands out distinct indices; the
            // tasks' own writes are ordered by the pool's return.
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            if i >= self.num_tasks {
                return;
            }
            // SAFETY: see `impl Sync for Tasks`; `i < num_tasks <= u32::MAX`.
            unsafe { (self.exec)(self.plugin, i as u32) };
        }
    }
}

/// `clap_host_thread_pool::request_exec` for the instance whose host state
/// is `host`: whether every task ran (module docs).
pub(crate) fn request_exec(host: *const HostState, num_tasks: u32) -> bool {
    let current = CURRENT.with(Cell::get);
    if current.is_null() {
        return false;
    }
    // SAFETY: only a `Swap` sets the thread-local to non-null, from `serve`,
    // to a request that outlives the `serve` call; the `Swap` lives on
    // `serve`'s stack, is never forgotten, and restores the previous value
    // when `serve` returns or unwinds, on this same thread. So a non-null
    // value is a request still being served.
    let request = unsafe { &*current };
    if !ptr::eq(request.host, host) {
        return false;
    }
    // Taken while the tasks run, so a call from inside one is refused.
    let _taken = Swap::new(ptr::null());
    request.run(num_tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> *const PoolRequest {
        CURRENT.with(Cell::get)
    }

    /// The served request is cleared when the span ends, by return or by
    /// unwind, so a later `request_exec` on this thread finds nothing (the
    /// integration test `a_request_after_process_returns_is_refused` covers
    /// the call; a stale pointer there would be a dangling read it cannot
    /// observe reliably).
    ///
    /// Mutation: make `Swap::drop` restore nothing → the inner span leaves
    /// null where the outer request should be back → fails.
    #[test]
    fn a_swap_restores_the_previous_request_on_return_and_unwind() {
        let fake = std::ptr::NonNull::<PoolRequest>::dangling().as_ptr() as *const PoolRequest;
        {
            let _s = Swap::new(fake);
            assert_eq!(current(), fake);
            {
                let _taken = Swap::new(ptr::null());
                assert!(current().is_null());
            }
            assert_eq!(current(), fake, "the inner span restores the outer");
        }
        assert!(current().is_null());
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _s = Swap::new(fake);
            panic!("unwind through the span");
        }));
        assert!(unwound.is_err());
        assert!(current().is_null(), "an unwind restores too");
    }
}
