//! The plugin lifecycle as a pure function.
//!
//! [`step`] decides activation, restart, rescan and the latency, tail and
//! layout re-reads for every plugin format. It has no FFI, no plugin and no
//! clock: an [`Event`] goes in, a new [`State`] and the [`Effect`]s to perform
//! come out. A shell performs each effect against its plugin and feeds the
//! result back as [`Event::Replied`].
//!
//! Everything here runs on the control (main) thread. The only effects that
//! touch the realtime thread are [`Effect::Recall`] and [`Effect::Lend`], which
//! move the Processor between the threads.
//!
//! # Plans
//!
//! A transition is a split or a rejoin: [`Effect::CallActivate`] turns a whole
//! plugin into two halves, [`Effect::CallDeactivate`] and [`Effect::Rebuild`]
//! need the halves back together. A plan that deactivates or rebuilds an active
//! plugin therefore starts with [`Effect::Recall`]. A step emits effects up to
//! and including the next one that can fail or wait (`Recall`, `CallDeactivate`,
//! `Rebuild`, `CallActivate`) and keeps the rest of the plan in the [`State`]; the
//! reply to that effect continues it. A plan parked behind a recall that found
//! the Processor lent runs once [`Event::Returned`] arrives.

use arrayvec::ArrayVec;

use crate::{
    Layout, MaxBlock, PluginError, PluginTail, RenderMode, SampleFormat, SampleRate, Samples,
};

/// How many effects one [`step`] can emit.
///
/// A bound, so a step never allocates its output and never grows with its
/// input.
pub const EFFECT_CAPACITY: usize = 14;

/// The effects of one [`step`], in the order to perform them.
pub type Effects = ArrayVec<Effect, EFFECT_CAPACITY>;

/// How many due timers one tick runs. The rest stay pending: see
/// [`State::has_backlog`].
pub const TIMERS_PER_TICK: usize = 3;

/// How many ready file descriptors one tick services. The rest stay pending:
/// see [`State::has_backlog`].
pub const FDS_PER_TICK: usize = 3;

/// The most effects one [`step`] emits: a tick that runs a full set of timers
/// and descriptors, reads the latency, tail and layout, notifies a values
/// rescan, reads and notifies an info rescan, rebuilds the MIDI mapping and
/// notifies a note-expression change. No other step comes near it;
/// the assertion below keeps [`EFFECT_CAPACITY`] at or above it.
pub const MAX_STEP_EFFECTS: usize = TIMERS_PER_TICK + FDS_PER_TICK + 8;

const _: () = assert!(MAX_STEP_EFFECTS <= EFFECT_CAPACITY);

// The id widths follow the formats' ABIs (CLAP's `clap_id` timer, a POSIX fd).

/// A plugin timer, as the format names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u32);

/// A plugin file descriptor registration, as the format names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FdId(pub i32);

/// What `activate` needs, and what a change of any field costs: a deactivate
/// and an activate.
///
/// A rate is finite and positive and a block holds at least one frame, so no
/// host is handed an activation its format would refuse.
///
/// # Examples
///
/// ```
/// use tutti_plugin_types::{MaxBlock, Prepare, RenderMode};
/// use tutti_types::{SampleRate, Samples};
///
/// let block = MaxBlock::of(Samples(512));
/// let prepare = Prepare::new(SampleRate(48_000.0), block).unwrap();
/// assert_eq!(prepare.mode(), RenderMode::Realtime);
/// assert!(Prepare::new(SampleRate(f64::NAN), block).is_none());
/// assert!(Prepare::new(SampleRate(0.0), block).is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prepare {
    rate: SampleRate,
    max_block: MaxBlock,
    format: SampleFormat,
    mode: RenderMode,
}

impl Prepare {
    /// An activation at `rate` with blocks of up to `max_block` frames, in
    /// 32-bit audio under realtime pressure. [`None`] if `rate` is not finite
    /// and positive.
    #[must_use]
    pub fn new(rate: SampleRate, max_block: MaxBlock) -> Option<Self> {
        Some(Self {
            rate: valid_rate(rate)?,
            max_block,
            format: SampleFormat::default(),
            mode: RenderMode::default(),
        })
    }

    /// This activation, at `rate`. [`None`] if `rate` is not finite and
    /// positive.
    #[must_use]
    pub fn with_rate(self, rate: SampleRate) -> Option<Self> {
        Some(Self {
            rate: valid_rate(rate)?,
            ..self
        })
    }

