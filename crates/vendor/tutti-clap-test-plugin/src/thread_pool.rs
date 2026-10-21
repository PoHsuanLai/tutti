//! THREAD-POOL probe: `clap.thread-pool`, from the plugin's side.
//!
//! Off by default: the plugin advertises `clap.thread-pool` only while a test
//! has asked for tasks ([`set_thread_pool_tasks`]), so a host that gives such
//! plugins a pool builds none for the other suites. Set it **before load**,
//! when the host reads the extension.
//!
//! Each `process` then asks the host to run that many tasks
//! (`request_exec`) and, like any plugin, runs them itself when the host
//! says no. Each task counts its runs and notes the thread it ran on, so the
//! capture can say whether every index ran exactly once *by the time
//! `request_exec` returned*, and on how many threads. A task can be made to
//! busy-wait ([`set_thread_pool_task_us`]) so helpers that are waking have
//! work left to join, and task 0 can call `request_exec` itself
//! ([`set_thread_pool_nested`]), which CLAP says a host must refuse. Each
//! task can log through `clap.log` ([`set_thread_pool_log`]), which CLAP
//! allows from any thread and a host must not allocate for while the audio
//! thread waits on the task. A request can be made with the host of the
//! instance that processed before this one ([`set_thread_pool_foreign`]), or
//! from outside `process` ([`thread_pool_request_now`]); a host must refuse
//! both.
//!
//! [`set_thread_pool_tasks`]: crate::thread_pool::tutti_test_plugin_set_thread_pool_tasks
//! [`set_thread_pool_task_us`]: crate::thread_pool::tutti_test_plugin_set_thread_pool_task_us
//! [`set_thread_pool_nested`]: crate::thread_pool::tutti_test_plugin_set_thread_pool_nested
//! [`set_thread_pool_log`]: crate::thread_pool::tutti_test_plugin_set_thread_pool_log
//! [`set_thread_pool_foreign`]: crate::thread_pool::tutti_test_plugin_set_thread_pool_foreign
//! [`thread_pool_request_now`]: crate::thread_pool::tutti_test_plugin_thread_pool_request_now
//!
//! Lock-free and allocation-free on the audio thread and in the tasks: the
//! host's no-alloc gates run around both.

use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clap_sys::ext::log::{clap_host_log, CLAP_EXT_LOG, CLAP_LOG_INFO};
use clap_sys::ext::thread_pool::{
    clap_host_thread_pool, clap_plugin_thread_pool, CLAP_EXT_THREAD_POOL,
};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;

/// The most tasks one request records. A request for more runs them all
/// but counts only these.
pub const MAX_POOL_TASKS: usize = 64;

/// Tasks each `process` requests. `0` = the probe is off.
static TASKS: AtomicU32 = AtomicU32::new(0);
/// Microseconds each task busy-waits.
static TASK_US: AtomicU32 = AtomicU32::new(0);
/// Whether task 0 makes a nested `request_exec`.
static NESTED: AtomicBool = AtomicBool::new(false);
/// Whether every task logs one line through `clap.log`.
static LOG: AtomicBool = AtomicBool::new(false);
/// Whether `process` requests with the previous instance's host.
static FOREIGN: AtomicBool = AtomicBool::new(false);
/// The host of the instance that last called `process`.
static LAST_HOST: AtomicPtr<clap_host> = AtomicPtr::new(std::ptr::null_mut());

/// The line a task logs.
const TASK_LOG_LINE: &CStr = c"thread-pool probe: a task logged";

/// Runs of each task in the current request.
static RUNS: [AtomicU32; MAX_POOL_TASKS] = [const { AtomicU32::new(0) }; MAX_POOL_TASKS];
/// The thread each task last ran on (a per-thread address).
static THREADS: [AtomicUsize; MAX_POOL_TASKS] = [const { AtomicUsize::new(0) }; MAX_POOL_TASKS];

/// What every `process` since the last reset saw. `#[repr(C)]` so a test
/// reads it across the dlopen seam.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadPoolCapture {
    /// `process` calls that made a request.
    pub requests: u32,
    /// Requests the host accepted (`request_exec` returned `true`).
    pub accepted: u32,
    /// Requests the host refused; the plugin then ran the tasks itself.
    pub refused: u32,
    /// Accepted requests after which some task had not run exactly once.
    pub accepted_incomplete: u32,
    /// The most distinct threads one accepted request's tasks ran on.
    pub max_threads: u32,
    /// Accepted requests whose tasks ran on more than one thread.
    pub spread: u32,
    /// Task runs on the thread that called `process`.
    pub runs_on_caller: u32,
    /// Nested `request_exec` calls (from inside a task) the host accepted.
    pub nested_accepted: u32,
    /// Nested calls the host refused.
    pub nested_refused: u32,
    /// Whether the host offered `clap.thread-pool` at all.
    pub host_offers: bool,
}

