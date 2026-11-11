//! [`FakePlugin`]: a plugin that implements [`Loaded`], [`Processor`] and
//! [`Controller`] without a plugin behind them.
//!
//! It records every call it receives in a preallocated lock-free log, so a test
//! asserts the sequence of calls rather than a render. It refuses a chosen
//! rate, can be told to lose itself on a refusal, to panic in `activate`, to
//! refuse a deactivation or a rebuild, and to fail a block. Both halves write
//! the log, so [`FakeProcessor`] takes no lock and may run inside
//! `assert_no_alloc`.
//!
//! [`FakePlugin`] is the test's handle on the shared state: it outlives the
//! halves, changes the figures they report, and raises plugin-initiated
//! requests on the word the shell would take.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use tutti_rt::RequestWord;

use crate::{
    AudioBufferMut, ChannelLayout, ContentScale, Control, Controller, EditDrain, EditSink,
    EditorError, EditorSize, EditorWindow, FdId, Layout, Loaded, LoadedPlugin, MainThread,
    Normalized, ParamId, ParameterInfo, PluginDescriptor, PluginError, PluginIo, PluginResult,
    PluginTail, Prepare, Processor, Refused, RenderMode, Request, RescanScope, RestartRules,
    SampleRate, Samples, Stuck, TimerId, WindowHandle, edits,
};

/// One call the fake received.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Call {
    /// `Loaded::activate`, at this rate and block size.
    Activate(SampleRate, Samples),
    /// `Loaded::deactivate` that rejoined.
    Deactivate,
    /// `Loaded::rebuild`.
    Rebuild,
    /// `Processor::process`, with this many frames.
    Process(usize),
    /// `Processor::stop`.
    Stop,
    /// `Processor::reset`.
    Reset,
    /// An edit the Processor applied at the head of a block.
    Edit(ParamId, Normalized),
    /// `Controller::latency`.
    Latency,
    /// `Controller::tail`.
    Tail,
    /// `Control::wiring`.
    Wiring,
    /// `Control::set_parameter` on the Controller or the loaded plugin.
    SetParameter(ParamId),
    /// `Control::save_state`.
    SaveState,
    /// `Control::load_state`, with this many bytes.
    LoadState(usize),
    /// `Control::rescan_parameters`.
    RescanParameters(RescanScope),
    /// `Control::rebuild_midi_mapping`.
    RebuildMidiMapping,
    /// `Control::run_main_thread`.
    RunMainThread,
    /// `Control::run_timer`.
    RunTimer(TimerId),
    /// `Control::service_fd`.
    ServiceFd(FdId),
}

const SCOPES: [RescanScope; 4] = [
    RescanScope::Values,
    RescanScope::Text,
    RescanScope::Info,
    RescanScope::All,
];

impl Call {
    /// The slot's three words: a kind (never zero) and two arguments.
    fn encode(self) -> [u64; 3] {
        match self {
            Self::Activate(rate, block) => [1, rate.0.to_bits(), block.0 as u64],
            Self::Deactivate => [2, 0, 0],
            Self::Rebuild => [3, 0, 0],
            Self::Process(frames) => [4, frames as u64, 0],
            Self::Stop => [5, 0, 0],
            Self::Edit(id, value) => [6, u64::from(id.get()), value.get().to_bits()],
            Self::Latency => [7, 0, 0],
            Self::Tail => [8, 0, 0],
            Self::Wiring => [9, 0, 0],
            Self::SetParameter(id) => [10, u64::from(id.get()), 0],
            Self::SaveState => [11, 0, 0],
            Self::LoadState(len) => [12, len as u64, 0],
            Self::RescanParameters(scope) => {
                let at = SCOPES.iter().position(|&s| s == scope).unwrap_or(0);
                [13, at as u64, 0]
            }
            Self::RunMainThread => [14, 0, 0],
            Self::RunTimer(id) => [15, u64::from(id.0), 0],
            Self::ServiceFd(id) => [16, u64::from(id.0 as u32), 0],
            Self::RebuildMidiMapping => [17, 0, 0],
            Self::Reset => [18, 0, 0],
        }
    }

