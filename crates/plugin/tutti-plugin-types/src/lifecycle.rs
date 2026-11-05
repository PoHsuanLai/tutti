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

use crate::{Layout, PluginError, PluginTail, RenderMode, SampleFormat, SampleRate, Samples};

/// How many effects one [`step`] can emit.
///
/// A bound, so a step never allocates its output and never grows with its
/// input.
pub const EFFECT_CAPACITY: usize = 12;

/// The effects of one [`step`], in the order to perform them.
pub type Effects = ArrayVec<Effect, EFFECT_CAPACITY>;

/// How many distinct timers one [`State`] keeps pending between ticks.
///
/// A timer raised past the bound is dropped; the shell raises a due timer
/// again on its next pass.
pub const PENDING_TIMERS: usize = 3;

/// How many distinct file descriptors one [`State`] keeps pending between
/// ticks. A descriptor raised past the bound is dropped; a ready descriptor is
/// reported again.
pub const PENDING_FDS: usize = 3;

/// A plugin timer, as the format names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u32);

/// A plugin file descriptor registration, as the format names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FdId(pub i32);

/// What `activate` needs, and what a change of any field costs: a deactivate
/// and an activate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prepare {
    /// The sample rate blocks are rendered at.
    pub rate: SampleRate,
    /// The most frames one block carries.
    pub max_block: Samples,
    /// The sample width audio is exchanged in.
    pub format: SampleFormat,
    /// Whether blocks arrive under realtime pressure.
    pub mode: RenderMode,
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
    /// `request_callback` (CLAP): run `on_main_thread`.
    Callback,
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

/// What the shell is asked to do. Each effect is answered by an
/// [`Event::Replied`].
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
    /// A transition failed and left no plugin, or the process hosting it died.
    /// Nothing leaves this stage.
    Poisoned,
}

/// What a plan still has to do, front first.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Task {
    Deactivate,
    Rebuild,
    Activate(Prepare),
    ReadParams(RescanScope),
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
    tasks: ArrayVec<Task, 4>,
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
    text: bool,
    info: bool,
    all: bool,
    timers: ArrayVec<TimerId, PENDING_TIMERS>,
    fds: ArrayVec<FdId, PENDING_FDS>,
}

impl Pending {
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
    pending: Pending,
    plan: Plan,
}

impl State {
    /// A freshly loaded plugin of a format with these restart rules.
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
            pending: Pending::default(),
            plan: Plan::default(),
        }
    }

    /// Where the plugin is.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Whether the plugin is split and producing audio: true only in
    /// [`Stage::Active`].
    pub fn renders(&self) -> bool {
        matches!(self.stage, Stage::Active)
    }

    /// The `Prepare` of the latest activation, requested or running.
    pub fn prepare(&self) -> Option<Prepare> {
        self.prepare
    }

    /// The latency last reported.
    pub fn latency(&self) -> Option<Samples> {
        self.latency
    }

    /// The tail last reported.
    pub fn tail(&self) -> Option<PluginTail> {
        self.tail
    }

    /// The layout last reported.
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }

    /// Whether the rest of a plan waits for the Processor to return from the
    /// realtime thread.
    pub fn is_parked(&self) -> bool {
        self.plan.wait == Wait::Parked
    }

    /// Whether a plan is under way, so an effect's reply is awaited.
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
    let pushed = out.try_push(effect);
    debug_assert!(pushed.is_ok(), "a step exceeded EFFECT_CAPACITY");
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
    }

    fn on_op(&mut self, op: HostOp, out: &mut Effects) {
        match op {
            HostOp::Activate(p) => {
                if self.plan.wait == Wait::Idle && !matches!(self.stage, Stage::Active) {
                    self.begin(&[Task::Activate(p)], out);
                }
            }
            HostOp::Reconfigure(p) => match (self.plan.wait, self.stage) {
                (Wait::Idle, Stage::Active) => {
                    self.begin(&[Task::Deactivate, Task::Activate(p)], out)
                }
                (Wait::Idle, _) => self.begin(&[Task::Activate(p)], out),
                (Wait::Activate, _) => {}
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

    /// Starts a plan: an active plugin is recalled first, a loaded one goes
    /// straight to the first task.
    fn begin(&mut self, tasks: &[Task], out: &mut Effects) {
        self.plan.tasks.clear();
        self.plan.tasks.extend(tasks.iter().copied());
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
            // Raised while the plugin activates: the reads after it answer.
            Request::LatencyChanged | Request::TailChanged => active && !activating,
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
            Request::RescanParams(RescanScope::Values) => p.values = true,
            Request::RescanParams(RescanScope::Text) => p.text = true,
            Request::RescanParams(RescanScope::Info) => p.info = true,
            Request::RescanParams(RescanScope::All) => p.all = true,
            Request::Timer(id) => {
                if !p.timers.contains(&id) {
                    let _ = p.timers.try_push(id);
                }
            }
            Request::Fd(id) => {
                if !p.fds.contains(&id) {
                    let _ = p.fds.try_push(id);
                }
            }
        }
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
        for id in std::mem::take(&mut self.pending.timers) {
            emit(out, Effect::RunTimer(id));
        }
        for id in std::mem::take(&mut self.pending.fds) {
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
        self.pending.clear_requests();
        let mut tasks = ArrayVec::<Task, 4>::new();
        tasks.push(Task::Deactivate);
        if reload {
            tasks.push(Task::Rebuild);
        }
        tasks.push(Task::Activate(prepare));
        if let Some(scope) = scope {
            tasks.push(Task::ReadParams(scope));
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
        if p.info && !p.all {
            p.info = false;
            emit(out, Effect::ReadParams(RescanScope::Info));
            emit(out, Effect::Notify(Notice::ParamTitlesChanged));
        }
    }

    fn on_reply(&mut self, reply: Reply, out: &mut Effects) {
        let wait = self.plan.wait;
        match reply {
            Reply::Activated if wait == Wait::Activate => {
                self.stage = Stage::Active;
                // The reads below answer these.
                self.pending.latency = false;
                self.pending.tail = false;
                emit(out, Effect::ReadLatency);
                emit(out, Effect::ReadTail);
                emit(out, Effect::ReadLayout);
                emit(out, Effect::Lend);
                self.advance(out);
            }
            Reply::Refused(reason) if matches!(wait, Wait::Activate | Wait::Rebuild) => {
                let Some(prepare) = self.prepare else { return };
                let rate = prepare.rate;
                self.stage = Stage::Refused { rate };
                self.plan = Plan::default();
                self.pending.clear_requests();
                self.announced_latency = false;
                self.announced_tail = false;
                self.revived = false;
                emit(out, Effect::Notify(Notice::Refused { rate, reason }));
            }
            Reply::Deactivated if wait == Wait::Deactivate => {
                self.stage = Stage::Loaded;
                self.advance(out);
            }
            Reply::Stuck(reason) if wait == Wait::Deactivate => {
                self.plan = Plan::default();
                emit(out, Effect::Notify(Notice::Stuck { reason }));
                emit(out, Effect::Lend);
            }
            Reply::Rebuilt if wait == Wait::Rebuild => {
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
            Reply::Done if self.after_main => {
                self.after_main = false;
                self.tick(out, false);
            }
            // A reply to nothing in flight, or one that needs no action.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