    /// This activation, exchanging audio in `format`.
    #[must_use]
    pub const fn with_format(mut self, format: SampleFormat) -> Self {
        self.format = format;
        self
    }

    /// This activation, under `mode`.
    #[must_use]
    pub const fn with_mode(mut self, mode: RenderMode) -> Self {
        self.mode = mode;
        self
    }

    /// The sample rate blocks are rendered at.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The most frames one block carries.
    #[must_use]
    pub const fn max_block(&self) -> MaxBlock {
        self.max_block
    }

    /// The sample width audio is exchanged in.
    #[must_use]
    pub const fn format(&self) -> SampleFormat {
        self.format
    }

    /// Whether blocks arrive under realtime pressure.
    #[must_use]
    pub const fn mode(&self) -> RenderMode {
        self.mode
    }
}

fn valid_rate(rate: SampleRate) -> Option<SampleRate> {
    (rate.get().is_finite() && rate.get() > 0.0).then_some(rate)
}

/// Which changes a format needs a restart for.
///
/// A format not listed here re-reads the figure instead. A plugin's own
/// request to restart, a reload and a rescan of everything restart on every
/// format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestartRules {
    /// An audio-ports change (`IoChanged`) needs a deactivate and an activate.
    pub io: bool,
    /// A latency change (`LatencyChanged`) needs a deactivate and an activate.
    pub latency: bool,
}

impl RestartRules {
    /// CLAP: latency and I/O.
    pub const CLAP: Self = Self {
        io: true,
        latency: true,
    };
    /// VST3: I/O and latency (and a reload, which every format restarts for).
    pub const VST3: Self = Self {
        io: true,
        latency: true,
    };
    /// VST2 and AU: none, they re-read.
    pub const NONE: Self = Self {
        io: false,
        latency: false,
    };
}

/// What a host asks of a plugin, an effect finished, or the plugin asked.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug)]
pub enum Event {
    /// An operation the host asks for.
    Op(HostOp),
    /// A plugin-initiated call, raised on any thread, delivered here.
    Request(Request),
    /// The main-thread idle pass: act on what has been requested.
    Tick,
    /// What the shell found when it interpreted the last [`Effect`].
    Replied(Reply),
    /// The Processor is back from the realtime thread after a
    /// [`Effect::Recall`] that answered [`Reply::Lent`].
    Returned,
    /// The process hosting the plugin is gone.
    Died,
}

/// What the host asks for.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostOp {
    /// Split the plugin for rendering. A plugin already active is left as it is.
    Activate(Prepare),
    /// A new rate, block size, format or mode: deactivate and activate again.
    /// On a plugin that is not active this is [`Activate`](Self::Activate).
    Reconfigure(Prepare),
    /// Rejoin the halves and stop rendering.
    Deactivate,
}

/// What the plugin called on the host.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// The plugin asked to be restarted.
    Restart,
    /// An audio-ports rescan (CLAP), `kIoChanged` (VST3), `audioMasterIOChanged`
    /// (VST2), a stream-format property (AU).
    IoChanged,
    /// Parameters changed; the scope says how much.
    RescanParams(RescanScope),
    /// The latency changed.
    LatencyChanged,
    /// The tail changed.
    TailChanged,
    /// `kReloadComponent`: rebuild the instance.
    Reload,
    /// `request_callback` (CLAP): run `on_main_thread`. Dropped while the
    /// plugin is `Refused`, which has nothing to run.
    Callback,
    /// VST3 `kMidiCCAssignmentChanged`: the plugin's controller-to-parameter
    /// mapping changed, so the host rebuilds its own.
    MidiCcAssignmentChanged,
    /// VST3 `kNoteExpressionChanged`: the plugin's note-expression types changed.
    NoteExpressionChanged,
    /// A registered timer is due.
    Timer(TimerId),
    /// A registered file descriptor is ready.
    Fd(FdId),
}

/// How much of the parameter set a rescan touches.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanScope {
    /// Values only.
    Values,
    /// Display text only.
    Text,
    /// Names, ranges and flags.
    Info,
    /// Everything, including a change that needs a restart.
    All,
}

