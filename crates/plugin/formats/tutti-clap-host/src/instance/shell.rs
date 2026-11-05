//! The one object an activation's two halves share.
//!
//! [`ClapProcessor`](super::ClapProcessor) and
//! [`ClapController`](super::ClapController) each hold an `Arc` of one
//! [`Shell`]. The shell owns the [`ClapLoaded`], and so the
//! plugin instance, and nothing else.

use std::cell::UnsafeCell;
use std::sync::atomic::AtomicBool;

use clap_sys::plugin::clap_plugin;

use super::ClapLoaded;

/// A plugin pointer the Processor calls the audio-side entries through.
///
/// A copy of the pointer the [`Shell`]'s `ClapLoaded` owns, not an owner: it
/// destroys nothing. The Processor reaches the plugin only through this and
/// never touches the `ClapLoaded`, so the Controller's `&mut ClapLoaded` has no
/// Rust-level alias.
#[derive(Clone, Copy)]
pub(crate) struct RawPlugin(*const clap_plugin);

// SAFETY: clause 2 of the invariant on `Shell`. A pointer to a plugin the CLAP
// spec lets the audio-side entries call from whichever thread holds the
// `[audio-thread]` role, which the Processor takes through the claim.
unsafe impl Send for RawPlugin {}

impl RawPlugin {
    pub(crate) fn new(ptr: *const clap_plugin) -> Self {
        Self(ptr)
    }

    pub(crate) fn as_ptr(self) -> *const clap_plugin {
        self.0
    }
}

/// The plugin of one activation, shared by its two halves.
///
/// # Safety argument
///
/// `unsafe impl Sync`: two threads reach one plugin instance, which is sound
/// because of the following, each clause a rule the code keeps.
///
/// 1. *One pair.* `activate_typed` makes one `Arc<Shell>`, one Processor and one
///    Controller. Neither half is `Clone`. `deactivate` consumes both and checks
///    they hold the same shell, so no third handle exists.
/// 2. *Disjoint entries.* The Processor calls only the audio-side entries
///    (`start_processing`, `process`, `stop_processing`, `reset`), through its
///    own [`RawPlugin`]. The Controller calls only the main-thread entries,
///    through the `ClapLoaded` it reaches here. Every method takes `&mut self`,
///    so each side runs on one thread at a time, and the `[audio-thread]` role
///    is taken through the instance's `AudioThreadClaim`, so two audio-side
///    calls never overlap even from two OS threads.
/// 3. *Only concurrent-legal entries run on the Controller while the Processor
///    is lent.* CLAP annotates every entry with a thread; the main-thread set
///    (`params.get_value`, text conversion, `state.save/load`, `gui`,
///    `latency.get`, `tail.get`, `on_main_thread`) may run beside `process`.
///    `activate` and `deactivate` are the rejoin and run only with both halves
///    in hand.
/// 4. *The plugin is destroyed on the main thread only*: by dropping the
///    `ClapLoaded` that `deactivate` returns, or by the Controller's drop, which
///    sets `retired` first. A dropped Processor releases its `Arc` and calls
///    nothing in the plugin. A `process` after the Controller went returns an
///    error without touching the plugin; `Lifecycle` recalls the Processor
///    before it drops the Controller, so that path is a backstop for a user who
///    drops them out of order.
/// 5. *Callbacks touch atomics only.* A plugin calling the host from either
///    thread reaches `HostState::requests` and the other atomics, never the
///    `ClapLoaded` in this cell.
///
/// The cell is read and written by the Controller alone; the Processor reads
/// only `retired` and `processing`.
pub(crate) struct Shell {
    loaded: UnsafeCell<Option<ClapLoaded>>,
    /// Set by the Controller's drop before it destroys the plugin.
    pub(crate) retired: AtomicBool,
    /// Whether `start_processing` has run and `stop_processing` has not.
    /// Written under the `[audio-thread]` claim, by the Processor and by the
    /// Controller's teardown; distinct from the plugin being active.
    pub(crate) processing: AtomicBool,
}

// SAFETY: see the invariant on `Shell`.
unsafe impl Sync for Shell {}

impl Shell {
    pub(crate) fn new(loaded: ClapLoaded) -> Self {
        Self {
            loaded: UnsafeCell::new(Some(loaded)),
            retired: AtomicBool::new(false),
            processing: AtomicBool::new(false),
        }
    }

    /// The plugin, shared, for the Controller's `&self` methods.
    ///
    /// # Safety
    ///
    /// Main thread, by the Controller only (clause 2), and not while a
    /// reference from [`loaded_mut`](Self::loaded_mut) is live, which the
    /// Controller's `&self`/`&mut self` split guarantees.
    pub(crate) unsafe fn loaded(&self) -> &Option<ClapLoaded> {
        // SAFETY: the caller's contract.
        unsafe { &*self.loaded.get() }
    }

    /// The plugin, exclusively, for the Controller's `&mut self` methods.
    ///
    /// # Safety
    ///
    /// As [`loaded`](Self::loaded); the Controller holds `&mut self`, so no
    /// other reference to the cell's content exists.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn loaded_mut(&self) -> &mut Option<ClapLoaded> {
        // SAFETY: the caller's contract.
        unsafe { &mut *self.loaded.get() }
    }

    /// The plugin, once the shell is the last handle.
    pub(crate) fn into_loaded(self) -> Option<ClapLoaded> {
        self.loaded.into_inner()
    }
}
