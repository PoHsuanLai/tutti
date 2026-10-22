//! `clap.thread-pool`: the host runs a plugin's tasks on the pool it was given
//! (`ClapLoaded::set_thread_pool`), and refuses every request it must.
//!
//! The reference plugin's thread-pool probe asks for N tasks per `process`
//! and records, right after `request_exec` returns, whether every index ran
//! exactly once and on how many threads (`tutti_clap_test_plugin::thread_pool`).
//! Its switches live in the image the host loaded, so they are reached across
//! the `dlopen` seam, and set before load (the extension is read then).
//!
//! The `#[global_allocator]` makes `assert_no_alloc` abort on an allocation;
//! without it the no-alloc test below would pass unconditionally.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assert_no_alloc::AllocDisabler;
use tutti_clap_host::{AudioBuffer32, ClapActive, ClapLoaded, ClapProcessContext};
use tutti_clap_test_plugin::ThreadPoolCapture;
use tutti_core::{Pool, WorkerPool};

mod support;
use support::probe_path::probe_path;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// The probe's switches are process-global; one test drives them at a time.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

const FRAMES: usize = 64;
const SAMPLE_RATE: f64 = 48_000.0;

/// The plugin image the host loads, opened once and never closed (a
/// `dlclose` before the host's load would discard the switches).
fn probe_lib() -> &'static libloading::Library {
    use std::sync::OnceLock;
    static LIB: OnceLock<libloading::Library> = OnceLock::new();
    LIB.get_or_init(|| unsafe {
        libloading::Library::new(probe_path()).expect("re-open reference plugin")
    })
}

fn switch_u32(symbol: &[u8], value: u32) {
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn(u32)> =
            probe_lib().get(symbol).expect("switch symbol present");
        f(value);
    }
}

fn switch_bool(symbol: &[u8], value: bool) {
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn(bool)> =
            probe_lib().get(symbol).expect("switch symbol present");
        f(value);
    }
}

fn capture() -> ThreadPoolCapture {
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn() -> ThreadPoolCapture> = probe_lib()
            .get(b"tutti_test_plugin_thread_pool_capture\0")
            .expect("capture symbol present");
        f()
    }
}

/// Arm the probe for `tasks` tasks of `task_us` each, and load + activate.
fn load(tasks: u32, task_us: u32, nested: bool) -> ClapActive<f32> {
    switch_u32(b"tutti_test_plugin_set_thread_pool_tasks\0", tasks);
    switch_u32(b"tutti_test_plugin_set_thread_pool_task_us\0", task_us);
    switch_bool(b"tutti_test_plugin_set_thread_pool_nested\0", nested);
    switch_bool(b"tutti_test_plugin_set_thread_pool_log\0", false);
    switch_bool(b"tutti_test_plugin_set_thread_pool_foreign\0", false);
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn()> = probe_lib()
            .get(b"tutti_test_plugin_reset_thread_pool\0")
            .expect("reset symbol present");
        f();
    }
    let path = Path::new(probe_path());
    let loaded = ClapLoaded::load_with_library(path, Some(path), SAMPLE_RATE, 512)
        .expect("reference plugin loads");
    assert!(
        loaded.supports_thread_pool(),
        "the armed probe offers clap.thread-pool"
    );
    loaded
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("reference plugin activates")
}

/// One block, on stack buffers only (so it can run inside a no-alloc gate).
fn block(inst: &mut ClapActive<f32>) {
    let (inl, inr) = ([0.0f32; FRAMES], [0.0f32; FRAMES]);
    let (mut outl, mut outr) = ([0.0f32; FRAMES], [0.0f32; FRAMES]);
    let outs: &mut [&mut [f32]] = &mut [&mut outl[..], &mut outr[..]];
    let ins: &[&[f32]] = &[&inl[..], &inr[..]];
    let mut buffer = AudioBuffer32 {
        inputs: ins,
        outputs: outs,
        num_samples: FRAMES,
        sample_rate: SAMPLE_RATE,
    };
    inst.process(&mut buffer, &ClapProcessContext::default())
        .expect("block processes");
}

/// `request_exec` with the last processing instance's host, from this thread
/// (`1` accepted, `0` refused, `-1` not offered).
fn request_now(tasks: u32) -> i32 {
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn(u32) -> i32> = probe_lib()
            .get(b"tutti_test_plugin_thread_pool_request_now\0")
            .expect("request-now symbol present");
        f(tasks)
    }
}

fn pool(participants: usize) -> Arc<WorkerPool> {
    Arc::new(WorkerPool::new(participants))
}

/// Blocks until the capture shows a request spread over several threads,
/// or a deadline (for a loaded runner, not the expected time).
fn until_spread(mut one: impl FnMut()) -> ThreadPoolCapture {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut blocks = 0;
    while (blocks < 50 || capture().spread == 0) && Instant::now() < deadline {
        one();
        blocks += 1;
    }
    capture()
}