    fn decode([kind, a, b]: [u64; 3]) -> Option<Self> {
        Some(match kind {
            1 => Self::Activate(SampleRate(f64::from_bits(a)), Samples(b as usize)),
            2 => Self::Deactivate,
            3 => Self::Rebuild,
            4 => Self::Process(a as usize),
            5 => Self::Stop,
            6 => Self::Edit(
                ParamId::new(a as u32),
                Normalized::clamped(f64::from_bits(b)),
            ),
            7 => Self::Latency,
            8 => Self::Tail,
            9 => Self::Wiring,
            10 => Self::SetParameter(ParamId::new(a as u32)),
            11 => Self::SaveState,
            12 => Self::LoadState(a as usize),
            13 => Self::RescanParameters(SCOPES[a as usize]),
            14 => Self::RunMainThread,
            15 => Self::RunTimer(TimerId(a as u32)),
            16 => Self::ServiceFd(FdId(a as u32 as i32)),
            17 => Self::RebuildMidiMapping,
            18 => Self::Reset,
            _ => return None,
        })
    }
}

/// How many calls the log holds.
const LOG_CAPACITY: usize = 4096;

/// A preallocated, lock-free record of calls. Any thread appends; read it once
/// the threads are quiet.
#[derive(Debug)]
struct CallLog {
    next: AtomicUsize,
    slots: Box<[[AtomicU64; 3]]>,
}

impl CallLog {
    fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
            slots: (0..LOG_CAPACITY)
                .map(|_| [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)])
                .collect(),
        }
    }

    fn push(&self, call: Call) {
        let at = self.next.fetch_add(1, Ordering::Relaxed);
        let Some(slot) = self.slots.get(at) else {
            return;
        };
        let [kind, a, b] = call.encode();
        slot[1].store(a, Ordering::Relaxed);
        slot[2].store(b, Ordering::Relaxed);
        slot[0].store(kind, Ordering::Release);
    }

    fn snapshot(&self) -> Vec<Call> {
        let used = self.next.load(Ordering::Relaxed);
        assert!(used <= LOG_CAPACITY, "the call log overflowed");
        self.slots[..used]
            .iter()
            .filter_map(|slot| {
                let kind = slot[0].load(Ordering::Acquire);
                Call::decode([
                    kind,
                    slot[1].load(Ordering::Relaxed),
                    slot[2].load(Ordering::Relaxed),
                ])
            })
            .collect()
    }

    fn clear(&self) {
        let used = self.next.swap(0, Ordering::Relaxed).min(LOG_CAPACITY);
        for slot in &self.slots[..used] {
            slot[0].store(0, Ordering::Relaxed);
        }
    }
}

#[derive(Debug)]
struct Shared {
    log: CallLog,
    refuse_rate: AtomicU64,
    lose_on_refuse: AtomicBool,
    panic_on_activate: AtomicBool,
    stuck: AtomicU32,
    rebuild_fails: AtomicBool,
    block_fails: AtomicBool,
    panic_in_deactivate: AtomicBool,
    panic_in_rebuild: AtomicBool,
    panic_in_latency: AtomicBool,
    edit_capacity: AtomicUsize,
    latency: AtomicUsize,
    tail: AtomicUsize,
    outputs: AtomicU32,
    requests: RequestWord,
    state: Mutex<Vec<u8>>,
}

impl Shared {
    fn wiring(&self) -> LoadedPlugin {
        self.log.push(Call::Wiring);
        let outputs = ChannelLayout::try_from(self.outputs.load(Ordering::Relaxed))
            .expect("a fake plugin has few outputs");
        LoadedPlugin {
            inputs: vec![ChannelLayout::STEREO],
            outputs: vec![outputs],
            latency_samples: Samples(self.latency.load(Ordering::Relaxed)),
            tail: self.tail(),
            ..LoadedPlugin::default()
        }
    }

