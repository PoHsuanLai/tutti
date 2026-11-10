//! The one object an activation's two halves share.
//!
//! [`Vst3Processor`](super::Vst3Processor) and
//! [`Vst3Controller`](super::Vst3Controller) each hold an `Arc` of one
//! [`Shell`]. The Controller owns the [`Vst3Loaded`], and so the plugin
//! instance; the Processor owns only the plugin's audio half. The shell holds
//! no plugin access at all: it is the identity of the pair, and the hand-off
//! point for the plugin when the Controller is dropped while the Processor
//! still renders.
//!
//! # Why two threads can reach one plugin
//!
//! 1. *One pair.* `activate_typed` makes one `Arc<Shell>`, one Processor and one
//!    Controller. Neither half is `Clone`. `deactivate` consumes both and
//!    checks they hold the same shell and no third handle exists.
//! 2. *Disjoint entries, by type.* The Processor owns an
//!    [`AudioSide`](tutti_vst3_ffi::AudioSide), which names only
//!    `IAudioProcessor::process` and `setProcessing`. The Controller owns the
//!    [`Plugin`](tutti_vst3_ffi::Plugin), which names every main-thread entry
//!    (the `IEditController` interface, `IComponent` state, `IUnitInfo`,
//!    `IConnectionPoint` and the editor) and is neither `Sync` nor shared, so
//!    each side runs on one thread at a time and neither can make the other's
//!    calls.
//! 3. *The calls that run on the Controller while the Processor renders are
//!    the ones the SDK lets run in the `Processing` state.* The Processor's
//!    two entries are `IAudioProcessor::process`, `[processing-thread &
//!    Processing]`, and `setProcessing`. Beside them, through the `Vst3Loaded`
//!    the Controller derefs to, the main thread runs: the `IEditController`
//!    interface (`[UI-thread]`: parameters, text conversion, units, program
//!    lists, the editor); `IComponent::getState` and `setState` (any state,
//!    `Processing` included); and the reads the core makes after an
//!    activation and on a request, `getLatencySamples`, `getTailSamples`,
//!    `getBusArrangement` and `getBusCount`/`getBusInfo`, which the SDK
//!    annotates `[UI-thread & Setup Done]`. That annotation names the state
//!    that must have been reached, and `Processing` is reached after it, so
//!    the SDK permits them there (its own wrapper reads latency while the audio
//!    thread renders). What the SDK does not leave to a plugin, and this host
//!    never does beside `process`, are the transitions: `setActive`,
//!    `setupProcessing`, `setBusArrangements` and `activateBus` run only in the
//!    activation (before the Processor exists) and in the rejoin (after it is
//!    gone). A plugin that races one of the permitted getters against its own
//!    `process` breaks the SDK's contract, not this host's, and is exactly as
//!    exposed in any other host. A parameter edit never crosses the pair as a
//!    call: it reaches the Processor as a point in `inputParameterChanges`, from
//!    the edit ring.
//! 4. *The plugin is destroyed on the control side.* `Lifecycle` rejoins the
//!    halves before it drops either, and the rejoin, or the Controller dropping
//!    last, destroys the plugin on the thread that holds the Controller. A
//!    Controller dropped while its Processor still renders hands the plugin to
//!    the shell instead of destroying it; the Processor, dropping last, does
//!    not destroy it on the thread it renders on either: it hands it to a
//!    bounded retirement queue ([`retire`]) that a control-side call empties
//!    ([`reap_retired`](super::Vst3Loaded::reap_retired): `load`, `activate` and
//!    every `flush_edits` tick). Until it is reaped the plugin stays active,
//!    unreferenced and idle. Only when the queue is full (64 plugins retired
//!    and never reaped) does the Processor's drop destroy the plugin in place.
//! 5. *Callbacks touch atomics and queues only.* A plugin calling the host from
//!    either thread reaches the `ComponentHandler`'s word and queue, never the
//!    `Vst3Loaded`.

use std::sync::Arc;

use parking_lot::Mutex;

use super::loaded::Vst3Loaded;

/// Who of a pair is alive, and the plugin when its Controller went first.
struct State {
    controller_alive: bool,
    processor_alive: bool,
    /// The plugin, parked by a Controller that was dropped while the
    /// Processor still held its share.
    parked: Option<Vst3Loaded>,
}