/// With no pool, every request is refused, so the plugin runs its tasks
/// itself (`ext/thread-pool.h`: `false` means the host did not run them).
///
/// Mutation: make `request_exec` return `true` when no request is installed
/// → `accepted` is 4 and `refused` 0 → fails.
#[test]
fn without_a_pool_every_request_is_refused() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut inst = load(8, 0, false);
    for _ in 0..4 {
        block(&mut inst);
    }
    let cap = capture();
    assert!(cap.host_offers, "the host offers clap.thread-pool");
    assert_eq!((cap.requests, cap.accepted, cap.refused), (4, 0, 4));
}

/// A pool of one participant is no pool: the plugin's own fallback does as
/// well, so the request is refused.
///
/// Mutation: drop the `participants() < 2` check in `pool_request` → the
/// request is accepted (and run on the caller) → `accepted` is 4 → fails.
#[test]
fn a_pool_of_one_is_refused() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut inst = load(8, 0, false);
    inst.set_thread_pool(Some(pool(1)));
    for _ in 0..4 {
        block(&mut inst);
    }
    let cap = capture();
    assert_eq!((cap.accepted, cap.refused), (0, 4));
}

/// With a pool, every request is accepted; when `request_exec` returns,
/// every task index has run exactly once; and the pool's helpers take part.
///
/// Mutations: (a) in `Tasks::drain`, `fetch_add(2)` → odd tasks never run
/// → `accepted_incomplete` > 0 → fails; (b) have `request_exec` return
/// `true` without running the tasks → every accepted request is incomplete
/// → fails (run: after 10 s); (c) have `PoolRequest::run` run the job on the caller only
/// (`(tasks.drain)()` instead of `pool.run`) → `spread` stays 0 until the
/// deadline → fails (run: after 10 s).
#[test]
#[cfg_attr(miri, ignore = "threads and a dlopen'd plugin")]
fn a_pool_runs_every_task_once_before_the_request_returns() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut inst = load(16, 50, false);
    assert!(inst.set_thread_pool(Some(pool(4))).is_none());
    let cap = until_spread(|| block(&mut inst));
    assert!(cap.requests >= 50);
    assert_eq!(cap.accepted, cap.requests, "every request accepted");
    assert_eq!(cap.refused, 0);
    assert_eq!(cap.accepted_incomplete, 0, "a request returned early");
    assert!(
        cap.spread > 0,
        "no request's tasks reached a helper: {cap:?}"
    );
    assert!(cap.max_threads <= 4);
}

/// Taking the pool back makes the next request refused again.
///
/// Mutation: have `set_thread_pool` keep the old pool when given `None` →
/// the last block is accepted → fails.
#[test]
fn removing_the_pool_refuses_again() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut inst = load(4, 0, false);
    inst.set_thread_pool(Some(pool(2)));
    block(&mut inst);
    assert!(inst.set_thread_pool(None).is_some());
    block(&mut inst);
    let cap = capture();
    assert_eq!((cap.accepted, cap.refused), (1, 1));
}

/// A request from inside a task — on a helper, or on the caller while its
/// request runs — is refused: the header says it "can't be called ... from
/// the thread pool".
///
/// Mutation: in `request_exec`, leave the request installed while the tasks
/// run (drop `_taken`) → task 0, on the caller, has its nested call accepted
/// → `nested_accepted` > 0 → fails.
#[test]
#[cfg_attr(miri, ignore = "threads and a dlopen'd plugin")]
fn a_request_from_inside_a_task_is_refused() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut inst = load(8, 0, true);
    inst.set_thread_pool(Some(pool(4)));
    for _ in 0..50 {
        block(&mut inst);
    }
    let cap = capture();
    assert_eq!(cap.accepted, 50);
    assert_eq!(cap.nested_refused, 50, "{cap:?}");
    assert_eq!(cap.nested_accepted, 0);
}