    fn tail(&self) -> PluginTail {
        match self.tail.load(Ordering::Relaxed) {
            0 => PluginTail::None,
            n => PluginTail::Finite(Samples(n)),
        }
    }

    fn descriptor() -> PluginDescriptor {
        PluginDescriptor {
            id: "fake".into(),
            name: "Fake".into(),
            ..PluginDescriptor::default()
        }
    }
}

/// The test's handle on a fake plugin: it outlives the halves, and everything
/// it sets is seen by them.
#[derive(Debug, Clone)]
pub struct FakePlugin(Arc<Shared>);

impl FakePlugin {
    /// A loaded fake and the handle on it. Reports no latency or tail and a
    /// stereo output.
    #[must_use]
    pub fn new() -> (Self, FakeLoaded) {
        let shared = Arc::new(Shared {
            log: CallLog::new(),
            refuse_rate: AtomicU64::new(0),
            lose_on_refuse: AtomicBool::new(false),
            panic_on_activate: AtomicBool::new(false),
            stuck: AtomicU32::new(0),
            rebuild_fails: AtomicBool::new(false),
            block_fails: AtomicBool::new(false),
            panic_in_deactivate: AtomicBool::new(false),
            panic_in_rebuild: AtomicBool::new(false),
            panic_in_latency: AtomicBool::new(false),
            edit_capacity: AtomicUsize::new(64),
            latency: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            outputs: AtomicU32::new(2),
            requests: RequestWord::new(),
            state: Mutex::new(Vec::new()),
        });
        (
            Self(shared.clone()),
            FakeLoaded {
                shared,
                descriptor: Shared::descriptor(),
                values: Vec::new(),
            },
        )
    }

    /// Every call so far, in the order they were made. Read it once the threads
    /// are quiet.
    ///
    /// # Panics
    ///
    /// If more than 4096 calls were made.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.0.log.snapshot()
    }

    /// Forgets the calls so far.
    pub fn clear_calls(&self) {
        self.0.log.clear();
    }

    /// Makes `activate` refuse this rate. `None` takes every rate again.
    pub fn refuse_rate(&self, rate: Option<SampleRate>) {
        let bits = rate.map_or(0, |r| r.0.to_bits());
        self.0.refuse_rate.store(bits, Ordering::Relaxed);
    }

    /// Makes a refused activation hand no plugin back, as AU can.
    pub fn lose_plugin_on_refusal(&self, lose: bool) {
        self.0.lose_on_refuse.store(lose, Ordering::Relaxed);
    }

    /// Makes `activate` panic.
    pub fn panic_in_activate(&self, panic: bool) {
        self.0.panic_on_activate.store(panic, Ordering::Relaxed);
    }

    /// Makes the next `n` deactivations refuse, handing both halves back.
    pub fn stick_deactivations(&self, n: u32) {
        self.0.stuck.store(n, Ordering::Relaxed);
    }

    /// Makes `rebuild` refuse.
    pub fn fail_rebuild(&self, fail: bool) {
        self.0.rebuild_fails.store(fail, Ordering::Relaxed);
    }

    /// Makes `deactivate` panic.
    pub fn panic_in_deactivate(&self, panic: bool) {
        self.0.panic_in_deactivate.store(panic, Ordering::Relaxed);
    }

    /// Makes `rebuild` panic.
    pub fn panic_in_rebuild(&self, panic: bool) {
        self.0.panic_in_rebuild.store(panic, Ordering::Relaxed);
    }

    /// Makes `Controller::latency` panic.
    pub fn panic_in_latency(&self, panic: bool) {
        self.0.panic_in_latency.store(panic, Ordering::Relaxed);
    }

    /// How many parameter edits the next activation's ring holds.
    pub fn edit_capacity(&self, capacity: usize) {
        self.0.edit_capacity.store(capacity, Ordering::Relaxed);
    }

    /// Makes `process` return an error.
    pub fn fail_blocks(&self, fail: bool) {
        self.0.block_fails.store(fail, Ordering::Relaxed);
    }

    /// The latency the plugin reports from now on.
    pub fn set_latency(&self, latency: Samples) {
        self.0.latency.store(latency.0, Ordering::Relaxed);
    }

    /// The tail the plugin reports from now on: none for zero.
    pub fn set_tail(&self, tail: Samples) {
        self.0.tail.store(tail.0, Ordering::Relaxed);
    }

    /// The output channel count the plugin reports from now on.
    pub fn set_outputs(&self, channels: u16) {
        self.0.outputs.store(u32::from(channels), Ordering::Relaxed);
    }

    /// The word the plugin's host callbacks raise, which the shell takes.
    #[must_use]
    pub fn request_word(&self) -> &RequestWord {
        &self.0.requests
    }
}