/// The identity of an activation's pair, and the hand-off between its halves
/// when they are dropped apart.
pub(super) struct Shell {
    state: Mutex<State>,
}

impl Shell {
    /// A shell for a fresh pair, both halves alive.
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                controller_alive: true,
                processor_alive: true,
                parked: None,
            }),
        }
    }

    /// The Controller is dropped holding `loaded`: parks it if the Processor
    /// still renders, else returns it to be destroyed by the caller's thread
    /// (the control side).
    pub(super) fn controller_gone(&self, loaded: Vst3Loaded) -> Option<Vst3Loaded> {
        let mut state = self.state.lock();
        state.controller_alive = false;
        if state.processor_alive {
            state.parked = Some(loaded);
            None
        } else {
            Some(loaded)
        }
    }

    /// The Controller took its plugin back (a rejoin): it no longer counts
    /// among the living, and there is nothing to park.
    pub(super) fn controller_rejoined(&self) {
        self.state.lock().controller_alive = false;
    }

    /// The Processor is dropped: the plugin a Controller parked, if it did.
    fn processor_gone(&self) -> Option<Vst3Loaded> {
        let mut state = self.state.lock();
        state.processor_alive = false;
        state.parked.take()
    }
}

/// Plugins whose last holder was a Processor, waiting to be destroyed on the
/// control side. Bounded so that pushing never allocates; built by
/// [`warm_retirement`] before any Processor exists.
static RETIRED: std::sync::OnceLock<crossbeam_queue::ArrayQueue<Vst3Loaded>> =
    std::sync::OnceLock::new();

/// How many retired plugins wait: more plugins than this retired by Processors
/// dropped on their own, and never reaped, are destroyed in place.
const RETIRED_CAPACITY: usize = 64;

fn retired() -> &'static crossbeam_queue::ArrayQueue<Vst3Loaded> {
    RETIRED.get_or_init(|| crossbeam_queue::ArrayQueue::new(RETIRED_CAPACITY))
}

/// Builds the retirement queue, so that no Processor's drop does. Control side.
pub(super) fn warm_retirement() {
    let _ = retired();
}

/// Takes a plugin no Controller holds off the realtime thread: queued for the
/// control side, or destroyed in place when the queue is full.
///
/// **Known gap (audit S11, deferred).** A full queue (64 plugins retired and
/// never reaped) or an unwarmed one still destroys the plugin on the calling
/// thread, which may be the audio thread: a `dlclose` under the process-wide
/// module lock. Nothing here can refuse the swap that dropped the Processor,
/// and leaking the plugin instead would trade a glitch for an unbounded leak.
/// The guard is the control side's: [`reap`] runs on every tick and
/// [`warm_retirement`] runs before the first Processor, so reaching the cap
/// means the control side has stopped ticking. The real fix is a swap that asks
/// for room first, which belongs to the session's node replacement, not to this
/// crate.
fn retire(loaded: Vst3Loaded) {
    if let Some(queue) = RETIRED.get() {
        if let Err(loaded) = queue.push(loaded) {
            drop(loaded);
        }
    } else {
        drop(loaded);
    }
}

/// Destroys the plugins retirement has queued, on the calling thread, and says
/// how many.
pub(super) fn reap() -> usize {
    let Some(queue) = RETIRED.get() else {
        return 0;
    };
    let mut reaped = 0;
    while let Some(loaded) = queue.pop() {
        drop(loaded);
        reaped += 1;
    }
    reaped
}

/// A Processor's share of its shell, which hands the plugin to retirement
/// instead of destroying it when the Controller went first.
///
/// The last field of the Processor, so the scratch and the plugin references
/// above it are released first.
pub(super) struct Hold(Option<Arc<Shell>>);

impl Hold {
    pub(super) fn new(shell: Arc<Shell>) -> Self {
        Self(Some(shell))
    }

    pub(super) fn shell(&self) -> &Arc<Shell> {
        self.0
            .as_ref()
            .expect("a Hold holds its shell until it drops")
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(shell) = self.0.take()
            && let Some(loaded) = shell.processor_gone()
        {
            retire(loaded);
        }
    }
}
