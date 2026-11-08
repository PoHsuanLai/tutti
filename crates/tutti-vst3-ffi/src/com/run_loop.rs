//! `Linux::IRunLoop` — the event loop a Linux host must lend the plugin.
//!
//! X11 has no ambient run loop the way Cocoa and Win32 do, so VST3 makes the
//! host provide one. The plugin registers its X connection's file descriptor
//! and any repeating timers with the host, which calls back when they are ready.
//!
//! ## Why this lives in its own module
//!
//! Two different host objects must answer `IRunLoop`, and they must answer with
//! the *same* loop:
//!
//! - [`HostApplication`](super::HostApplication) — handed to
//!   `IPluginFactory3::setHostContext` and to `IPluginBase::initialize`. This is
//!   the one that actually matters: the SDK installs a host-context callback
//!   (`public.sdk/source/vst/vstguieditor.cpp`) which casts the context to
//!   `IRunLoop` and pushes it into VSTGUI's `LinuxFactory` at **factory-load
//!   time**, long before any editor exists.
//! - [`HostPlugFrame`](super::HostPlugFrame) — handed to `IPlugView::setFrame`.
//!   Some toolkits look here instead, so it is answered here too.
//!
//! Registering on one and pumping the other would leave handlers in a queue
//! nothing services, so both delegate to one [`RunLoop`] owned at library
//! scope and shared by `Arc`.
//!
//! ## Why the host context is the load-bearing one
//!
//! VSTGUI's `X11::Frame::Frame` (`x11frame.cpp`) calls `RunLoop::init()` and
//! only *then* installs the frame-provided loop as a fallback — but `init()`
//! immediately dereferences `LinuxFactory::getRunLoop()`. If the host context
//! never supplied one, that dereference is null and the plugin segfaults inside
//! `IPlugView::attached`, on the first editor open. Providing `IRunLoop` on the
//! plug frame alone does not prevent it: the frame's copy is consumed one line
//! too late. This was confirmed by suppressing exactly that capability in
//! Steinberg's own `editorhost`, which then crashed identically.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tutti_plugin_types::MainThread;

use vst3::Steinberg::Linux::{FileDescriptor, IEventHandler, ITimerHandler, TimerInterval};
use vst3::Steinberg::tresult;
use vst3::Steinberg::{kResultFalse, kResultOk};

/// Which plugin instance a registration belongs to.
///
/// The module's run loop is shared by every plugin made from it, and the
/// handlers it holds are raw pointers into the plugin that registered them. A
/// plugin that leaves one registered when its editor closes (or when it is
/// dropped) would have the next [`RunLoop::run_iteration`], on behalf of any
/// plugin of the module, call into freed memory. The owner lets
/// [`RunLoop::purge`] take them back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Owner(u64);