/// What the shell is asked to do. Each effect but [`Notify`](Self::Notify) is
/// answered by an [`Event::Replied`]; `Notify` is delivered to the host and
/// has no reply.
///
/// Exhaustive: every shell matches every variant.
#[derive(Debug)]
pub enum Effect {
    /// Splits the plugin into a Processor and a Controller.
    CallActivate(Prepare),
    /// Rejoins the halves. Needs both in hand, so [`Recall`](Self::Recall)
    /// precedes it.
    CallDeactivate,
    /// Rebuild the instance from its load parameters (VST3 reload). Needs the
    /// plugin loaded, so a `Recall` and a `CallDeactivate` precede it.
    Rebuild,
    /// Ask the realtime thread to hand the Processor back, between blocks.
    Recall,
    /// Give the Processor to the realtime thread.
    Lend,
    /// Read the latency from the plugin.
    ReadLatency,
    /// Read the tail from the plugin.
    ReadTail,
    /// Read the audio-port layout from the plugin.
    ReadLayout,
    /// Re-read the parameter set.
    ReadParams(RescanScope),
    /// Rebuild the host's MIDI controller-to-parameter mapping from the plugin.
    RebuildMidiMapping,
    /// Run the plugin's main-thread work (CLAP `on_main_thread`).
    RunMainThread,
    /// Run a due plugin timer.
    RunTimer(TimerId),
    /// Service a ready plugin file descriptor.
    ServiceFd(FdId),
    /// Tell the host.
    Notify(Notice),
}

/// What the shell found when it performed an [`Effect`].
///
/// Exhaustive: [`step`] matches every variant.
#[derive(Debug)]
pub enum Reply {
    /// `CallActivate`: the plugin is split.
    Activated,
    /// `CallActivate` or `Rebuild`: the plugin would not take it and is loaded.
    Refused(PluginError),
    /// `CallDeactivate`: the halves are rejoined.
    Deactivated,
    /// `Rebuild`: the instance is rebuilt and loaded.
    Rebuilt,
    /// `ReadLatency`: the figure.
    Latency(Samples),
    /// `ReadTail`: the figure.
    Tail(PluginTail),
    /// `ReadLayout`: the layout.
    Layout(Layout),
    /// The effect needs a state the plugin is not in (a read while loaded).
    NotActive,
    /// `Recall`: the Processor is already held.
    Returned,
    /// `Recall`: the Processor is on the realtime thread; [`Event::Returned`]
    /// follows.
    Lent,
    /// `CallDeactivate`: the plugin would not stop. Both halves are intact.
    Stuck(PluginError),
    /// Any other effect, performed.
    Done,
}

/// What the host is told, in the order it happened.
///
/// Exhaustive: every host matches every variant.
#[derive(Debug)]
pub enum Notice {
    /// The latency moved, or the plugin announced it.
    LatencyChanged(Samples),
    /// The tail moved, or the plugin announced it.
    TailChanged(PluginTail),
    /// The audio-port layout differs from the one the host was last given.
    IoChanged,
    /// Parameter values or their display text changed.
    ParamValuesChanged,
    /// The plugin's note-expression types changed.
    NoteExpressionChanged,
    /// Parameters were added, removed or described differently.
    ParamTitlesChanged,
    /// The instance was rebuilt.
    Reloaded,
    /// The plugin refused to run at `rate`; it renders nothing until an
    /// activation succeeds.
    Refused {
        /// The rate that was refused.
        rate: SampleRate,
        /// Why.
        reason: PluginError,
    },
    /// The plugin would not stop; it is as it was.
    Stuck {
        /// Why.
        reason: PluginError,
    },
    /// A call into the plugin panicked outside an activation, a deactivation or
    /// a rebuild (those tell the host through [`Refused`](Self::Refused)). The
    /// plugin is lost and nothing leaves [`Stage::Poisoned`].
    Lost {
        /// Why.
        reason: PluginError,
    },
}

/// Where a plugin is in its life.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stage {
    /// Loaded and not running.
    Loaded,
    /// Split into a Processor and a Controller.
    Active,
    /// Loaded; it refused to run at `rate` and renders nothing. An activation
    /// retries.
    Refused {
        /// The rate that was refused.
        rate: SampleRate,
    },
    /// The plugin is lost. Reached only by [`Event::Died`], which the shell
    /// sends when a transition leaves no plugin (a failed activation that hands
    /// nothing back, a panic) or the process hosting it is gone. Nothing leaves
    /// this stage.
    Poisoned,
}

/// What a plan still has to do, front first.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Task {
    Deactivate,
    Rebuild,
    Activate(Prepare),
    ReadParams(RescanScope),
    RebuildMidiMapping,
}

