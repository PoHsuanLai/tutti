//! The one object an activation's two halves share.
//!
//! [`Vst3Processor`](super::Vst3Processor) and
//! [`Vst3Controller`](super::Vst3Controller) each hold an `Arc` of one
//! [`Shell`]. The shell owns the [`Vst3Loaded`], and so the plugin instance,
//! and nothing else.

use std::cell::UnsafeCell;

use super::loaded::Vst3Loaded;

/// The plugin of one activation, shared by its two halves.
///
/// # Safety argument
///
/// `unsafe impl Sync`: two threads reach one plugin instance, which is sound
/// because of the following, each clause a rule the code keeps.
///
/// 1. *One pair.* `activate_typed` makes one `Arc<Shell>`, one Processor and one
///    Controller. Neither half is `Clone`. `deactivate` consumes both and
///    checks they hold the same shell and no third handle exists.
/// 2. *Disjoint entries.* The Processor calls only `IAudioProcessor::process`
///    and `setProcessing`, through its own `ComPtr` to the processor interface,
///    and never reaches the `Vst3Loaded` in this cell, so the Controller's
///    `&mut Vst3Loaded` has no Rust-level alias. The Controller calls the
///    `IEditController`, `IComponent` state, `IUnitInfo` and
///    `IConnectionPoint` entries and the editor, through the `Vst3Loaded` it
///    reaches here. Every method of either half takes `&mut self`, or a shared
///    reference of a type that is not `Sync`, so each side runs on one thread
///    at a time.
/// 3. *Only concurrent-legal entries run on the Controller while the Processor
///    renders.* The SDK annotates each interface with its thread: `process` is
///    `[processing-thread & Processing]` and `setProcessing` `[UI-thread or
///    processing-thread & Setup Done]`; `IEditController` is `[UI-thread]`;
///    `IComponent::getState` and `setState` may run in any state, `Processing`
///    included. `setActive`, `setupProcessing`, `setBusArrangements` and
///    `getLatencySamples` are `[UI-thread & Setup Done]` and run only in the
///    rejoin, with both halves in hand, or in the activation that makes them.
///    A parameter edit never crosses the pair as a call: it reaches the
///    Processor as a point in `inputParameterChanges`, from the edit ring.
/// 4. *The plugin is destroyed by the last handle.* The Controller's drop and
///    the Processor's drop each release their `Arc`; the one that releases it
///    last drops the `Vst3Loaded`, which stops processing, deactivates and
///    terminates the plugin. `Lifecycle` rejoins the halves before it drops
///    either, so on the supported path that is the main thread; a user who
///    drops the Controller first and keeps the Processor renders on, with the
///    plugin alive, and the Processor's own drop then ends the activation.
/// 5. *Callbacks touch atomics and queues only.* A plugin calling the host from
///    either thread reaches the `ComponentHandler`'s word and queue, never the
///    `Vst3Loaded` in this cell.
///
/// The cell is read and written by the Controller alone, and by whichever
/// thread drops the last handle, when no other handle exists.
pub(super) struct Shell {
    loaded: UnsafeCell<Option<Vst3Loaded>>,
}

// SAFETY: see the invariant on `Shell`.
unsafe impl Sync for Shell {}

impl Shell {
    pub(super) fn new(loaded: Vst3Loaded) -> Self {
        Self {
            loaded: UnsafeCell::new(Some(loaded)),
        }
    }

    /// The plugin, shared, for the Controller's `&self` methods.
    ///
    /// # Safety
    ///
    /// Main thread, by the Controller only (clause 2), and not while a
    /// reference from [`loaded_mut`](Self::loaded_mut) is live, which the
    /// Controller's `&self`/`&mut self` split guarantees.
    pub(super) unsafe fn loaded(&self) -> &Option<Vst3Loaded> {
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
    pub(super) unsafe fn loaded_mut(&self) -> &mut Option<Vst3Loaded> {
        // SAFETY: the caller's contract.
        unsafe { &mut *self.loaded.get() }
    }

    /// The plugin, once the shell is the last handle.
    pub(super) fn into_loaded(self) -> Option<Vst3Loaded> {
        self.loaded.into_inner()
    }
}
