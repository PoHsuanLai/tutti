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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use dpi::{PhysicalSize, Size};
use tutti_rt::RequestWord;

use crate::{
    AudioBufferMut, ChannelLayout, ContentScale, Control, Controller, EditDrain, EditSink,
    EditorError, EditorWindow, FdId, Layout, Loaded, LoadedPlugin, MainThread, Normalized,
    ParamEdit, ParamId, ParameterInfo, PluginBus, PluginDescriptor, PluginError, PluginIo,
    PluginResult, PluginTail, Prepare, Processor, Refused, RenderMode, RenderModeSupport, Request,
    RescanScope, RestartRules, SampleRate, Samples, Stuck, TimerId, WindowHandle, edits,
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
    /// `EditorWindow::open_editor`.
    OpenEditor,
    /// `EditorWindow::close_editor`.
    CloseEditor,
    /// `EditorWindow::editor_idle`.
    EditorIdle,
    /// `EditorWindow::set_editor_scale`, with the factor's bits.
    SetEditorScale(u64),
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
            Self::Activate(rate, block) => [1, rate.get().to_bits(), block.0 as u64],
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
            Self::OpenEditor => [19, 0, 0],
            Self::CloseEditor => [20, 0, 0],
            Self::EditorIdle => [21, 0, 0],
            Self::SetEditorScale(bits) => [22, bits, 0],
        }
    }

    fn decode([kind, a, b]: [u64; 3]) -> Option<Self> {
        Some(match kind {
            1 => Self::Activate(SampleRate::new(f64::from_bits(a))?, Samples(b as usize)),
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
            19 => Self::OpenEditor,
            20 => Self::CloseEditor,
            21 => Self::EditorIdle,
            22 => Self::SetEditorScale(a),
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
    panic_in_deactivate: AtomicBool,
    panic_in_rebuild: AtomicBool,
    panic_in_latency: AtomicBool,
    edit_capacity: AtomicUsize,
    latency: AtomicUsize,
    tail: AtomicUsize,
    outputs: AtomicU32,
    requests: RequestWord,
    state: Mutex<Vec<u8>>,
    /// Frames the fake sounds after each trigger, or `0` for a silent fake
    /// (`FakePlugin::ring_out`).
    ring: AtomicUsize,
    /// Whether the fake says it is asleep over sound (`FakePlugin::sleep_over_sound`).
    sleeps_over_sound: AtomicBool,
    /// Whether the fake has an editor (`FakePlugin::with_editor`).
    editor: AtomicBool,
    /// Whether the editor takes a new scale live (`FakePlugin::with_editor_scale`).
    takes_scale: AtomicBool,
    /// The size the plugin asked its window to take, packed `width << 32 | height`,
    /// until the host polls it; `0` when it has not asked.
    resize_request: AtomicU64,
    /// How the fake takes a render mode (`FakePlugin::take_render_mode`):
    /// `0` at activation, `1` live, `2` not at all.
    render_mode_support: AtomicU8,
    /// The mode the fake was last told live: `0` none, `1` realtime, `2`
    /// offline.
    told_mode: AtomicU8,
}

impl Shared {
    fn wiring(&self) -> LoadedPlugin {
        self.log.push(Call::Wiring);
        let outputs = ChannelLayout::try_from(self.outputs.load(Ordering::Relaxed))
            .expect("a fake plugin has few outputs");
        LoadedPlugin {
            inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            outputs: vec![PluginBus::main(outputs)],
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
            panic_in_deactivate: AtomicBool::new(false),
            panic_in_rebuild: AtomicBool::new(false),
            panic_in_latency: AtomicBool::new(false),
            edit_capacity: AtomicUsize::new(64),
            latency: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            outputs: AtomicU32::new(2),
            requests: RequestWord::new(),
            state: Mutex::new(Vec::new()),
            ring: AtomicUsize::new(0),
            sleeps_over_sound: AtomicBool::new(false),
            editor: AtomicBool::new(false),
            takes_scale: AtomicBool::new(false),
            resize_request: AtomicU64::new(0),
            render_mode_support: AtomicU8::new(0),
            told_mode: AtomicU8::new(0),
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
        let bits = rate.map_or(0, |r| r.get().to_bits());
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

    /// The latency the plugin reports from now on.
    pub fn set_latency(&self, latency: Samples) {
        self.0.latency.store(latency.0, Ordering::Relaxed);
    }

    /// The tail the plugin reports from now on: none for zero.
    pub fn set_tail(&self, tail: Samples) {
        self.0.tail.store(tail.0, Ordering::Relaxed);
    }

    /// Makes the fake sound for `frames` after each trigger (a non-silent input
    /// frame or a MIDI event), at 0.5 on every output channel from the
    /// trigger's frame on, and say what CLAP's process status would: *sleep*
    /// in a block that gave it nothing and sounded nothing, *continue*
    /// otherwise. `0`, the default, is a silent fake that says nothing.
    pub fn ring_out(&self, frames: usize) {
        self.0.ring.store(frames, Ordering::Relaxed);
    }

    /// Makes the fake say *sleep* every block, sounding or not: a plugin
    /// that lies about being asleep.
    pub fn sleep_over_sound(&self, lie: bool) {
        self.0.sleeps_over_sound.store(lie, Ordering::Relaxed);
    }

    /// How the fake takes a change of render mode from now on. The default is
    /// [`RenderModeSupport::OnActivation`], as VST3 takes it.
    pub fn take_render_mode(&self, support: RenderModeSupport) {
        let code = match support {
            RenderModeSupport::OnActivation => 0,
            RenderModeSupport::Live => 1,
            RenderModeSupport::Unsupported => 2,
        };
        self.0.render_mode_support.store(code, Ordering::Relaxed);
    }

    /// The mode the fake was last told live, through
    /// [`Control::set_render_mode`]; `None` before any.
    #[must_use]
    pub fn told_render_mode(&self) -> Option<RenderMode> {
        match self.0.told_mode.load(Ordering::Relaxed) {
            1 => Some(RenderMode::Realtime),
            2 => Some(RenderMode::Offline),
            _ => None,
        }
    }

    /// The output channel count the plugin reports from now on.
    pub fn set_outputs(&self, channels: u16) {
        self.0.outputs.store(u32::from(channels), Ordering::Relaxed);
    }

    /// Gives the fake an editor: `open_editor` answers 640 by 480 and the
    /// window calls are logged. The default is none.
    pub fn with_editor(&self, has: bool) {
        self.0.editor.store(has, Ordering::Relaxed);
    }

    /// Makes the fake's editor take a new scale live. The default is the trait's
    /// refusal, `EditorError::ScaleUnsupported`, as VST2 and AU answer.
    pub fn with_editor_scale(&self, takes: bool) {
        self.0.takes_scale.store(takes, Ordering::Relaxed);
    }

    /// The plugin asks its window to take `size`; the host's next
    /// `poll_editor_resize_request` answers it, once. The fake counts in device
    /// pixels, as it answers `open_editor`.
    pub fn request_editor_size(&self, size: PhysicalSize<u32>) {
        let packed = (u64::from(size.width) << 32) | u64::from(size.height);
        self.0.resize_request.store(packed, Ordering::Relaxed);
    }

    /// The word the plugin's host callbacks raise, which the shell takes.
    #[must_use]
    pub fn request_word(&self) -> &RequestWord {
        &self.0.requests
    }
}

/// The fake, loaded.
///
/// `Clone`, so a test can lend it as a `MainThreadControl`: a clone shares the
/// fake's state and call log, and copies its parameter values.
#[derive(Debug, Clone)]
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
    /// Frames of the voice still to sound (`FakePlugin::ring_out`).
    sounding: usize,
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

            fn parameter_plain(
                &self,
                _main: &MainThread,
                _id: ParamId,
                value: Normalized,
            ) -> Option<f64> {
                Some(value.get())
            }

            fn parameter_normalized(
                &self,
                _main: &MainThread,
                _id: ParamId,
                plain: f64,
            ) -> Option<Normalized> {
                Normalized::new(plain)
            }

            fn set_parameter(&mut self, _main: &MainThread, id: ParamId, value: Normalized) {
                self.shared.log.push(Call::SetParameter(id));
                set_value(&mut self.values, id, value);
                let set: fn(&mut Self, ParamId, Normalized) = $set;
                set(self, id, value);
            }

            fn save_state(
                &mut self,
                _main: &MainThread,
                _: $crate::StateUse,
            ) -> PluginResult<Vec<u8>> {
                self.shared.log.push(Call::SaveState);
                Ok(self.shared.state.lock().map_err(|_| poisoned())?.clone())
            }

            fn load_state(
                &mut self,
                _main: &MainThread,
                bytes: &[u8],
                _: $crate::StateUse,
            ) -> PluginResult<()> {
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

            /// As [`FakePlugin::take_render_mode`] set it; at activation by
            /// default, as VST3 takes it.
            fn render_mode_support(&self, _main: &MainThread) -> RenderModeSupport {
                match self.shared.render_mode_support.load(Ordering::Relaxed) {
                    1 => RenderModeSupport::Live,
                    2 => RenderModeSupport::Unsupported,
                    _ => RenderModeSupport::OnActivation,
                }
            }

            /// Takes the mode where the fake is [`RenderModeSupport::Live`],
            /// recording it for [`FakePlugin::told_render_mode`].
            fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
                if self.render_mode_support(main) != RenderModeSupport::Live {
                    return false;
                }
                let code = if mode.is_offline() { 2 } else { 1 };
                self.shared.told_mode.store(code, Ordering::Relaxed);
                true
            }

            /// The fake reports no edits outside a block.
            fn take_reported_edits(&mut self, _main: &MainThread, _out: &mut Vec<ParamEdit>) {}

            /// `0`: the fake reports edits in `ProcessOutput`, like CLAP and VST3.
            fn dropped_reported_edits(&self) -> u64 {
                0
            }
        }

        /// The fake has an editor only when the test gave it one.
        impl EditorWindow for $ty {
            fn has_editor(&self, _main: &MainThread) -> bool {
                self.shared.editor.load(Ordering::Relaxed)
            }

            fn open_editor(
                &mut self,
                _main: &MainThread,
                _parent: WindowHandle,
                _scale: ContentScale,
            ) -> PluginResult<Size> {
                if !self.shared.editor.load(Ordering::Relaxed) {
                    return Err(PluginError::Editor(EditorError::PluginError(
                        "the fake plugin has no editor".into(),
                    )));
                }
                self.shared.log.push(Call::OpenEditor);
                Ok(PhysicalSize::new(640, 480).into())
            }

            fn set_editor_scale(
                &mut self,
                _main: &MainThread,
                scale: ContentScale,
            ) -> PluginResult<()> {
                if !self.shared.takes_scale.load(Ordering::Relaxed) {
                    return Err(PluginError::Editor(EditorError::ScaleUnsupported));
                }
                self.shared
                    .log
                    .push(Call::SetEditorScale(scale.get().to_bits()));
                Ok(())
            }

            fn close_editor(&mut self, _main: &MainThread) {
                self.shared.log.push(Call::CloseEditor);
            }

            fn editor_idle(&mut self, _main: &MainThread) {
                self.shared.log.push(Call::EditorIdle);
            }

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
                _requested: dpi::Size,
            ) -> crate::PluginResult<dpi::Size> {
                Err(crate::PluginError::Editor(crate::EditorError::PluginError(
                    "set_editor_size not supported".into(),
                )))
            }

            /// What the test asked the plugin to request, once.
            fn poll_editor_resize_request(
                &mut self,
                _main: &crate::MainThread,
            ) -> Option<dpi::Size> {
                match self.shared.resize_request.swap(0, Ordering::Relaxed) {
                    0 => None,
                    packed => {
                        Some(dpi::PhysicalSize::new((packed >> 32) as u32, packed as u32).into())
                    }
                }
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
        if refused != 0.0 && refused == prepare.rate().get() {
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
                sounding: 0,
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
        let ring = self.shared.ring.load(Ordering::Relaxed);
        let frames = match &mut io.buffer {
            AudioBufferMut::F32(buffer) => {
                buffer.clear_outputs();
                if ring > 0 {
                    // A voice: every trigger (a non-silent input frame, a MIDI
                    // event at the frame) starts `ring` frames of 0.5.
                    let mut triggered = false;
                    for i in 0..buffer.num_samples {
                        let hit = io
                            .ctx
                            .midi_events
                            .iter()
                            .any(|(at, _)| at.get() as usize == i)
                            || buffer.inputs.iter().any(|c| c[i] != 0.0);
                        if hit {
                            self.sounding = ring;
                            triggered = true;
                        }
                        if self.sounding > 0 {
                            for out in buffer.outputs.iter_mut() {
                                out[i] = 0.5;
                            }
                            self.sounding -= 1;
                            triggered = true;
                        }
                    }
                    io.out.set_status(
                        if triggered && !self.shared.sleeps_over_sound.load(Ordering::Relaxed) {
                            crate::BlockStatus::Continue
                        } else {
                            crate::BlockStatus::Sleep
                        },
                    );
                }
                buffer.num_samples
            }
            AudioBufferMut::F64(buffer) => {
                buffer.clear_outputs();
                buffer.num_samples
            }
        };
        log.push(Call::Process(frames));
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