/// The effect a plan is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Wait {
    #[default]
    Idle,
    /// A `Recall` is in flight.
    Recall,
    /// The Processor is on the realtime thread: the plan resumes on
    /// [`Event::Returned`].
    Parked,
    Deactivate,
    Rebuild,
    Activate,
}

#[derive(Debug, Clone, Default)]
struct Plan {
    tasks: ArrayVec<Task, 5>,
    wait: Wait,
}

/// What the plugin has asked for and the next tick has not yet acted on.
#[derive(Debug, Clone, Default)]
struct Pending {
    restart: bool,
    io: bool,
    latency: bool,
    tail: bool,
    reload: bool,
    callback: bool,
    values: bool,
    midi_cc: bool,
    note_expr: bool,
    text: bool,
    info: bool,
    all: bool,
    timers: Vec<TimerId>,
    fds: Vec<FdId>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        !(self.restart
            || self.io
            || self.latency
            || self.tail
            || self.reload
            || self.callback
            || self.values
            || self.midi_cc
            || self.note_expr
            || self.text
            || self.info
            || self.all)
            && self.timers.is_empty()
            && self.fds.is_empty()
    }

    fn clear_requests(&mut self) {
        let (timers, fds, callback) = (
            std::mem::take(&mut self.timers),
            std::mem::take(&mut self.fds),
            self.callback,
        );
        *self = Self::default();
        self.timers = timers;
        self.fds = fds;
        self.callback = callback;
    }
}

/// The lifecycle decisions of one plugin: where it is, what it last reported
/// and what it still has to do.
///
/// A value: [`step`] consumes one and returns the next.
#[derive(Debug, Clone)]
pub struct State {
    rules: RestartRules,
    stage: Stage,
    prepare: Option<Prepare>,
    latency: Option<Samples>,
    tail: Option<PluginTail>,
    layout: Option<Layout>,
    announced_latency: bool,
    announced_tail: bool,
    revived: bool,
    after_main: bool,
    /// The host's latest op, raised while an activation was in flight.
    later: Option<HostOp>,
    /// A rebuild was refused: the next activation rebuilds first.
    rebuild_pending: bool,
    /// Parameters changed while no activation was to follow: the next
    /// activation reads them all.
    reread_params: bool,
    pending: Pending,
    plan: Plan,
}

impl State {
    /// A freshly loaded plugin of a format with these restart rules.
    #[must_use]
    pub fn new(rules: RestartRules) -> Self {
        Self {
            rules,
            stage: Stage::Loaded,
            prepare: None,
            latency: None,
            tail: None,
            layout: None,
            announced_latency: false,
            announced_tail: false,
            revived: false,
            after_main: false,
            later: None,
            rebuild_pending: false,
            reread_params: false,
            pending: Pending::default(),
            plan: Plan::default(),
        }
    }

    /// Whether an [`Event::Tick`] would plan nothing: no request is pending, no
    /// plan is in flight and no op is waiting to be replayed.
    ///
    /// A shell that has taken no request since its last pass can skip
    /// [`Lifecycle::drive`](crate::Lifecycle::drive) for the tick while this
    /// holds (and call [`Control::flush_edits`](crate::Control::flush_edits)
    /// itself, which is all else a tick does), so an idle pass of a realtime
    /// thread allocates nothing. Conservative: `false` may still tick to nothing.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        matches!(self.plan.wait, Wait::Idle | Wait::Parked)
            && self.plan.tasks.is_empty()
            && self.later.is_none()
            && !self.after_main
            && self.pending.is_empty()
    }

    /// Where the plugin is.
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Whether the plugin is split and producing audio: true only in
    /// [`Stage::Active`].
    #[must_use]
    pub fn renders(&self) -> bool {
        matches!(self.stage, Stage::Active)
    }

    /// The `Prepare` of the latest activation, requested or running.
    #[must_use]
    pub fn prepare(&self) -> Option<Prepare> {
        self.prepare
    }

    /// Records `mode` in the `Prepare` every later activation uses, without
    /// restarting anything. For a format that applies the render mode live
    /// (CLAP's `render.set`): the host sets it on the plugin itself, and this
    /// keeps a later restart or rate change from putting the old mode back.
    /// An activation already planned takes the new mode too.
    pub fn set_mode(&mut self, mode: RenderMode) {
        if let Some(prepare) = self.prepare.as_mut() {
            prepare.mode = mode;
        }
        for task in &mut self.plan.tasks {
            if let Task::Activate(prepare) = task {
                prepare.mode = mode;
            }
        }
    }

    /// The latency last reported.
    #[must_use]
    pub fn latency(&self) -> Option<Samples> {
        self.latency
    }

    /// The tail last reported.
    #[must_use]
    pub fn tail(&self) -> Option<PluginTail> {
        self.tail
    }

    /// The layout last reported.
    #[must_use]
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }

    /// Whether the rest of a plan waits for the Processor to return from the
    /// realtime thread.
    #[must_use]
    pub fn is_parked(&self) -> bool {
        self.plan.wait == Wait::Parked
    }

    /// Whether timers or file descriptors are left over from a tick that ran
    /// its share ([`TIMERS_PER_TICK`], [`FDS_PER_TICK`]): the shell ticks again.
    #[must_use]
    pub fn has_backlog(&self) -> bool {
        !self.pending.timers.is_empty() || !self.pending.fds.is_empty()
    }

    /// Whether a plan is under way, so an effect's reply is awaited.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.plan.wait != Wait::Idle
    }
}