static REQUESTS: AtomicU32 = AtomicU32::new(0);
static ACCEPTED: AtomicU32 = AtomicU32::new(0);
static REFUSED: AtomicU32 = AtomicU32::new(0);
static INCOMPLETE: AtomicU32 = AtomicU32::new(0);
static MAX_THREADS: AtomicU32 = AtomicU32::new(0);
static SPREAD: AtomicU32 = AtomicU32::new(0);
static RUNS_ON_CALLER: AtomicU32 = AtomicU32::new(0);
static NESTED_ACCEPTED: AtomicU32 = AtomicU32::new(0);
static NESTED_REFUSED: AtomicU32 = AtomicU32::new(0);
static HOST_OFFERS: AtomicBool = AtomicBool::new(false);
/// The thread inside `process`, while it is.
static CALLER: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static MARKER: u8 = const { 0 };
}

/// A per-thread address, without `thread::current()` (which may allocate on
/// a thread std did not start).
fn this_thread() -> usize {
    MARKER.with(|m| m as *const u8 as usize)
}

static PLUGIN_THREAD_POOL: clap_plugin_thread_pool = clap_plugin_thread_pool { exec: Some(exec) };

/// `clap.thread-pool` while the probe is on, else null.
pub(crate) fn get_extension(id: &CStr) -> *const c_void {
    if id == CLAP_EXT_THREAD_POOL && TASKS.load(Ordering::SeqCst) > 0 {
        return &PLUGIN_THREAD_POOL as *const _ as *const c_void;
    }
    std::ptr::null()
}

/// The host's `clap_host_thread_pool`, or null.
///
/// # Safety
/// `host` must be null or a live `clap_host`.
unsafe fn host_thread_pool(host: *const clap_host) -> *const clap_host_thread_pool {
    if host.is_null() {
        return std::ptr::null();
    }
    match (*host).get_extension {
        Some(get) => get(host, CLAP_EXT_THREAD_POOL.as_ptr()) as *const clap_host_thread_pool,
        None => std::ptr::null(),
    }
}