/// The fake, loaded.
#[derive(Debug)]
pub struct FakeLoaded {
    shared: Arc<Shared>,
    descriptor: PluginDescriptor,
    values: Vec<(ParamId, Normalized)>,
}

/// The realtime half of an active fake.
#[derive(Debug)]
pub struct FakeProcessor {
    shell: Arc<ActivationShell>,
    shared: Arc<Shared>,
    drain: EditDrain,
}

/// The main-thread half of an active fake.
#[derive(Debug)]
pub struct FakeController {
    shell: Arc<ActivationShell>,
    shared: Arc<Shared>,
    descriptor: PluginDescriptor,
    sink: EditSink,
    values: Vec<(ParamId, Normalized)>,
    /// Parameters whose latest value the full ring refused.
    unsent: Vec<ParamId>,
}

/// What the two halves of one activation share, so a pair from two activations
/// is told apart by identity.
#[derive(Debug)]
struct ActivationShell;

fn set_value(values: &mut Vec<(ParamId, Normalized)>, id: ParamId, value: Normalized) {
    match values.iter_mut().find(|(known, _)| *known == id) {
        Some(slot) => slot.1 = value,
        None => values.push((id, value)),
    }
}

macro_rules! fake_control {
    ($ty:ty, $set:expr, $flush:expr) => {
        impl Control for $ty {
            fn info(&self) -> &PluginDescriptor {
                &self.descriptor
            }

            fn wiring(&self, _main: &MainThread) -> LoadedPlugin {
                self.shared.wiring()
            }

            fn parameters(&self, _main: &MainThread) -> Vec<ParameterInfo> {
                self.values
                    .iter()
                    .map(|&(id, _)| ParameterInfo::new(id, "p"))
                    .collect()
            }

            fn parameter(&self, _main: &MainThread, id: ParamId) -> Option<Normalized> {
                self.values.iter().find(|(k, _)| *k == id).map(|&(_, v)| v)
            }

            fn set_parameter(&mut self, _main: &MainThread, id: ParamId, value: Normalized) {
                self.shared.log.push(Call::SetParameter(id));
                set_value(&mut self.values, id, value);
                let set: fn(&mut Self, ParamId, Normalized) = $set;
                set(self, id, value);
            }

            fn save_state(&mut self, _main: &MainThread) -> PluginResult<Vec<u8>> {
                self.shared.log.push(Call::SaveState);
                Ok(self.shared.state.lock().map_err(|_| poisoned())?.clone())
            }

            fn load_state(&mut self, _main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
                self.shared.log.push(Call::LoadState(bytes.len()));
                *self.shared.state.lock().map_err(|_| poisoned())? = bytes.to_vec();
                Ok(())
            }

            fn flush_edits(&mut self, main: &MainThread) {
                let flush: fn(&mut Self, &MainThread) = $flush;
                flush(self, main);
            }

            fn rebuild_midi_mapping(&mut self, _main: &MainThread) {
                self.shared.log.push(Call::RebuildMidiMapping);
            }

            fn rescan_parameters(&mut self, _main: &MainThread, scope: RescanScope) {
                self.shared.log.push(Call::RescanParameters(scope));
            }

            fn run_main_thread(&mut self, _main: &MainThread) {
                self.shared.log.push(Call::RunMainThread);
            }

            fn run_timer(&mut self, _main: &MainThread, id: TimerId) {
                self.shared.log.push(Call::RunTimer(id));
            }

            fn service_fd(&mut self, _main: &MainThread, id: FdId) {
                self.shared.log.push(Call::ServiceFd(id));
            }

            /// The fake never asks for anything.
            fn take_requests(&mut self, _main: &MainThread, _out: &mut Vec<Request>) {}

            /// The fake cannot change mode live.
            fn set_render_mode(&mut self, _main: &MainThread, _mode: RenderMode) -> bool {
                false
            }

            /// The fake reports no edits outside a block.
            fn take_reported_edits(
                &mut self,
                _main: &MainThread,
                _out: &mut Vec<(ParamId, Normalized)>,
            ) {
            }

            /// `0`: the fake reports edits in `ProcessOutput`, like CLAP and VST3.
            fn dropped_reported_edits(&self) -> u64 {
                0
            }
        }

        /// The fake has no editor.
        impl EditorWindow for $ty {
            fn has_editor(&self, _main: &MainThread) -> bool {
                false
            }

            fn open_editor(
                &mut self,
                _main: &MainThread,
                _parent: WindowHandle,
                _scale: ContentScale,
            ) -> PluginResult<EditorSize> {
                Err(PluginError::Editor(EditorError::PluginError(
                    "the fake plugin has no editor".into(),
                )))
            }

            fn close_editor(&mut self, _main: &MainThread) {}

            fn editor_idle(&mut self, _main: &MainThread) {}

            /// The default capabilities: this surface reports no resize support.
            fn editor_capabilities(
                &mut self,
                _main: &crate::MainThread,
            ) -> crate::EditorCapabilities {
                crate::EditorCapabilities::default()
            }

            /// Not supported here.
            fn set_editor_size(
                &mut self,
                _main: &crate::MainThread,
                _requested: crate::EditorSize,
            ) -> crate::PluginResult<crate::EditorSize> {
                Err(crate::PluginError::Editor(crate::EditorError::PluginError(
                    "set_editor_size not supported".into(),
                )))
            }

            /// Nothing asked: this surface never queues a resize request.
            fn poll_editor_resize_request(
                &mut self,
                _main: &crate::MainThread,
            ) -> Option<crate::EditorSize> {
                None
            }
        }
    };
}