/// Decides what happens next.
///
/// Total: every event in every state yields a state and at most
/// [`EFFECT_CAPACITY`] effects. A reply that answers no effect in flight is
/// ignored.
#[must_use = "the effects are the decisions"]
pub fn step(mut state: State, event: Event) -> (State, Effects) {
    let mut out = Effects::new();
    state.on_event(event, &mut out);
    (state, out)
}

fn emit(out: &mut Effects, effect: Effect) {
    // A dropped effect would wedge the plugin mid-plan, so overflow is a bug
    // that must stop. `MAX_STEP_EFFECTS` bounds every step below the capacity.
    assert!(
        out.try_push(effect).is_ok(),
        "a step exceeded EFFECT_CAPACITY"
    );
}

impl State {
    fn on_event(&mut self, event: Event, out: &mut Effects) {
        if matches!(self.stage, Stage::Poisoned) {
            return;
        }
        match event {
            Event::Died => self.poison(),
            Event::Op(op) => self.on_op(op, out),
            Event::Request(request) => self.on_request(request),
            Event::Tick => self.tick(out, true),
            Event::Replied(reply) => self.on_reply(reply, out),
            Event::Returned => {
                if self.plan.wait == Wait::Parked {
                    self.advance(out);
                }
            }
        }
    }

    fn poison(&mut self) {
        self.stage = Stage::Poisoned;
        self.plan = Plan::default();
        self.pending = Pending::default();
        self.after_main = false;
        self.later = None;
        self.rebuild_pending = false;
        self.reread_params = false;
    }

    fn on_op(&mut self, op: HostOp, out: &mut Effects) {
        // An op raised while an activation is in flight cannot change the
        // plan yet: the latest one is replayed once the activation is answered.
        if self.plan.wait == Wait::Activate {
            self.later = Some(merge_later(self.later, op));
            return;
        }
        match op {
            HostOp::Activate(p) => match self.plan.wait {
                Wait::Idle => {
                    if !matches!(self.stage, Stage::Active) {
                        self.begin(&[Task::Activate(p)], out);
                    }
                }
                // An activate leaves an active plugin as it is, and a plan that
                // ends active already has its `Prepare`.
                _ if self.leads_active() => {}
                _ => self.plan.set_activate(p),
            },
            HostOp::Reconfigure(p) => match (self.plan.wait, self.stage) {
                (Wait::Idle, Stage::Active) => {
                    self.begin(&[Task::Deactivate, Task::Activate(p)], out)
                }
                (Wait::Idle, _) => self.begin(&[Task::Activate(p)], out),
                _ => self.plan.set_activate(p),
            },
            HostOp::Deactivate => match (self.plan.wait, self.stage) {
                (Wait::Idle, Stage::Active) => self.begin(&[Task::Deactivate], out),
                (Wait::Recall | Wait::Parked, _) => {
                    self.plan.tasks.clear();
                    self.plan.tasks.push(Task::Deactivate);
                }
                (Wait::Deactivate | Wait::Rebuild, _) => self.plan.tasks.clear(),
                _ => {}
            },
        }
    }

    /// Whether the plugin is, or the plan in flight leaves it, active.
    fn leads_active(&self) -> bool {
        match self.plan.wait {
            Wait::Idle => matches!(self.stage, Stage::Active),
            Wait::Activate => true,
            _ => self
                .plan
                .tasks
                .iter()
                .any(|t| matches!(t, Task::Activate(_))),
        }
    }

