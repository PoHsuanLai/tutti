//! The one object an activation's two halves share.
//!
//! [`Vst2Processor`](crate::Vst2Processor) and
//! [`Vst2Controller`](crate::Vst2Controller) each hold an `Arc` of one
//! [`Shell`]. The shell owns the [`Vst2Loaded`], and so the plugin instance, and
//! the flag that says whether `effStartProcess` is the plugin's last word.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering};

use vst::plugin::Plugin as _;

use crate::loaded::Vst2Loaded;

/// The plugin of one activation, shared by its two halves.
///
/// # Safety argument
///
/// `unsafe impl Sync`: two threads reach one plugin instance. For VST2 that is
/// sound because of the following, each clause a rule the code keeps, and clause
/// 3 rests on a **host convention**, not on the format: the SDK says only that
/// `process` runs on the audio thread, the dispatcher on the UI thread, and
/// `setParameter` / `getParameter` on either. Nothing is said of any other
/// opcode beside `process`. What every major host does, and what plugins are
/// written and tested against, is what clause 3 names. A plugin that breaks it
/// misbehaves in every host, and one that races state loads against `process` is
/// the quirk step 9 of design 016 handles on its own instance.
///
/// 1. *One pair.* `activate` makes one `Arc<Shell>`, one Processor and one
///    Controller. Neither half is `Clone`. `deactivate` consumes both and
///    checks they hold the same shell and no third handle exists.
/// 2. *Disjoint entries.* The Processor calls only the plugin's render entries
///    (`processReplacing`, `processReplacingF64`, `effProcessEvents`,
///    `effStartProcess`, `effStopProcess`) through its own `RenderHandle`, which
///    is a copy of the `AEffect` pointer and holds no reference into the
///    `Vst2Loaded` in this cell, and `setParameter` through its own clone of the
///    plugin's parameter object, which is `Sync`. So the Controller's
///    `&mut Vst2Loaded` has no Rust-level alias. The Controller calls the
///    dispatcher and the parameter object through the `Vst2Loaded` it reaches
///    here. Every method of either half takes `&mut self`, or a shared
///    reference of a type that is not `Sync`, so each side runs on one thread at
///    a time.
/// 3. *The calls that run on the Controller while the Processor renders are the
///    ones hosts run beside `process`.* Through the `Vst2Loaded` the Controller
///    derefs to, the main thread runs: `effGetChunk` / `effSetChunk`, program
///    get, set and name, parameter get, set, name and display, the
///    `effGetParameterProperties` and MIDI-metadata queries, the editor opcodes,
///    and the reads the core makes after an activation and on a request
///    (`initialDelay`, `numInputs`, `numOutputs`, `effGetTailSize`). What a
///    plugin cannot take while processing, and this host never does beside
///    `process`, are `effSetSampleRate`, `effSetBlockSize`,
///    `effSetProcessPrecision`, `effSetSpeakerArrangement` and
///    `effMainsChanged`: they run only in the activation (before the Processor
///    exists) and in the rejoin (after it is gone).
/// 4. *The plugin is destroyed on the control side.* Both halves hold an `Arc`;
///    `Lifecycle` rejoins them before it drops either, and the Controller
///    dropping last or the rejoin destroys the plugin on the thread that holds
///    the Controller. A Processor that finds itself the last holder (its
///    Controller was dropped first) does not destroy the plugin on the thread it
///    renders on: it hands the shell to a bounded retirement queue ([`retire`])
///    that a control-side call empties ([`reap`]: `activate` and every
///    `flush_edits` tick). Until it is reaped the plugin stays resumed,
///    unreferenced and idle. Only when the queue is full (64 plugins retired and
///    never reaped) does the Processor's drop destroy the plugin in place.
/// 5. *Callbacks touch atomics and queues only.* A plugin calling the host from
///    either thread reaches `HostState`'s request word, its queues and its
///    atomics, never the `Vst2Loaded` in this cell.
///
/// The cell is read and written by the Controller alone, and by whichever
/// thread drops the last handle, when no other handle exists.
pub(crate) struct Shell {
    loaded: UnsafeCell<Option<Vst2Loaded>>,
    /// Whether `effStartProcess` is the plugin's last word. Written by the
    /// Processor (it starts and stops) and read by the drop that finds it on,
    /// when no Processor is left to stop it.
    processing: AtomicBool,
}

// SAFETY: see the invariant on `Shell`.
unsafe impl Sync for Shell {}

impl Shell {
    /// A shell for a plugin that was just `effStartProcess`ed by the activation.
    pub(crate) fn new(loaded: Vst2Loaded) -> Self {
        Self {
            loaded: UnsafeCell::new(Some(loaded)),
            processing: AtomicBool::new(true),
        }
    }

    /// Records that processing started (`true`) or stopped (`false`), and
    /// returns whether it was on before.
    pub(crate) fn set_processing(&self, on: bool) -> bool {
        self.processing.swap(on, Ordering::AcqRel)
    }

    /// Whether processing is on.
    pub(crate) fn is_processing(&self) -> bool {
        self.processing.load(Ordering::Acquire)
    }

    /// The plugin, shared, for the Controller's `&self` methods.
    ///
    /// # Safety
    ///
    /// Main thread, by the Controller only (clause 2), and not while a
    /// reference from [`loaded_mut`](Self::loaded_mut) is live, which the
    /// Controller's `&self`/`&mut self` split guarantees.
    pub(crate) unsafe fn loaded(&self) -> &Option<Vst2Loaded> {
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
    pub(crate) unsafe fn loaded_mut(&self) -> &mut Option<Vst2Loaded> {
        // SAFETY: the caller's contract.
        unsafe { &mut *self.loaded.get() }
    }

    /// The plugin, once the shell is the last handle. Its processing, if any,
    /// was stopped by whoever rejoins.
    pub(crate) fn into_loaded(mut self) -> Option<Vst2Loaded> {
        self.loaded.get_mut().take()
    }
}

impl Drop for Shell {
    /// The shell is the last holder and the plugin is still in it: no Processor
    /// was left to stop processing, so it is stopped here, once, before the
    /// plugin suspends and closes (`Vst2Handle`'s drop).
    fn drop(&mut self) {
        let processing = *self.processing.get_mut();
        if let Some(loaded) = self.loaded.get_mut()
            && processing
            && loaded.handle.resumed
            && !std::thread::panicking()
        {
            loaded.handle.instance.stop_process();
        }
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
pub(crate) fn warm_retirement() {
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
pub(crate) fn reap() -> usize {
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
pub(crate) struct Hold(Option<std::sync::Arc<Shell>>);

impl Hold {
    pub(crate) fn new(shell: std::sync::Arc<Shell>) -> Self {
        Self(Some(shell))
    }

    pub(crate) fn shell(&self) -> &std::sync::Arc<Shell> {
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