fn poisoned() -> PluginError {
    PluginError::State("the fake's state lock was poisoned".into())
}

fake_control!(FakeLoaded, |_, _, _| {}, |_, _| {});
fake_control!(
    FakeController,
    |this, id, value| {
        // The edit reaches the Processor at the head of its next block. A full
        // ring is counted and the latest value stays in `values`, for `flush`.
        if this.sink.send(crate::Edit::Param { id, value }).is_ok() {
            this.unsent.retain(|&known| known != id);
        } else if !this.unsent.contains(&id) {
            this.unsent.push(id);
        }
    },
    |this, main| {
        for id in std::mem::take(&mut this.unsent) {
            let Some(value) = this.parameter(main, id) else {
                continue;
            };
            if this.sink.send(crate::Edit::Param { id, value }).is_err() {
                this.unsent.push(id);
            }
        }
    }
);

impl Loaded for FakeLoaded {
    type Processor = FakeProcessor;
    type Controller = FakeController;

    const RESTARTS: RestartRules = RestartRules::CLAP;

    fn activate(
        self,
        _main: &MainThread,
        prepare: Prepare,
    ) -> Result<(FakeProcessor, FakeController), Refused<Self>> {
        let shared = self.shared.clone();
        shared.log.push(Call::Activate(
            prepare.rate(),
            prepare.max_block().samples(),
        ));
        assert!(
            !shared.panic_on_activate.load(Ordering::Relaxed),
            "the fake was told to panic in activate"
        );
        let refused = f64::from_bits(shared.refuse_rate.load(Ordering::Relaxed));
        if refused != 0.0 && refused == prepare.rate().0 {
            let reason = PluginError::Other(format!("the fake refuses {refused} Hz"));
            let plugin = (!shared.lose_on_refuse.load(Ordering::Relaxed)).then_some(self);
            return Err(Refused { plugin, reason });
        }
        let shell = Arc::new(ActivationShell);
        let (sink, drain) = edits(shared.edit_capacity.load(Ordering::Relaxed).max(1));
        Ok((
            FakeProcessor {
                shell: shell.clone(),
                shared: shared.clone(),
                drain,
            },
            FakeController {
                shell,
                shared,
                descriptor: self.descriptor,
                sink,
                values: self.values,
                unsent: Vec::new(),
            },
        ))
    }