    /// Starts a plan: an active plugin is recalled first, a loaded one goes
    /// straight to the first task.
    fn begin(&mut self, tasks: &[Task], out: &mut Effects) {
        self.plan.tasks.clear();
        self.plan.tasks.extend(tasks.iter().copied());
        if self.rebuild_pending && !matches!(self.stage, Stage::Active) {
            self.plan.tasks.insert(0, Task::Rebuild);
        }
        if matches!(self.stage, Stage::Active) {
            emit(out, Effect::Recall);
            self.plan.wait = Wait::Recall;
        } else {
            self.advance(out);
        }
    }

    /// Emits the plan's tasks up to the next effect that is waited on.
    fn advance(&mut self, out: &mut Effects) {
        self.plan.wait = Wait::Idle;
        while !self.plan.tasks.is_empty() {
            match self.plan.tasks.remove(0) {
                Task::Deactivate => {
                    emit(out, Effect::CallDeactivate);
                    self.plan.wait = Wait::Deactivate;
                    return;
                }
                Task::Rebuild => {
                    emit(out, Effect::Rebuild);
                    self.plan.wait = Wait::Rebuild;
                    return;
                }
                Task::Activate(p) => {
                    if matches!(self.stage, Stage::Refused { .. }) {
                        self.revived = true;
                        self.stage = Stage::Loaded;
                    }
                    self.prepare = Some(p);
                    emit(out, Effect::CallActivate(p));
                    self.plan.wait = Wait::Activate;
                    return;
                }
                Task::RebuildMidiMapping => emit(out, Effect::RebuildMidiMapping),
                Task::ReadParams(scope) => {
                    emit(out, Effect::ReadParams(scope));
                    if matches!(scope, RescanScope::Info | RescanScope::All) {
                        emit(out, Effect::Notify(Notice::ParamTitlesChanged));
                    }
                    if scope == RescanScope::All {
                        emit(out, Effect::Notify(Notice::ParamValuesChanged));
                    }
                }
            }
        }
    }

    fn on_request(&mut self, request: Request) {
        let active = matches!(self.stage, Stage::Active);
        let activating = self.plan.wait == Wait::Activate;
        let accepted = match request {
            // The plugin runs these whenever it is loaded.
            Request::Callback | Request::Timer(_) | Request::Fd(_) => {
                active || matches!(self.stage, Stage::Loaded)
            }
            // Not while loaded: the reads after an activation answer them.
            Request::LatencyChanged | Request::TailChanged => active,
            _ => active || activating,
        };
        if !accepted {
            return;
        }
        let p = &mut self.pending;
        match request {
            Request::Restart => p.restart = true,
            Request::IoChanged => p.io = true,
            Request::LatencyChanged => p.latency = true,
            Request::TailChanged => p.tail = true,
            Request::Reload => p.reload = true,
            Request::Callback => p.callback = true,
            Request::MidiCcAssignmentChanged => p.midi_cc = true,
            Request::NoteExpressionChanged => p.note_expr = true,
            Request::RescanParams(RescanScope::Values) => p.values = true,
            Request::RescanParams(RescanScope::Text) => p.text = true,
            Request::RescanParams(RescanScope::Info) => p.info = true,
            Request::RescanParams(RescanScope::All) => p.all = true,
            Request::Timer(id) => {
                if !p.timers.contains(&id) {
                    p.timers.push(id);
                }
            }
            Request::Fd(id) => {
                if !p.fds.contains(&id) {
                    p.fds.push(id);
                }
            }
        }
    }
}

/// The op to replay once the activation in flight is answered, given the one
/// kept so far and a new one. The latest op wins by what it asks for: an
/// `Activate` asks only for an active plugin, which the activation in flight
/// already gives, so it never displaces a kept `Reconfigure` or `Activate`, and
/// after a kept `Deactivate` it asks for an activation at its own `Prepare`.
fn merge_later(kept: Option<HostOp>, op: HostOp) -> HostOp {
    match (op, kept) {
        (HostOp::Activate(p), Some(HostOp::Deactivate)) => HostOp::Reconfigure(p),
        (HostOp::Activate(_), Some(kept)) => kept,
        (op, _) => op,
    }
}