impl Owner {
    /// A new owner, distinct from every other in the process.
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A timer the plugin asked the host to run, and when it last fired.
struct TimerEntry {
    handler: *mut ITimerHandler,
    owner: Option<Owner>,
    period: Duration,
    last_fired: Instant,
}

/// A file descriptor handler the plugin registered.
struct FdEntry {
    handler: *mut IEventHandler,
    owner: Option<Owner>,
}

/// Registered handlers. Raw pointers are plugin-owned COM objects; they are
/// dereferenced only from [`RunLoop::run_iteration`], which the host calls on
/// its UI thread — the same thread that registered them.
#[derive(Default)]
struct State {
    /// fd → plugin event handler, invoked when the fd becomes readable.
    event_handlers: HashMap<i32, FdEntry>,
    /// Repeating timers, keyed by handler address.
    timers: HashMap<usize, TimerEntry>,
    /// Whose code is running, for a registration that arrives through the
    /// module's own `IRunLoop` (the factory's host context), which cannot say
    /// which plugin it serves: see [`RunLoop::scope`].
    scope: Option<Owner>,
    /// Dispatch tallies. Test-only: see [`RunLoopActivity`].
    #[cfg(feature = "conformance")]
    timers_fired: u64,
    #[cfg(feature = "conformance")]
    fds_dispatched: u64,
}

// SAFETY: see `RunLoop` — the pointers are only dereferenced from
// `run_iteration` on the UI thread, after being re-checked as registered, and
// the Mutex serialises access to the collections themselves.
unsafe impl Send for State {}

/// The host's run loop, shared between every object that exposes `IRunLoop`.
///
/// Handlers are plugin-owned COM objects registered through `IRunLoop` (by the
/// host application and the plug frame, which hold this loop). The loop calls
/// them from [`run_iteration`](Self::run_iteration), and the VST3 contract is
/// that the plugin keeps a handler alive until it unregisters it and that all
/// of this happens on the UI thread. The registration entries are therefore
/// crate-private and `unsafe`; the type itself is `Send + Sync` only so the
/// owning objects can be, and `run_iteration` must be called on the UI thread
/// that registers (it takes a `&MainThread`, which only that thread holds).
#[derive(Default)]
pub(crate) struct RunLoop {
    state: Mutex<State>,
}

opaque_debug!(RunLoop);

/// One handler due this iteration, copied out from under the lock.
enum Due {
    Timer(usize, *mut ITimerHandler, Option<Owner>),
    Fd(i32, *mut IEventHandler, Option<Owner>),
}

/// [`Scope`] for a caller that cannot borrow the loop: it holds the `Arc`.
pub(crate) struct OwnedScope {
    run_loop: Arc<RunLoop>,
    previous: Option<Owner>,
}

impl Drop for OwnedScope {
    fn drop(&mut self) {
        self.run_loop.state().scope = self.previous;
    }
}

/// Whether the caller is the thread that claimed the process's main thread.
/// `IRunLoop` is UI-thread by spec, and a registration from another thread
/// would put a handler in a table only the UI thread may dispatch from: it is
/// refused (`kResultFalse`).
fn on_main_thread() -> bool {
    MainThread::current().is_some()
}

/// While it lives, registrations that name no owner belong to one
/// ([`RunLoop::scope`]).
pub(crate) struct Scope<'a> {
    run_loop: &'a RunLoop,
    previous: Option<Owner>,
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.run_loop.state().scope = self.previous;
    }
}

impl RunLoop {
    /// A loop with nothing registered.
    #[must_use]
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether the handler recorded in `due` is still registered.
    fn still_registered(state: &State, due: &Due) -> bool {
        match *due {
            Due::Timer(key, ..) => state.timers.contains_key(&key),
            Due::Fd(fd, handler, _) => {
                state.event_handlers.get(&fd).map(|e| e.handler) == Some(handler)
            }
        }
    }

    /// Fires any timers whose period has elapsed and dispatches any readable
    /// file descriptors.
    ///
    /// A Linux host must call this regularly from its UI thread while a plugin
    /// editor is open; without it the plugin's GUI never redraws and its timers
    /// never tick.
    ///
    /// The lock is not held while a handler runs, so a handler may register or
    /// unregister handlers (itself included) from inside its callback. A handler
    /// unregistered by an earlier callback of the same iteration is not called.
    ///
    /// The handlers are the plugin's UI-thread objects, which the `&MainThread`
    /// proves this thread to be the owner of; the host crate (which forbids
    /// `unsafe`) calls it as it calls every other main-thread entry.
    pub(crate) fn run_iteration(&self, _main: &MainThread) {
        let mut due: Vec<Due> = Vec::new();
        let mut fds: Vec<libc::pollfd> = Vec::new();
        {
            let mut state = self.state();
            let now = Instant::now();
            for (&key, entry) in state.timers.iter_mut() {
                if now.duration_since(entry.last_fired) >= entry.period {
                    entry.last_fired = now;
                    if !entry.handler.is_null() {
                        due.push(Due::Timer(key, entry.handler, entry.owner));
                    }
                }
            }
            // Zero timeout below: never block the caller's UI loop.
            fds.extend(state.event_handlers.keys().map(|&fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            }));
        }
        if !fds.is_empty() {
            // SAFETY: `fds` is a valid array of its own length.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
            if ready > 0 {
                let state = self.state();
                for pfd in fds.iter().filter(|p| p.revents & libc::POLLIN != 0) {
                    if let Some(entry) = state.event_handlers.get(&pfd.fd)
                        && !entry.handler.is_null()
                    {
                        due.push(Due::Fd(pfd.fd, entry.handler, entry.owner));
                    }
                }
            }
        }