/// The plugin's `process` called from a participant of a job already
/// running on the same pool — a parallel graph block's worker — has its
/// request refused rather than waiting on the job it was made from: the
/// block returns (no deadlock), and the plugin runs its tasks itself.
///
/// Mutation: have `WorkerPool::try_run` run the job whatever the gate
/// (`self.run(wake, job); true`) → a nested request is accepted and run on
/// the caller alone → every request is accepted → fails.
#[test]
#[cfg_attr(miri, ignore = "threads and a dlopen'd plugin")]
fn a_request_from_a_pool_worker_is_refused() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let shared = pool(4);
    let mut inst = load(16, 0, false);
    inst.set_thread_pool(Some(shared.clone() as Arc<dyn Pool>));
    let slot = Mutex::new(&mut inst);
    let processed = std::sync::atomic::AtomicU32::new(0);
    for _ in 0..50 {
        // The outer job stands for a graph block: the participant that
        // takes the plugin runs its `process` inside the job.
        shared.run(4, &|_| {
            if let Ok(mut inst) = slot.try_lock() {
                block(&mut inst);
                processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    let cap = capture();
    assert_eq!(processed.into_inner(), cap.requests);
    assert!(cap.requests >= 50, "{cap:?}");
    assert_eq!((cap.accepted, cap.refused), (0, cap.requests), "{cap:?}");
}

/// A request naming another instance's host is refused, even from inside
/// this instance's `process` on the thread serving its request.
///
/// Mutation: drop the `ptr::eq(request.host, host)` check in `request_exec`
/// → both foreign requests are accepted → `accepted` is 3 → fails.
#[test]
fn a_request_naming_another_instance_is_refused() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut a = load(4, 0, false);
    let mut b = load(4, 0, false);
    a.set_thread_pool(Some(pool(2)));
    b.set_thread_pool(Some(pool(2)));
    block(&mut b);
    switch_bool(b"tutti_test_plugin_set_thread_pool_foreign\0", true);
    // `a` names `b`'s host, then `b` names `a`'s.
    block(&mut a);
    block(&mut b);
    switch_bool(b"tutti_test_plugin_set_thread_pool_foreign\0", false);
    let cap = capture();
    assert_eq!(
        (cap.requests, cap.accepted, cap.refused),
        (3, 1, 2),
        "{cap:?}"
    );
}

/// A request made after `process` returned is refused, even on the thread
/// that called `process` and with the instance's own host: the request is
/// served only for the span of the call.
///
/// Mutation: drop the `current.is_null()` check in `request_exec` → the
/// call dereferences null → panics, aborting the process → fails. (A
/// thread-local left pointing at the finished request would be a dangling
/// read this test cannot observe reliably; the unit test
/// `a_swap_restores_the_previous_request_on_return_and_unwind` covers it.)
#[test]
fn a_request_after_process_returns_is_refused() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut inst = load(4, 0, false);
    inst.set_thread_pool(Some(pool(2)));
    block(&mut inst);
    assert_eq!(capture().accepted, 1, "the block itself is served");
    assert_eq!(request_now(4), 0, "a request outside `process` is refused");
    let from_another = std::thread::spawn(|| request_now(4)).join().unwrap();
    assert_eq!(from_another, 0, "and from another thread");
}

/// The pool, gated: every participant's share of a job runs inside
/// `assert_no_alloc`, helpers included.
struct GatedPool(WorkerPool);

impl Pool for GatedPool {
    fn participants(&self) -> usize {
        self.0.participants()
    }
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) {
        self.0
            .run(wake, &|i| assert_no_alloc::assert_no_alloc(|| job(i)));
    }
    fn try_run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) -> bool {
        self.0
            .try_run(wake, &|i| assert_no_alloc::assert_no_alloc(|| job(i)))
    }
    fn waits(&self) -> tutti_core::BlockWaits {
        self.0.waits()
    }
}

fn gated(participants: usize) -> Arc<GatedPool> {
    Arc::new(GatedPool(
        WorkerPool::builder(participants)
            .spin(Duration::from_micros(1))
            .idle(Duration::from_micros(1))
            .build(),
    ))
}

/// Serving requests allocates nothing on the audio thread or on the
/// helpers that run the tasks.
///
/// Mutation: box the task counter in `PoolRequest::run`
/// (`Box::new(Tasks { .. })`) → the gate aborts the process.
#[test]
#[cfg_attr(miri, ignore = "threads and a dlopen'd plugin")]
fn serving_requests_does_not_allocate() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut inst = load(16, 20, false);
    inst.set_thread_pool(Some(gated(4)));
    // Warm-up outside the gate: the first block starts processing.
    block(&mut inst);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            block(&mut inst);
        }
    });
    let cap = capture();
    assert_eq!(cap.accepted, 201);
    assert_eq!(cap.accepted_incomplete, 0);
}

/// A task that logs through `clap.log` is held to the audio thread's rules on
/// whichever thread runs it, since the audio thread waits for it: the line
/// is counted as dropped, not formatted, printed and kept (which allocates
/// and locks).
///
/// Mutation: drop `in_served_task()` from `host_log`'s check → a helper's
/// line allocates inside the gate → the process aborts → fails.
#[test]
#[cfg_attr(miri, ignore = "threads and a dlopen'd plugin")]
fn a_task_that_logs_does_not_allocate() {
    let _l = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut inst = load(16, 20, false);
    inst.set_thread_pool(Some(gated(4)));
    block(&mut inst);
    switch_bool(b"tutti_test_plugin_set_thread_pool_log\0", true);
    let dropped = inst.log_lines_dropped();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            block(&mut inst);
        }
    });
    switch_bool(b"tutti_test_plugin_set_thread_pool_log\0", false);
    let cap = capture();
    assert_eq!(cap.accepted, 201);
    assert!(cap.spread > 0, "no task reached a helper: {cap:?}");
    assert_eq!(inst.log_lines_dropped() - dropped, 200 * 16);
}