impl Plan {
    /// Replaces the activation still to come, or adds one.
    fn set_activate(&mut self, p: Prepare) {
        match self
            .tasks
            .iter_mut()
            .find(|t| matches!(t, Task::Activate(_)))
        {
            Some(task) => *task = Task::Activate(p),
            None => {
                let at = self
                    .tasks
                    .iter()
                    .position(|t| matches!(t, Task::ReadParams(_)))
                    .unwrap_or(self.tasks.len());
                self.tasks.insert(at, Task::Activate(p));
            }
        }
    }
}

impl State {
    /// The idle pass. `callback` is false when this pass continues one that
    /// already ran the plugin's main-thread work.
    fn tick(&mut self, out: &mut Effects, callback: bool) {
        if !matches!(self.plan.wait, Wait::Idle | Wait::Parked) {
            return;
        }
        if !matches!(self.stage, Stage::Active | Stage::Loaded) {
            return;
        }
        // The callback may ask for more, so nothing else is decided until it
        // has run.
        if callback && self.pending.callback {
            self.pending.callback = false;
            self.after_main = true;
            emit(out, Effect::RunMainThread);
            return;
        }
        let timers = self.pending.timers.len().min(TIMERS_PER_TICK);
        for id in self.pending.timers.drain(..timers) {
            emit(out, Effect::RunTimer(id));
        }
        let fds = self.pending.fds.len().min(FDS_PER_TICK);
        for id in self.pending.fds.drain(..fds) {
            emit(out, Effect::ServiceFd(id));
        }
        if !matches!(self.stage, Stage::Active) {
            return;
        }
        if self.plan.wait == Wait::Idle && self.plan_restart(out) {
            return;
        }
        self.read_without_restart(out);
    }

    /// Plans the restart the pending requests call for. Returns whether there
    /// is one.
    fn plan_restart(&mut self, out: &mut Effects) -> bool {
        let rules = self.rules;
        let p = &self.pending;
        let restart =
            p.reload || p.restart || p.all || (p.io && rules.io) || (p.latency && rules.latency);
        let Some(prepare) = self.prepare.filter(|_| restart) else {
            return false;
        };
        let reload = p.reload;
        self.announced_latency |= p.latency;
        self.announced_tail |= p.tail;
        let scope = if reload || p.all {
            Some(RescanScope::All)
        } else if p.info {
            Some(RescanScope::Info)
        } else {
            None
        };
        if scope != Some(RescanScope::All) && (p.values || p.text) {
            emit(out, Effect::Notify(Notice::ParamValuesChanged));
        }
        if p.note_expr {
            emit(out, Effect::Notify(Notice::NoteExpressionChanged));
        }
        let midi_cc = p.midi_cc;
        self.pending.clear_requests();
        let mut tasks = ArrayVec::<Task, 5>::new();
        tasks.push(Task::Deactivate);
        if reload {
            tasks.push(Task::Rebuild);
        }
        tasks.push(Task::Activate(prepare));
        if let Some(scope) = scope {
            tasks.push(Task::ReadParams(scope));
        }
        if midi_cc {
            tasks.push(Task::RebuildMidiMapping);
        }
        self.plan.tasks = tasks;
        emit(out, Effect::Recall);
        self.plan.wait = Wait::Recall;
        true
    }

    /// The reads and notices that need no restart. A request that does is left
    /// pending.
    fn read_without_restart(&mut self, out: &mut Effects) {
        let rules = self.rules;
        let p = &mut self.pending;
        if p.latency && !rules.latency {
            p.latency = false;
            self.announced_latency = true;
            emit(out, Effect::ReadLatency);
        }
        if p.tail {
            p.tail = false;
            self.announced_tail = true;
            emit(out, Effect::ReadTail);
        }
        if p.io && !rules.io {
            p.io = false;
            emit(out, Effect::ReadLayout);
        }
        if p.values || p.text {
            p.values = false;
            p.text = false;
            emit(out, Effect::Notify(Notice::ParamValuesChanged));
        }
        if p.midi_cc {
            p.midi_cc = false;
            emit(out, Effect::RebuildMidiMapping);
        }
        if p.note_expr {
            p.note_expr = false;
            emit(out, Effect::Notify(Notice::NoteExpressionChanged));
        }
        if p.info && !p.all {
            p.info = false;
            emit(out, Effect::ReadParams(RescanScope::Info));
            emit(out, Effect::Notify(Notice::ParamTitlesChanged));
        }
    }