        for item in due {
            // Re-check under the lock: an earlier callback may have unregistered
            // this handler, and the plugin may free it once it has.
            if !Self::still_registered(&self.state(), &item) {
                continue;
            }
            match item {
                Due::Timer(_, handler, owner) => {
                    // What the callback registers is its owner's.
                    let _scope = owner.map(|o| self.scope(o));
                    // SAFETY: the plugin registered `handler` and keeps it alive
                    // until it unregisters it (`register_timer`'s contract); it was
                    // still registered a moment ago, on this (the UI) thread, and
                    // no lock is held, so the callback may re-enter the loop.
                    unsafe { ((*(*handler).vtbl).onTimer)(handler) };
                    #[cfg(feature = "conformance")]
                    {
                        self.state().timers_fired += 1;
                    }
                }
                Due::Fd(fd, handler, owner) => {
                    let _scope = owner.map(|o| self.scope(o));
                    // SAFETY: as for the timer: registered, alive until
                    // unregistered, UI thread, lock released.
                    unsafe { ((*(*handler).vtbl).onFDIsSet)(handler, fd) };
                    #[cfg(feature = "conformance")]
                    {
                        self.state().fds_dispatched += 1;
                    }
                }
            }
        }
    }

    /// Until the guard drops, a registration that names no owner (one through
    /// the module's host context, which every plugin shares) is `owner`'s. The
    /// host wraps the calls that make a plugin register (`attached`) and the
    /// handlers' own callbacks in one.
    pub(crate) fn scope(&self, owner: Owner) -> Scope<'_> {
        let previous = self.state().scope.replace(owner);
        Scope {
            run_loop: self,
            previous,
        }
    }

    /// [`scope`](Self::scope), holding the loop by `Arc` instead of borrowing it.
    pub(crate) fn scope_owned(self: &Arc<Self>, owner: Owner) -> OwnedScope {
        let previous = self.state().scope.replace(owner);
        OwnedScope {
            run_loop: Arc::clone(self),
            previous,
        }
    }

    /// Forgets every registration, owned or not. The module calls it as it
    /// closes: whatever a plugin left registered through the module's own
    /// context (owned by no instance) points into code about to be unloaded.
    pub(crate) fn clear_all(&self) -> usize {
        let mut state = self.state();
        let n = state.timers.len() + state.event_handlers.len();
        state.timers.clear();
        state.event_handlers.clear();
        n
    }

    /// Forgets every timer and file descriptor `owner` registered and has not
    /// unregistered, and says how many. Nothing is dereferenced. The host calls
    /// it when an editor closes and when a plugin drops: from then on nothing
    /// of the plugin's UI runs, so a registration left behind is a leak of the
    /// plugin's, and calling it would be a call into freed memory.
    pub(crate) fn purge(&self, owner: Owner) -> usize {
        let mut state = self.state();
        let before = state.timers.len() + state.event_handlers.len();
        state.timers.retain(|_, e| e.owner != Some(owner));
        state.event_handlers.retain(|_, e| e.owner != Some(owner));
        before - state.timers.len() - state.event_handlers.len()
    }

    /// Snapshot of what is registered and what this loop has dispatched.
    #[cfg(feature = "conformance")]
    #[must_use]
    pub(crate) fn activity(&self) -> super::RunLoopActivity {
        let state = self.state();
        super::RunLoopActivity {
            timers_registered: state.timers.len(),
            event_handlers_registered: state.event_handlers.len(),
            timers_fired: state.timers_fired,
            fds_dispatched: state.fds_dispatched,
        }
    }

    // ── The IRunLoop operations, shared by every exposing object ─────────────

    /// `IRunLoop::registerEventHandler`: calls `handler.onFDIsSet(fd)` from
    /// [`run_iteration`](Self::run_iteration) when `fd` is readable. A null
    /// handler is ignored.
    ///
    /// # Safety
    ///
    /// `handler` is null or points at a live `IEventHandler` that stays alive
    /// until it is passed to [`unregister_event_handler`](Self::unregister_event_handler)
    /// or its `owner` is [purged](Self::purge), and is only called on the UI
    /// thread.
    pub(crate) unsafe fn register_event_handler(
        &self,
        handler: *mut IEventHandler,
        fd: FileDescriptor,
        owner: Option<Owner>,
    ) -> tresult {
        if handler.is_null() {
            return kResultOk;
        }
        if !on_main_thread() {
            return kResultFalse;
        }
        let mut state = self.state();
        let owner = owner.or(state.scope);
        state.event_handlers.insert(fd, FdEntry { handler, owner });
        kResultOk
    }

    /// `IRunLoop::unregisterEventHandler`: forgets every fd registered with
    /// `handler`. The pointer is only compared, never dereferenced.
    pub(crate) fn unregister_event_handler(&self, handler: *mut IEventHandler) -> tresult {
        self.state()
            .event_handlers
            .retain(|_, e| e.handler != handler);
        kResultOk
    }

    /// `IRunLoop::registerTimer`: calls `handler.onTimer()` from
    /// [`run_iteration`](Self::run_iteration) every `milliseconds` (at least 1).
    /// A null handler is ignored.
    ///
    /// # Safety
    ///
    /// `handler` is null or points at a live `ITimerHandler` that stays alive
    /// until it is passed to [`unregister_timer`](Self::unregister_timer) or its
    /// `owner` is [purged](Self::purge), and is only called on the UI thread.
    pub(crate) unsafe fn register_timer(
        &self,
        handler: *mut ITimerHandler,
        milliseconds: TimerInterval,
        owner: Option<Owner>,
    ) -> tresult {
        if handler.is_null() {
            return kResultOk;
        }
        if !on_main_thread() {
            return kResultFalse;
        }
        // Zero would mean "every iteration"; clamp so a misbehaving plugin
        // cannot spin the host's UI loop.
        let period = Duration::from_millis(milliseconds.max(1));
        let mut state = self.state();
        let owner = owner.or(state.scope);
        state.timers.insert(
            handler as usize,
            TimerEntry {
                handler,
                owner,
                period,
                last_fired: Instant::now(),
            },
        );
        kResultOk
    }

    /// `IRunLoop::unregisterTimer`. The pointer is only compared.
    pub(crate) fn unregister_timer(&self, handler: *mut ITimerHandler) -> tresult {
        self.state().timers.remove(&(handler as usize));
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
    use vst3::Steinberg::Linux::{ITimerHandler as RawTimerHandler, ITimerHandlerTrait};
    use vst3::{Class, ComWrapper};

    /// A timer handler that unregisters itself from inside its own callback.
    struct SelfUnregistering {
        run_loop: Arc<RunLoop>,
        me: AtomicPtr<RawTimerHandler>,
        calls: AtomicUsize,
    }

    impl Class for SelfUnregistering {
        type Interfaces = (RawTimerHandler,);
    }

    impl ITimerHandlerTrait for SelfUnregistering {
        unsafe fn onTimer(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Re-enters the loop from the callback: takes the state lock,
            // which `run_iteration` must not hold across this call.
            self.run_loop
                .unregister_timer(self.me.load(Ordering::SeqCst));
        }
    }

    /// A handler may unregister itself from its callback without deadlocking,
    /// and is not called again.
    ///
    /// Mutation: hold the state lock across the handler calls in
    /// `run_iteration` and the callback's `unregister_timer` locks
    /// the same mutex on the same thread: the test hangs instead of passing.
    #[test]
    fn a_timer_may_unregister_itself_from_its_callback() {
        let run_loop = RunLoop::new();
        let handler = ComWrapper::new(SelfUnregistering {
            run_loop: run_loop.clone(),
            me: AtomicPtr::new(std::ptr::null_mut()),
            calls: AtomicUsize::new(0),
        });
        let ptr = handler
            .as_com_ref::<RawTimerHandler>()
            .expect("the class implements ITimerHandler")
            .as_ptr();
        handler.me.store(ptr, Ordering::SeqCst);
        let _main = MainThread::claim().unwrap();
        // SAFETY: `ptr` is the live `handler` above, which outlives its
        // registration (it unregisters itself, and is dropped after the test).
        unsafe { run_loop.register_timer(ptr, 1, None) };

        std::thread::sleep(Duration::from_millis(5));
        run_loop.run_iteration(&MainThread::claim().unwrap());
        std::thread::sleep(Duration::from_millis(5));
        run_loop.run_iteration(&MainThread::claim().unwrap());

        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    }

    /// A handler that does nothing, kept alive by the test.
    struct Idle;
    impl Class for Idle {
        type Interfaces = (RawTimerHandler,);
    }
    impl ITimerHandlerTrait for Idle {
        unsafe fn onTimer(&self) {}
    }

    fn idle() -> (ComWrapper<Idle>, *mut RawTimerHandler) {
        let handler = ComWrapper::new(Idle);
        let ptr = handler
            .as_com_ref::<RawTimerHandler>()
            .expect("the class implements ITimerHandler")
            .as_ptr();
        (handler, ptr)
    }

    /// **A purge takes back one owner's registrations and no one else's.** A
    /// registration that names no owner takes the scope's, and one outside any
    /// scope stays unowned (the module's own, which nothing purges).
    ///
    /// Mutations (run, each fails): `purge` retaining everything -> the count is 0 and the loop
    /// still holds the owner's timers; `purge` removing every entry -> the other
    /// owner's timer is gone too; `register_timer` ignoring the scope -> the
    /// scoped timer is unowned, survives the purge, and the count is short.
    #[test]
    fn a_purge_takes_back_one_owners_registrations() {
        let run_loop = RunLoop::new();
        let _main = MainThread::claim().unwrap();
        let (a, b, scoped, unowned) = (Owner::next(), Owner::next(), idle(), idle());
        let (named_a, named_b) = (idle(), idle());
        // SAFETY: every handler outlives the registrations, which the test
        // purges or drops with the loop.
        unsafe {
            run_loop.register_timer(named_a.1, 10, Some(a));
            run_loop.register_timer(named_b.1, 10, Some(b));
            run_loop.register_timer(unowned.1, 10, None);
            let _scope = run_loop.scope(a);
            run_loop.register_timer(scoped.1, 10, None);
        }
        assert_eq!(run_loop.state().timers.len(), 4);
        assert_eq!(run_loop.purge(a), 2, "the named one and the scoped one");
        let left: Vec<_> = run_loop.state().timers.values().map(|t| t.owner).collect();
        assert_eq!(left.len(), 2);
        assert!(left.contains(&Some(b)) && left.contains(&None));
        assert_eq!(run_loop.purge(a), 0);
    }

    /// **A handler that leaks is not called once its owner is purged.**
    ///
    /// Mutation (run, fails): skip the purge -> the leaked timer fires and `calls` is 1.
    #[test]
    fn a_purged_timer_is_not_fired() {
        struct Counting(AtomicUsize);
        impl Class for Counting {
            type Interfaces = (RawTimerHandler,);
        }
        impl ITimerHandlerTrait for Counting {
            unsafe fn onTimer(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let run_loop = RunLoop::new();
        let handler = ComWrapper::new(Counting(AtomicUsize::new(0)));
        let ptr = handler.as_com_ref::<RawTimerHandler>().unwrap().as_ptr();
        let _main = MainThread::claim().unwrap();
        let owner = Owner::next();
        // SAFETY: `handler` outlives the registration.
        unsafe { run_loop.register_timer(ptr, 1, Some(owner)) };
        run_loop.purge(owner);
        std::thread::sleep(Duration::from_millis(5));
        run_loop.run_iteration(&MainThread::claim().unwrap());
        assert_eq!(handler.0.load(Ordering::SeqCst), 0);
    }

    /// **A registration from a thread that is not the main thread is refused,
    /// and `clear_all` empties the table.** `IRunLoop` is UI-thread by spec.
    ///
    /// Mutations (run, each fails): drop the `on_main_thread` check in
    /// `register_timer` -> the spawned thread's registration succeeds; make
    /// `clear_all` a no-op -> the count and the table are not empty.
    #[test]
    fn registration_is_main_thread_only_and_clear_all_empties() {
        let run_loop = RunLoop::new();
        let (handler, ptr) = idle();
        let off = {
            let run_loop = run_loop.clone();
            let ptr = ptr as usize;
            let _main = MainThread::claim().unwrap();
            std::thread::spawn(move || {
                // SAFETY: `handler` outlives the call, which registers nothing.
                unsafe { run_loop.register_timer(ptr as *mut RawTimerHandler, 1, None) }
            })
            .join()
            .unwrap()
        };
        assert_eq!(off, kResultFalse);
        assert!(run_loop.state().timers.is_empty());
        // SAFETY: `handler` outlives the registration, cleared below.
        unsafe { run_loop.register_timer(ptr, 1, None) };
        assert_eq!(run_loop.clear_all(), 1);
        assert!(run_loop.state().timers.is_empty());
        drop(handler);
    }
}