    fn deactivate(
        _main: &MainThread,
        processor: FakeProcessor,
        controller: FakeController,
    ) -> Result<Self, Stuck<FakeProcessor, FakeController>> {
        if !Arc::ptr_eq(&processor.shell, &controller.shell) {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other("the halves are of two activations".into()),
            });
        }
        let shared = controller.shared.clone();
        assert!(
            !shared.panic_in_deactivate.load(Ordering::Relaxed),
            "the fake was told to panic in deactivate"
        );
        if shared
            .stuck
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other("the fake will not stop".into()),
            });
        }
        shared.log.push(Call::Deactivate);
        Ok(FakeLoaded {
            shared,
            descriptor: controller.descriptor,
            values: controller.values,
        })
    }

    fn rebuild(self, _main: &MainThread) -> Result<Self, Refused<Self>> {
        self.shared.log.push(Call::Rebuild);
        assert!(
            !self.shared.panic_in_rebuild.load(Ordering::Relaxed),
            "the fake was told to panic in rebuild"
        );
        if self.shared.rebuild_fails.load(Ordering::Relaxed) {
            return Err(Refused {
                plugin: Some(self),
                reason: PluginError::Other("the fake cannot rebuild".into()),
            });
        }
        Ok(self)
    }
}

impl Controller for FakeController {
    type Loaded = FakeLoaded;
    type Processor = FakeProcessor;

    fn latency(&self, _main: &MainThread) -> Samples {
        self.shared.log.push(Call::Latency);
        assert!(
            !self.shared.panic_in_latency.load(Ordering::Relaxed),
            "the fake was told to panic in latency"
        );
        Samples(self.shared.latency.load(Ordering::Relaxed))
    }

    fn tail(&self, _main: &MainThread) -> PluginTail {
        self.shared.log.push(Call::Tail);
        self.shared.tail()
    }
}

impl Processor for FakeProcessor {
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
        let log = &self.shared.log;
        self.drain.drain(|edit| {
            if let crate::Edit::Param { id, value } = edit {
                log.push(Call::Edit(id, value));
            }
        });
        let frames = match &mut io.buffer {
            AudioBufferMut::F32(buffer) => {
                buffer.clear_outputs();
                buffer.num_samples
            }
            AudioBufferMut::F64(buffer) => {
                buffer.clear_outputs();
                buffer.num_samples
            }
        };
        log.push(Call::Process(frames));
        if self.shared.block_fails.load(Ordering::Relaxed) {
            return Err(PluginError::Process("the fake failed the block".into()));
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.shared.log.push(Call::Stop);
    }

    fn reset(&mut self) {
        self.shared.log.push(Call::Reset);
    }
}

impl From<&FakeLoaded> for Layout {
    fn from(fake: &FakeLoaded) -> Self {
        Layout::from(&fake.shared.wiring())
    }
}