    fn on_reply(&mut self, reply: Reply, out: &mut Effects) {
        let wait = self.plan.wait;
        // `RunMainThread` is emitted alone, so the next reply of any kind is
        // its answer.
        let continuing = std::mem::take(&mut self.after_main);
        match reply {
            Reply::Activated if wait == Wait::Activate => {
                self.stage = Stage::Active;
                emit(out, Effect::ReadLatency);
                emit(out, Effect::ReadTail);
                emit(out, Effect::ReadLayout);
                emit(out, Effect::Lend);
                if std::mem::take(&mut self.reread_params) {
                    self.plan
                        .tasks
                        .retain(|t| !matches!(t, Task::ReadParams(_)));
                    self.plan.tasks.push(Task::ReadParams(RescanScope::All));
                }
                self.advance(out);
                self.replay(out);
            }
            Reply::Refused(reason) if matches!(wait, Wait::Activate | Wait::Rebuild) => {
                let Some(prepare) = self.prepare else { return };
                let rate = prepare.rate;
                self.stage = Stage::Refused { rate };
                self.rebuild_pending |= wait == Wait::Rebuild;
                self.reread_params = false;
                self.plan = Plan::default();
                self.pending = Pending::default();
                self.announced_latency = false;
                self.announced_tail = false;
                self.revived = false;
                emit(out, Effect::Notify(Notice::Refused { rate, reason }));
                self.replay(out);
            }
            Reply::Deactivated if wait == Wait::Deactivate => {
                self.stage = Stage::Loaded;
                self.settle_requests();
                self.advance(out);
            }
            Reply::Stuck(reason) if wait == Wait::Deactivate => {
                self.plan = Plan::default();
                emit(out, Effect::Notify(Notice::Stuck { reason }));
                emit(out, Effect::Lend);
            }
            Reply::Rebuilt if wait == Wait::Rebuild => {
                self.rebuild_pending = false;
                emit(out, Effect::Notify(Notice::Reloaded));
                self.advance(out);
            }
            Reply::Returned if wait == Wait::Recall => self.advance(out),
            Reply::Lent if wait == Wait::Recall => self.plan.wait = Wait::Parked,
            Reply::Latency(now) => {
                let announced = std::mem::take(&mut self.announced_latency);
                let before = self.latency.replace(now);
                if before.is_some_and(|b| b != now || announced) {
                    emit(out, Effect::Notify(Notice::LatencyChanged(now)));
                }
            }
            Reply::Tail(now) => {
                let announced = std::mem::take(&mut self.announced_tail);
                let before = self.tail.replace(now);
                if before.is_some_and(|b| b != now || announced) {
                    emit(out, Effect::Notify(Notice::TailChanged(now)));
                }
            }
            Reply::Layout(now) => {
                let revived = std::mem::take(&mut self.revived);
                let changed = match &self.layout {
                    None => revived,
                    Some(before) => revived || *before != now,
                };
                self.layout = Some(now);
                if changed {
                    emit(out, Effect::Notify(Notice::IoChanged));
                }
            }
            Reply::Done if continuing => self.tick(out, false),
            // A reply to nothing in flight, or one that needs no action.
            _ => {}
        }
    }
}

impl State {
    /// Replays the op that was raised during an activation.
    fn replay(&mut self, out: &mut Effects) {
        if let Some(op) = self.later.take() {
            self.on_op(op, out);
        }
    }

    /// The plugin is deactivated and will be activated again by the plan: the
    /// restart-needing requests raised meanwhile are answered by it. The reads
    /// after the activation answer a latency, tail or layout change; a reload
    /// is folded into the plan; a rescan of everything re-reads the parameters.
    fn settle_requests(&mut self) {
        let p = &mut self.pending;
        let (reload, all) = (p.reload, p.all);
        self.announced_latency |= p.latency;
        self.announced_tail |= p.tail;
        p.restart = false;
        p.io = false;
        p.latency = false;
        p.tail = false;
        p.reload = false;
        p.all = false;
        let tasks = &mut self.plan.tasks;
        if reload && !tasks.contains(&Task::Rebuild) {
            tasks.insert(0, Task::Rebuild);
        }
        if reload || all {
            if tasks.iter().any(|t| matches!(t, Task::Activate(_))) {
                tasks.retain(|t| !matches!(t, Task::ReadParams(_)));
                tasks.push(Task::ReadParams(RescanScope::All));
            } else {
                // No activation follows: the parameters are read after the
                // next one.
                self.reread_params = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;