/// `clap_plugin_thread_pool::exec`.
unsafe extern "C" fn exec(plugin: *const clap_plugin, task_index: u32) {
    let me = this_thread();
    if me == CALLER.load(Ordering::Relaxed) {
        RUNS_ON_CALLER.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(runs) = RUNS.get(task_index as usize) {
        runs.fetch_add(1, Ordering::Relaxed);
        THREADS[task_index as usize].store(me, Ordering::Relaxed);
    }
    if task_index == 0 && NESTED.load(Ordering::Relaxed) {
        let tp = host_thread_pool(crate::plugin_host(plugin));
        if let Some(request) = tp.as_ref().and_then(|tp| tp.request_exec) {
            // The nested call asks for no tasks, so an accepting host has
            // nothing to run and the outer request's counts stay exact.
            if request(crate::plugin_host(plugin), 0) {
                NESTED_ACCEPTED.fetch_add(1, Ordering::Relaxed);
            } else {
                NESTED_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if LOG.load(Ordering::Relaxed) {
        log_from_task(crate::plugin_host(plugin));
    }
    let us = TASK_US.load(Ordering::Relaxed);
    if us > 0 {
        let t = Instant::now();
        while t.elapsed() < Duration::from_micros(u64::from(us)) {
            std::hint::spin_loop();
        }
    }
}

/// One `clap.log` line from a task, if the host offers the extension.
///
/// # Safety
/// `host` must be null or a live `clap_host`.
unsafe fn log_from_task(host: *const clap_host) {
    if host.is_null() {
        return;
    }
    let Some(get) = (*host).get_extension else {
        return;
    };
    let log = get(host, CLAP_EXT_LOG.as_ptr()) as *const clap_host_log;
    if let Some(log) = log.as_ref().and_then(|l| l.log) {
        log(host, CLAP_LOG_INFO, TASK_LOG_LINE.as_ptr());
    }
}

/// From `process`: make this block's request, if the probe is on.
///
/// # Safety
/// `plugin` and `host` are this instance's, inside its `process`.
pub(crate) unsafe fn on_process(plugin: *const clap_plugin, host: *const clap_host) {
    let n = TASKS.load(Ordering::Relaxed);
    if n == 0 {
        return;
    }
    CALLER.store(this_thread(), Ordering::Relaxed);
    let recorded = (n as usize).min(MAX_POOL_TASKS);
    for runs in &RUNS[..recorded] {
        runs.store(0, Ordering::Relaxed);
    }
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    let prev = LAST_HOST.swap(host as *mut clap_host, Ordering::Relaxed);
    // The host the request names: this instance's, or (foreign) the previous
    // caller's, which is another live instance while the test keeps it.
    let named = if FOREIGN.load(Ordering::Relaxed) && !prev.is_null() {
        prev as *const clap_host
    } else {
        host
    };
    let tp = host_thread_pool(host);
    let request = tp.as_ref().and_then(|tp| tp.request_exec);
    HOST_OFFERS.store(request.is_some(), Ordering::Relaxed);
    let accepted = match request {
        Some(request) => request(named, n),
        None => false,
    };
    if accepted {
        ACCEPTED.fetch_add(1, Ordering::Relaxed);
        // Read right after the return: the header's "will block until all
        // the tasks are processed".
        if RUNS[..recorded]
            .iter()
            .any(|r| r.load(Ordering::Relaxed) != 1)
        {
            INCOMPLETE.fetch_add(1, Ordering::Relaxed);
        }
        let mut seen = [0usize; MAX_POOL_TASKS];
        let mut distinct = 0;
        for t in &THREADS[..recorded] {
            let t = t.load(Ordering::Relaxed);
            if !seen[..distinct].contains(&t) {
                seen[distinct] = t;
                distinct += 1;
            }
        }
        MAX_THREADS.fetch_max(distinct as u32, Ordering::Relaxed);
        if distinct > 1 {
            SPREAD.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        // What the header tells a plugin to do: run them itself.
        REFUSED.fetch_add(1, Ordering::Relaxed);
        for i in 0..n {
            exec(plugin, i);
        }
    }
    CALLER.store(0, Ordering::Relaxed);
}

/// Ask for `tasks` tasks per `process` (`0` turns the probe off, and with it
/// the extension). Set before load.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_thread_pool_tasks(tasks: u32) {
    TASKS.store(tasks, Ordering::SeqCst);
}

/// Make each task busy-wait `us` microseconds.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_thread_pool_task_us(us: u32) {
    TASK_US.store(us, Ordering::SeqCst);
}

/// Make task 0 call `request_exec` from inside the request.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_thread_pool_nested(nested: bool) {
    NESTED.store(nested, Ordering::SeqCst);
}

/// Make every task log one line through the host's `clap.log`.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_thread_pool_log(log: bool) {
    LOG.store(log, Ordering::SeqCst);
}

/// Make each `process` name, in its request, the host of the instance that
/// called `process` before it (its own, if it was the last).
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
/// While it is on, the instance that processed last must stay loaded.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_thread_pool_foreign(foreign: bool) {
    FOREIGN.store(foreign, Ordering::SeqCst);
}

/// Call `request_exec(host, tasks)` now, from the calling thread, with the
/// host of the instance that last called `process`: `1` accepted, `0`
/// refused, `-1` no host or no `request_exec`. Tasks it gets run are
/// counted as any others.
///
/// # Safety
/// The instance that last called `process` must still be loaded.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_thread_pool_request_now(tasks: u32) -> i32 {
    let host = LAST_HOST.load(Ordering::SeqCst) as *const clap_host;
    let tp = host_thread_pool(host);
    match tp.as_ref().and_then(|tp| tp.request_exec) {
        Some(request) => i32::from(request(host, tasks)),
        None => -1,
    }
}

/// Zero the capture.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_reset_thread_pool() {
    for c in [
        &REQUESTS,
        &ACCEPTED,
        &REFUSED,
        &INCOMPLETE,
        &MAX_THREADS,
        &SPREAD,
        &RUNS_ON_CALLER,
        &NESTED_ACCEPTED,
        &NESTED_REFUSED,
    ] {
        c.store(0, Ordering::SeqCst);
    }
    HOST_OFFERS.store(false, Ordering::SeqCst);
    LAST_HOST.store(std::ptr::null_mut(), Ordering::SeqCst);
}

/// What the probe saw since the last reset.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_thread_pool_capture() -> ThreadPoolCapture {
    ThreadPoolCapture {
        requests: REQUESTS.load(Ordering::SeqCst),
        accepted: ACCEPTED.load(Ordering::SeqCst),
        refused: REFUSED.load(Ordering::SeqCst),
        accepted_incomplete: INCOMPLETE.load(Ordering::SeqCst),
        max_threads: MAX_THREADS.load(Ordering::SeqCst),
        spread: SPREAD.load(Ordering::SeqCst),
        runs_on_caller: RUNS_ON_CALLER.load(Ordering::SeqCst),
        nested_accepted: NESTED_ACCEPTED.load(Ordering::SeqCst),
        nested_refused: NESTED_REFUSED.load(Ordering::SeqCst),
        host_offers: HOST_OFFERS.load(Ordering::SeqCst),
    }
}
