//! `Linux::IRunLoop` — the event loop a Linux host must lend the plugin.
//!
//! X11 has no ambient run loop the way Cocoa and Win32 do, so VST3 makes the
//! host provide one. The plugin registers its X connection's file descriptor
//! and any repeating timers with us, and we call back when they are ready.
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
//!   Some toolkits look here instead, so we answer here too.
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vst3::Steinberg::kResultOk;
use vst3::Steinberg::tresult;
use vst3::Steinberg::Linux::{FileDescriptor, IEventHandler, ITimerHandler, TimerInterval};

/// A timer the plugin asked us to run, and when it last fired.
struct TimerEntry {
    handler: *mut ITimerHandler,
    period: Duration,
    last_fired: Instant,
}

/// Registered handlers. Raw pointers are plugin-owned COM objects; they are
/// dereferenced only from [`RunLoop::run_iteration`], which the host calls on
/// its UI thread — the same thread that registered them.
#[derive(Default)]
struct State {
    /// fd → plugin event handler, invoked when the fd becomes readable.
    event_handlers: HashMap<i32, *mut IEventHandler>,
    /// Repeating timers, keyed by handler address.
    timers: HashMap<usize, TimerEntry>,
    /// Dispatch tallies. Test-only: see [`RunLoopActivity`].
    #[cfg(feature = "conformance")]
    timers_fired: u64,
    #[cfg(feature = "conformance")]
    fds_dispatched: u64,
}

// SAFETY: see `State` — the pointers are only used from the UI thread, and the
// Mutex serialises access to the collections themselves.
unsafe impl Send for State {}

/// What one [`RunLoop::run_iteration`] actually did, plus what is registered.
///
/// Only meaningful to assert on: "the plugin registered a timer and our pump
/// fired it" is otherwise invisible from outside — the handlers are plugin-side
/// COM objects and the effects land in the plugin's own GUI.
#[cfg(feature = "conformance")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunLoopActivity {
    /// Timers the plugin currently has registered.
    pub timers_registered: usize,
    /// File descriptors the plugin currently has registered.
    pub event_handlers_registered: usize,
    /// Cumulative `ITimerHandler::onTimer` calls this loop has made.
    pub timers_fired: u64,
    /// Cumulative `IEventHandler::onFDIsSet` calls this loop has made.
    pub fds_dispatched: u64,
}

/// The host's run loop, shared between every object that exposes `IRunLoop`.
#[derive(Default)]
pub(crate) struct RunLoop {
    state: Mutex<State>,
}

impl RunLoop {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Fire any timers whose period has elapsed and dispatch any readable file
    /// descriptors.
    ///
    /// A Linux host must call this regularly from its UI thread while a plugin
    /// editor is open; without it the plugin's GUI never redraws and its timers
    /// never tick.
    pub(crate) fn run_iteration(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());

        let now = Instant::now();
        // Tallied locally because `values_mut` holds the borrow on `state`.
        #[cfg(feature = "conformance")]
        let mut fired = 0u64;
        for entry in state.timers.values_mut() {
            if now.duration_since(entry.last_fired) >= entry.period {
                entry.last_fired = now;
                let handler = entry.handler;
                if !handler.is_null() {
                    // SAFETY: plugin-owned handler, still registered
                    // (unregistration takes this same lock), called on the UI
                    // thread as the spec requires.
                    unsafe { ((*(*handler).vtbl).onTimer)(handler) };
                    #[cfg(feature = "conformance")]
                    {
                        fired += 1;
                    }
                }
            }
        }

        #[cfg(feature = "conformance")]
        {
            state.timers_fired += fired;
        }

        if state.event_handlers.is_empty() {
            return;
        }
        // Zero timeout: never block the caller's UI loop.
        let mut fds: Vec<libc::pollfd> = state
            .event_handlers
            .keys()
            .map(|&fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `fds` is a valid array of its own length.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
        if ready <= 0 {
            return;
        }
        #[cfg(feature = "conformance")]
        let mut dispatched = 0u64;
        for pfd in &fds {
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            if let Some(&handler) = state.event_handlers.get(&pfd.fd) {
                if !handler.is_null() {
                    // SAFETY: as above.
                    unsafe { ((*(*handler).vtbl).onFDIsSet)(handler, pfd.fd) };
                    #[cfg(feature = "conformance")]
                    {
                        dispatched += 1;
                    }
                }
            }
        }
        #[cfg(feature = "conformance")]
        {
            state.fds_dispatched += dispatched;
        }
    }

    /// Snapshot of what is registered and what this loop has dispatched.
    #[cfg(feature = "conformance")]
    pub(crate) fn activity(&self) -> RunLoopActivity {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        RunLoopActivity {
            timers_registered: state.timers.len(),
            event_handlers_registered: state.event_handlers.len(),
            timers_fired: state.timers_fired,
            fds_dispatched: state.fds_dispatched,
        }
    }

    // ── The IRunLoop operations, shared by every exposing object ─────────────

    pub(crate) fn register_event_handler(
        &self,
        handler: *mut IEventHandler,
        fd: FileDescriptor,
    ) -> tresult {
        if handler.is_null() {
            return kResultOk;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.event_handlers.insert(fd, handler);
        kResultOk
    }

    pub(crate) fn unregister_event_handler(&self, handler: *mut IEventHandler) -> tresult {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.event_handlers.retain(|_, &mut h| h != handler);
        kResultOk
    }

    pub(crate) fn register_timer(
        &self,
        handler: *mut ITimerHandler,
        milliseconds: TimerInterval,
    ) -> tresult {
        if handler.is_null() {
            return kResultOk;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Zero would mean "every iteration"; clamp so a misbehaving plugin
        // cannot spin the host's UI loop.
        let period = Duration::from_millis(milliseconds.max(1));
        state.timers.insert(
            handler as usize,
            TimerEntry {
                handler,
                period,
                last_fired: Instant::now(),
            },
        );
        kResultOk
    }

    pub(crate) fn unregister_timer(&self, handler: *mut ITimerHandler) -> tresult {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.timers.remove(&(handler as usize));
        kResultOk
    }
}
