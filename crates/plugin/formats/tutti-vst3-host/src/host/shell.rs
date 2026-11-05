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
/// 3. *The calls that run on the Controller while the Processor renders are
///    the ones the SDK lets run in the `Processing` state.* The Processor's
///    two entries are `IAudioProcessor::process`, `[processing-thread &
///    Processing]`, and `setProcessing`. Beside them, through the `Vst3Loaded`
///    the Controller derefs to, the main thread runs: the `IEditController`
///    interface (`[UI-thread]`: parameters, text conversion, units, program
///    lists, the editor); `IComponent::getState` and `setState` (any state,
///    `Processing` included); and the reads the core makes after an
///    activation and on a request, `getLatencySamples`, `getTailSamples`,
///    `getBusArrangement` and `getBusCount`/`getBusInfo`, which the SDK
///    annotates `[UI-thread & Setup Done]`. That annotation names the state
///    that must have been reached, and `Processing` is reached after it, so
///    the SDK permits them there (its own wrapper reads latency while the audio
///    thread renders). What the SDK does not leave to a plugin, and this host
///    never does beside `process`, are the transitions: `setActive`,
///    `setupProcessing`, `setBusArrangements` and `activateBus` run only in the
///    activation (before the Processor exists) and in the rejoin (after it is
///    gone). A plugin that races one of the permitted getters against its own
///    `process` breaks the SDK's contract, not this host's, and is exactly as
///    exposed in any other host. A parameter edit never crosses the pair as a
///    call: it reaches the Processor as a point in `inputParameterChanges`, from
///    the edit ring.
/// 4. *The plugin is destroyed on the control side.* Both halves hold an
///    `Arc`; `Lifecycle` rejoins them before it drops either, and the Controller
///    dropping last or the rejoin destroys the plugin on the thread that holds
///    the Controller. A Processor that finds itself the last holder (its
///    Controller was dropped first) does not destroy the plugin on the thread it
///    renders on: it hands the shell to a bounded retirement queue
///    ([`retire`]) that a control-side call empties
///    ([`reap_retired`](super::Vst3Loaded::reap_retired): `load`, `activate`
///    and every `flush_edits` tick). Until it is reaped the plugin stays
///    active, unreferenced and idle. Only when the queue is full (64 plugins
///    retired and never reaped) does the Processor's drop destroy the plugin in
///    place.
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

/// Shells whose last holder was a Processor, waiting to be destroyed on the
/// control side. Bounded so that pushing never allocates; built by
/// [`warm_retirement`] before any Processor exists.
static RETIRED: std::sync::OnceLock<crossbeam_queue::ArrayQueue<Shell>> =
    std::sync::OnceLock::new();

/// How many retired shells wait: more plugins than this retired by Processors
/// dropped on their own, and never reaped, are destroyed in place.
const RETIRED_CAPACITY: usize = 64;

fn retired() -> &'static crossbeam_queue::ArrayQueue<Shell> {
    RETIRED.get_or_init(|| crossbeam_queue::ArrayQueue::new(RETIRED_CAPACITY))
}

/// Builds the retirement queue, so that no Processor's drop does. Control side.
pub(super) fn warm_retirement() {
    let _ = retired();
}

/// Takes a shell no Controller holds off the realtime thread: queued for the
/// control side, or destroyed in place when the queue is full.
fn retire(shell: Shell) {
    if let Some(queue) = RETIRED.get() {
        if let Err(shell) = queue.push(shell) {
            drop(shell);
        }
    } else {
        drop(shell);
    }
}

/// Destroys the shells retirement has queued, on the calling thread, and says
/// how many.
pub(super) fn reap() -> usize {
    let Some(queue) = RETIRED.get() else {
        return 0;
    };
    let mut reaped = 0;
    while let Some(shell) = queue.pop() {
        drop(shell);
        reaped += 1;
    }
    reaped
}

/// A Processor's share of its shell, which hands the shell to retirement
/// instead of destroying the plugin when it is the last.
///
/// The last field of the Processor, so the scratch and the plugin references
/// above it are released first.
pub(super) struct Hold(Option<std::sync::Arc<Shell>>);

impl Hold {
    pub(super) fn new(shell: std::sync::Arc<Shell>) -> Self {
        Self(Some(shell))
    }

    pub(super) fn shell(&self) -> &std::sync::Arc<Shell> {
        self.0
            .as_ref()
            .expect("a Hold holds its shell until it drops")
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(arc) = self.0.take()
            && let Some(shell) = std::sync::Arc::into_inner(arc)
        {
            retire(shell);
        }
    }
}
