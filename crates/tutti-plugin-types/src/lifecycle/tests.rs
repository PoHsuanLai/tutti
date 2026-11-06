//! State-table tests and properties of [`step`], with no plugin loaded.
//!
//! A [`Shell`] stands in for the interpreter: it performs each effect against
//! a model of the plugin (is it split, where is the Processor) and feeds the
//! reply back, recording what each step did. Properties run arbitrary event
//! sequences through it.
//!
//! Each test's comment names the mutation that makes it fail. Every mutation
//! was applied to `step`, the test watched to fail, and the code restored.

use std::collections::VecDeque;

use proptest::prelude::*;

use super::*;
use crate::ChannelLayout;

// ---------------------------------------------------------------- vocabulary

/// An effect with its payload reduced to what the tests compare.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fx {
    Activate(Prepare),
    Deactivate,
    Rebuild,
    Recall,
    Lend,
    Latency,
    Tail,
    Layout,
    Params(RescanScope),
    Main,
    Timer(u32),
    Fd(i32),
    LatencyNotice(usize),
    TailNotice,
    IoNotice,
    ParamValues,
    ParamTitles,
    MidiMapping,
    NoteExprNotice,
    Reloaded,
    RefusedNotice,
    StuckNotice,
    LostNotice,
}

fn tag(effect: &Effect) -> Fx {
    match effect {
        Effect::CallActivate(p) => Fx::Activate(*p),
        Effect::CallDeactivate => Fx::Deactivate,
        Effect::Rebuild => Fx::Rebuild,
        Effect::Recall => Fx::Recall,
        Effect::Lend => Fx::Lend,
        Effect::ReadLatency => Fx::Latency,
        Effect::ReadTail => Fx::Tail,
        Effect::ReadLayout => Fx::Layout,
        Effect::ReadParams(s) => Fx::Params(*s),
        Effect::RebuildMidiMapping => Fx::MidiMapping,
        Effect::RunMainThread => Fx::Main,
        Effect::RunTimer(id) => Fx::Timer(id.0),
        Effect::ServiceFd(id) => Fx::Fd(id.0),
        Effect::Notify(Notice::LatencyChanged(s)) => Fx::LatencyNotice(s.0),
        Effect::Notify(Notice::TailChanged(_)) => Fx::TailNotice,
        Effect::Notify(Notice::IoChanged) => Fx::IoNotice,
        Effect::Notify(Notice::ParamValuesChanged) => Fx::ParamValues,
        Effect::Notify(Notice::ParamTitlesChanged) => Fx::ParamTitles,
        Effect::Notify(Notice::NoteExpressionChanged) => Fx::NoteExprNotice,
        Effect::Notify(Notice::Reloaded) => Fx::Reloaded,
        Effect::Notify(Notice::Refused { .. }) => Fx::RefusedNotice,
        Effect::Notify(Notice::Stuck { .. }) => Fx::StuckNotice,
        Effect::Notify(Notice::Lost { .. }) => Fx::LostNotice,
    }
}

fn prep(n: u8) -> Prepare {
    Prepare {
        rate: SampleRate(if n.is_multiple_of(2) {
            48_000.0
        } else {
            44_100.0
        }),
        max_block: Samples(64 << (n % 3)),
        format: if n.is_multiple_of(5) {
            SampleFormat::Float64
        } else {
            SampleFormat::Float32
        },
        mode: if n.is_multiple_of(7) {
            RenderMode::Offline
        } else {
            RenderMode::Realtime
        },
    }
}

fn err() -> PluginError {
    PluginError::Other("test".into())
}

/// One direct step, for the state tables.
fn go(state: State, event: Event) -> (State, Vec<Fx>) {
    let (state, out) = step(state, event);
    (state, out.iter().map(tag).collect())
}

fn reply(state: State, r: Reply) -> (State, Vec<Fx>) {
    go(state, Event::Replied(r))
}

fn stereo() -> Layout {
    Layout {
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
        ..Layout::default()
    }
}

/// An active plugin that has answered its first reads, with the Processor
/// lent, reached by the steps a shell takes.
fn active(rules: RestartRules, p: Prepare) -> State {
    let s = State::new(rules);
    let (s, _) = go(s, Event::Op(HostOp::Activate(p)));
    let (s, _) = reply(s, Reply::Activated);
    let (s, _) = reply(s, Reply::Latency(Samples(0)));
    let (s, _) = reply(s, Reply::Tail(PluginTail::Unknown));
    let (s, _) = reply(s, Reply::Layout(stereo()));
    let (s, _) = reply(s, Reply::Done);
    assert_eq!(s.stage(), Stage::Active);
    s
}

fn refused(rules: RestartRules, p: Prepare) -> State {
    let (s, _) = go(State::new(rules), Event::Op(HostOp::Activate(p)));
    let (s, _) = reply(s, Reply::Refused(err()));
    assert!(matches!(s.stage(), Stage::Refused { .. }));
    s
}

fn req(s: State, r: Request) -> State {
    go(s, Event::Request(r)).0
}

const REST: RestartRules = RestartRules::NONE;
const RESTARTS: RestartRules = RestartRules::CLAP;

// -------------------------------------------------------------------- shell

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    ActivateOp,
    ReconfigureOp,
    DeactivateOp,
    Request,
    Tick,
    Activated,
    Reply,
    Returned,
    Died,
}

fn kind(event: &Event) -> Kind {
    match event {
        Event::Op(HostOp::Activate(_)) => Kind::ActivateOp,
        Event::Op(HostOp::Reconfigure(_)) => Kind::ReconfigureOp,
        Event::Op(HostOp::Deactivate) => Kind::DeactivateOp,
        Event::Request(_) => Kind::Request,
        Event::Tick => Kind::Tick,
        Event::Replied(Reply::Activated) => Kind::Activated,
        Event::Replied(_) => Kind::Reply,
        Event::Returned => Kind::Returned,
        Event::Died => Kind::Died,
    }
}

/// What one step did, and what the model of the plugin says about it.
#[derive(Debug)]
struct Rec {
    kind: Kind,
    before: Stage,
    after: Stage,
    fx: Vec<Fx>,
    /// The model had the Processor away when a rejoin was emitted.
    rejoin_while_out: bool,
    /// An activation was emitted for a plugin the model had split, or a
    /// deactivation for one it had not.
    transition_out_of_turn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Refuse {
    Never,
    Always,
    Choice,
}

/// Where the realtime thread is: gives the Processor back at once (a single
/// thread user) or only when the test calls `return_processor`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Rt {
    Local,
    Thread,
}

struct Shell {
    state: State,
    /// The model's Processor is on the main thread.
    hand: bool,
    /// A recall was answered `Lent` and the Processor has not come back.
    returning: bool,
    real_active: bool,
    real_latency: usize,
    refuse: Refuse,
    rt: Rt,
    choices: Vec<u8>,
    at: usize,
    during_activate: Vec<Request>,
    /// Host ops raised between an effect and its reply.
    inject: Vec<HostOp>,
    trace: Vec<Rec>,
}

impl Shell {
    fn new(rules: RestartRules, refuse: Refuse, rt: Rt, choices: Vec<u8>) -> Self {
        Self {
            state: State::new(rules),
            hand: true,
            returning: false,
            real_active: false,
            real_latency: 0,
            refuse,
            rt,
            choices,
            at: 0,
            during_activate: Vec::new(),
            inject: Vec::new(),
            trace: Vec::new(),
        }
    }

    /// A shell whose plugin always does as asked, on one thread.
    fn easy(rules: RestartRules) -> Self {
        Self::new(rules, Refuse::Never, Rt::Local, vec![1])
    }

    fn pick(&mut self, n: u8) -> u8 {
        if self.choices.is_empty() {
            return 1;
        }
        let c = self.choices[self.at % self.choices.len()];
        self.at += 1;
        c % n
    }

    fn feed(&mut self, event: Event, q: &mut VecDeque<Effect>, all: &mut Vec<Fx>) {
        let k = kind(&event);
        let before = self.state.stage();
        let (state, out) = step(std::mem::replace(&mut self.state, State::new(REST)), event);
        self.state = state;
        assert!(out.len() <= EFFECT_CAPACITY);
        let mut rec = Rec {
            kind: k,
            before,
            after: self.state.stage(),
            fx: Vec::new(),
            rejoin_while_out: false,
            transition_out_of_turn: false,
        };
        for effect in out {
            match effect {
                Effect::CallDeactivate | Effect::Rebuild if !self.hand => {
                    rec.rejoin_while_out = true;
                }
                Effect::CallActivate(_) if self.real_active => rec.transition_out_of_turn = true,
                Effect::CallDeactivate if !self.real_active => rec.transition_out_of_turn = true,
                _ => {}
            }
            rec.fx.push(tag(&effect));
            all.push(tag(&effect));
            q.push_back(effect);
        }
        self.trace.push(rec);
    }

    fn perform(&mut self, effect: &Effect, q: &mut VecDeque<Effect>, all: &mut Vec<Fx>) -> Reply {
        match effect {
            Effect::CallActivate(_) => {
                for r in std::mem::take(&mut self.during_activate) {
                    self.feed(Event::Request(r), q, all);
                }
                let refuse = match self.refuse {
                    Refuse::Never => false,
                    Refuse::Always => true,
                    Refuse::Choice => self.pick(3) == 0,
                };
                if refuse {
                    Reply::Refused(err())
                } else {
                    self.real_active = true;
                    Reply::Activated
                }
            }
            Effect::CallDeactivate => {
                if self.refuse == Refuse::Choice && self.pick(4) == 0 {
                    Reply::Stuck(err())
                } else {
                    self.real_active = false;
                    Reply::Deactivated
                }
            }
            Effect::Rebuild => {
                if self.refuse == Refuse::Always
                    || (self.refuse == Refuse::Choice && self.pick(3) == 0)
                {
                    Reply::Refused(err())
                } else {
                    Reply::Rebuilt
                }
            }
            Effect::Recall => {
                if self.hand {
                    Reply::Returned
                } else if self.rt == Rt::Local {
                    self.hand = true;
                    Reply::Returned
                } else {
                    self.returning = true;
                    Reply::Lent
                }
            }
            Effect::Lend => {
                self.hand = false;
                Reply::Done
            }
            Effect::ReadLatency if self.real_active => Reply::Latency(Samples(self.real_latency)),
            Effect::ReadTail if self.real_active => Reply::Tail(PluginTail::Unknown),
            Effect::ReadLayout if self.real_active => Reply::Layout(stereo()),
            Effect::ReadLatency | Effect::ReadTail | Effect::ReadLayout => Reply::NotActive,
            _ => Reply::Done,
        }
    }

    /// Steps `event`, then performs every effect it leads to. Returns them all.
    fn send(&mut self, event: Event) -> Vec<Fx> {
        let (mut q, mut all) = (VecDeque::new(), Vec::new());
        self.feed(event, &mut q, &mut all);
        while let Some(effect) = q.pop_front() {
            while !self.inject.is_empty() && self.pick(2) == 0 {
                let op = self.inject.remove(0);
                self.feed(Event::Op(op), &mut q, &mut all);
            }
            let r = self.perform(&effect, &mut q, &mut all);
            self.feed(Event::Replied(r), &mut q, &mut all);
        }
        all
    }

    fn op(&mut self, op: HostOp) -> Vec<Fx> {
        self.send(Event::Op(op))
    }

    /// The realtime thread hands the Processor back.
    fn return_processor(&mut self) -> Vec<Fx> {
        assert!(self.returning);
        self.hand = true;
        self.returning = false;
        self.send(Event::Returned)
    }
}

// ------------------------------------------------------------- strategies

#[derive(Debug, Clone)]
enum Ev {
    Activate(u8),
    Reconfigure(u8),
    Deactivate,
    Req(Request),
    Tick,
    Return,
    Died,
}

fn scope() -> impl Strategy<Value = RescanScope> {
    prop_oneof![
        Just(RescanScope::Values),
        Just(RescanScope::Text),
        Just(RescanScope::Info),
        Just(RescanScope::All),
    ]
}

fn request() -> impl Strategy<Value = Request> {
    prop_oneof![
        Just(Request::Restart),
        Just(Request::IoChanged),
        scope().prop_map(Request::RescanParams),
        Just(Request::LatencyChanged),
        Just(Request::TailChanged),
        Just(Request::Reload),
        Just(Request::Callback),
        Just(Request::MidiCcAssignmentChanged),
        Just(Request::NoteExpressionChanged),
        (0u32..6).prop_map(|i| Request::Timer(TimerId(i))),
        (0i32..6).prop_map(|i| Request::Fd(FdId(i))),
    ]
}

fn ev() -> impl Strategy<Value = Ev> {
    prop_oneof![
        2 => any::<u8>().prop_map(Ev::Activate),
        2 => any::<u8>().prop_map(Ev::Reconfigure),
        2 => Just(Ev::Deactivate),
        6 => request().prop_map(Ev::Req),
        6 => Just(Ev::Tick),
        4 => Just(Ev::Return),
        1 => Just(Ev::Died),
    ]
}

fn rules() -> impl Strategy<Value = RestartRules> {
    (any::<bool>(), any::<bool>()).prop_map(|(io, latency)| RestartRules { io, latency })
}

/// A strategy for a value that is not `Clone`.
fn lazy<T: std::fmt::Debug>(make: fn() -> T) -> impl Strategy<Value = T> {
    Just(()).prop_map(move |()| make())
}

fn any_reply() -> impl Strategy<Value = Reply> {
    prop_oneof![
        lazy(|| Reply::Activated),
        lazy(|| Reply::Refused(err())),
        lazy(|| Reply::Deactivated),
        lazy(|| Reply::Rebuilt),
        (0usize..3).prop_map(|n| Reply::Latency(Samples(n))),
        lazy(|| Reply::Tail(PluginTail::Unknown)),
        lazy(|| Reply::Tail(PluginTail::None)),
        lazy(|| Reply::Layout(stereo())),
        lazy(|| Reply::Layout(Layout::default())),
        lazy(|| Reply::NotActive),
        lazy(|| Reply::Returned),
        lazy(|| Reply::Lent),
        lazy(|| Reply::Stuck(err())),
        lazy(|| Reply::Done),
    ]
}

/// Any event at all, whether or not an effect is waiting for it.
fn any_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        any::<u8>().prop_map(|n| Event::Op(HostOp::Activate(prep(n)))),
        any::<u8>().prop_map(|n| Event::Op(HostOp::Reconfigure(prep(n)))),
        lazy(|| Event::Op(HostOp::Deactivate)),
        request().prop_map(Event::Request),
        lazy(|| Event::Tick),
        any_reply().prop_map(Event::Replied),
        lazy(|| Event::Returned),
        lazy(|| Event::Died),
    ]
}

fn shell_run(shell: &mut Shell, events: &[Ev]) {
    for e in events {
        match e {
            Ev::Activate(n) => drop(shell.op(HostOp::Activate(prep(*n)))),
            Ev::Reconfigure(n) => drop(shell.op(HostOp::Reconfigure(prep(*n)))),
            Ev::Deactivate => drop(shell.op(HostOp::Deactivate)),
            Ev::Req(r) => drop(shell.send(Event::Request(*r))),
            Ev::Tick => drop(shell.send(Event::Tick)),
            Ev::Return => {
                if shell.returning {
                    shell.return_processor();
                }
            }
            Ev::Died => drop(shell.send(Event::Died)),
        }
    }
}

fn count(fx: &[Fx], f: impl Fn(&Fx) -> bool) -> usize {
    fx.iter().filter(|x| f(x)).count()
}

// ---------------------------------------------------------------- properties

proptest! {
    /// Every `Replied(Activated)` is answered, in the same step, by the three
    /// reads and then one `Lend`, whatever preceded it and whatever the format.
    ///
    /// Mutation: swap `Effect::Lend` ahead of `Effect::ReadLatency` in the
    /// `Reply::Activated` arm, or drop `Effect::ReadTail` from it; this fails.
    #[test]
    fn an_activation_is_answered_by_the_reads_then_one_lend(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..48),
        local in any::<bool>(),
    ) {
        let rt = if local { Rt::Local } else { Rt::Thread };
        let mut shell = Shell::new(rules, Refuse::Choice, rt, choices);
        shell_run(&mut shell, &events);
        for rec in shell.trace.iter().filter(|r| r.kind == Kind::Activated) {
            prop_assert_eq!(&rec.fx[..4], &[Fx::Latency, Fx::Tail, Fx::Layout, Fx::Lend]);
            prop_assert_eq!(count(&rec.fx, |f| *f == Fx::Lend), 1);
        }
    }

    /// The plugin renders only in `Active`: a `Lend` is emitted only into it,
    /// and `Refused` is left only by emitting an activation (or by dying).
    ///
    /// Mutation: let `HostOp::Deactivate` take a `Refused` stage to `Loaded`
    /// (match `Stage::Active | Stage::Refused { .. }` in its idle arm and set
    /// the stage); the second assertion fails.
    #[test]
    fn only_an_active_plugin_renders(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..48),
        local in any::<bool>(),
    ) {
        let rt = if local { Rt::Local } else { Rt::Thread };
        let mut shell = Shell::new(rules, Refuse::Choice, rt, choices);
        shell_run(&mut shell, &events);
        for rec in &shell.trace {
            if rec.fx.contains(&Fx::Lend) {
                prop_assert_eq!(rec.after, Stage::Active);
            }
            if matches!(rec.before, Stage::Refused { .. })
                && !matches!(rec.after, Stage::Refused { .. })
            {
                let activates = rec.fx.iter().any(|f| matches!(f, Fx::Activate(_)));
                prop_assert!(activates || rec.kind == Kind::Died);
            }
        }
    }

    /// From `Refused`, nothing but an activation reaches an effect that
    /// renders, whatever the events (replies included).
    ///
    /// Mutation: make `Reply::Activated` ignore `wait == Wait::Activate`
    /// (`Reply::Activated =>`); a stray reply then revives the plugin and
    /// this fails.
    #[test]
    fn a_refused_plugin_stays_silent_until_asked_to_activate(
        rules in rules(),
        events in prop::collection::vec(any_event(), 0..32),
    ) {
        let mut s = refused(rules, prep(0));
        for e in events {
            if matches!(e, Event::Op(HostOp::Activate(_) | HostOp::Reconfigure(_))) {
                continue;
            }
            let (next, fx) = go(s, e);
            s = next;
            prop_assert!(!fx.contains(&Fx::Lend));
            let silent = matches!(s.stage(), Stage::Refused { .. } | Stage::Poisoned);
            prop_assert!(silent);
            prop_assert!(!s.renders());
        }
    }

    /// `Poisoned` absorbs: nothing after `Died` emits an effect or changes it.
    ///
    /// Mutation: let `Event::Op` through before the `Poisoned` guard in
    /// `on_event` (move the guard below the match); this fails.
    #[test]
    fn a_poisoned_plugin_absorbs_every_event(
        rules in rules(),
        before in prop::collection::vec(ev(), 0..24),
        after in prop::collection::vec(any_event(), 1..24),
    ) {
        let mut shell = Shell::new(rules, Refuse::Choice, Rt::Thread, vec![1, 0, 2]);
        shell_run(&mut shell, &before);
        let (mut s, _) = go(std::mem::replace(&mut shell.state, State::new(REST)), Event::Died);
        prop_assert_eq!(s.stage(), Stage::Poisoned);
        prop_assert!(!s.is_busy());
        for e in after {
            let (next, fx) = go(s, e);
            s = next;
            prop_assert!(fx.is_empty());
            prop_assert_eq!(s.stage(), Stage::Poisoned);
        }
    }

    /// A latency or tail change raised while the plugin activates is answered
    /// by the reads after it: the next tick restarts nothing.
    ///
    /// Mutation: make the `LatencyChanged | TailChanged` arm of `on_request`
    /// `active || activating`; the bit is then kept and the tick restarts.
    #[test]
    fn a_change_raised_inside_an_activation_does_not_restart_it(
        io in any::<bool>(),
        inside in prop::collection::vec(
            prop_oneof![Just(Request::LatencyChanged), Just(Request::TailChanged)], 1..6),
        n in any::<u8>(),
    ) {
        let rules = RestartRules { io, latency: true };
        let mut shell = Shell::easy(rules);
        shell.op(HostOp::Activate(prep(n)));
        shell.during_activate = inside;
        let fx = shell.op(HostOp::Reconfigure(prep(n.wrapping_add(1))));
        prop_assert!(fx.contains(&Fx::Lend));
        let tick = shell.send(Event::Tick);
        prop_assert_eq!(count(&tick, |f| matches!(f, Fx::Deactivate | Fx::Activate(_))), 0);
    }

    /// A quiet state's tick plans nothing, after any sequence of events: what
    /// lets a shell skip the tick of an idle pass.
    ///
    /// Mutation: `State::is_quiet` ignoring `pending` (or answering `true`) →
    /// a tick after a request plans a restart or a notice → fails.
    #[test]
    fn a_quiet_state_ticks_to_nothing(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..48),
    ) {
        let mut shell = Shell::new(rules, Refuse::Choice, Rt::Local, choices);
        for event in &events {
            shell_run(&mut shell, std::slice::from_ref(event));
            if shell.state.is_quiet() {
                let (_, fx) = go(shell.state.clone(), Event::Tick);
                prop_assert!(fx.is_empty(), "{fx:?}");
            }
        }
    }

    /// Activations over a sequence are at most one per activating op plus one
    /// per tick that planned a restart.
    ///
    /// Mutation: in `plan_restart`, push `Task::Activate(prepare)` twice; this
    /// fails once a plan runs.
    #[test]
    fn activations_are_bounded_by_ops_and_planned_restarts(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..48),
    ) {
        let mut shell = Shell::new(rules, Refuse::Choice, Rt::Local, choices);
        shell_run(&mut shell, &events);
        let activations: usize = shell.trace.iter()
            .map(|r| count(&r.fx, |f| matches!(f, Fx::Activate(_)))).sum();
        let ops = shell.trace.iter()
            .filter(|r| matches!(r.kind, Kind::ActivateOp | Kind::ReconfigureOp)).count();
        let planned = shell.trace.iter()
            .filter(|r| matches!(r.kind, Kind::Tick | Kind::Reply) && r.fx.contains(&Fx::Recall))
            .count();
        prop_assert!(activations <= ops + planned);
    }

    /// A reload subsumes every other request: one rebuild, one rejoin, one
    /// activation, one notice, however many finer requests came with it.
    ///
    /// Mutation: in `plan_restart` do not push `Task::Rebuild` and the rebuild
    /// count fails; stop calling `clear_requests` there and the last tick
    /// repeats the parameter notices.
    #[test]
    fn a_reload_subsumes_the_finer_requests(
        rules in rules(),
        others in prop::collection::vec(request(), 0..8),
        n in any::<u8>(),
    ) {
        let mut shell = Shell::easy(rules);
        shell.op(HostOp::Activate(prep(n)));
        shell.send(Event::Request(Request::Reload));
        for r in others {
            shell.send(Event::Request(r));
        }
        let fx = shell.send(Event::Tick);
        prop_assert_eq!(count(&fx, |f| *f == Fx::Rebuild), 1);
        prop_assert_eq!(count(&fx, |f| *f == Fx::Deactivate), 1);
        prop_assert_eq!(count(&fx, |f| matches!(f, Fx::Activate(_))), 1);
        prop_assert_eq!(count(&fx, |f| *f == Fx::Reloaded), 1);
        while shell.state.has_backlog() {
            shell.send(Event::Tick);
        }
        prop_assert!(shell.send(Event::Tick).is_empty());
    }

    /// A rejoin never meets a Processor that is away: `CallDeactivate` and
    /// `Rebuild` come only after a `Recall` was answered `Returned` or
    /// followed by `Event::Returned`.
    ///
    /// Mutation: in the `Reply::Lent` arm continue the plan (`self.advance(out)`
    /// instead of parking) or emit `CallDeactivate` straight after `Recall` in
    /// `begin`; this fails.
    #[test]
    fn a_rejoin_never_meets_a_processor_that_is_away(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..64),
    ) {
        let mut shell = Shell::new(rules, Refuse::Choice, Rt::Thread, choices);
        shell_run(&mut shell, &events);
        for rec in &shell.trace {
            prop_assert!(!rec.rejoin_while_out);
            prop_assert!(!rec.transition_out_of_turn);
        }
    }

    /// A restart that ends in `Refused` notifies the refusal once and never a
    /// latency change, and renders nothing after.
    ///
    /// Mutation: in the `Reply::Refused` arm also emit `Effect::ReadLatency`
    /// (a read against a stopped plugin); this fails.
    #[test]
    fn a_restart_that_ends_refused_says_so_once(
        rules in rules(),
        extra in prop::collection::vec(request(), 0..6),
        n in any::<u8>(),
    ) {
        let mut shell = Shell::easy(rules);
        shell.op(HostOp::Activate(prep(n)));
        shell.refuse = Refuse::Always;
        shell.real_latency = 99;
        shell.send(Event::Request(Request::Restart));
        shell.send(Event::Request(Request::LatencyChanged));
        for r in extra {
            if !matches!(r, Request::Callback) {
                shell.send(Event::Request(r));
            }
        }
        let fx = shell.send(Event::Tick);
        prop_assert_eq!(count(&fx, |f| *f == Fx::RefusedNotice), 1);
        prop_assert_eq!(count(&fx, |f| matches!(f, Fx::LatencyNotice(_))), 0);
        prop_assert_eq!(count(&fx, |f| *f == Fx::Lend), 0);
        prop_assert_eq!(count(&fx, |f| matches!(f, Fx::Latency | Fx::Tail | Fx::Layout)), 0);
        let stage = shell.state.stage();
        let refused = matches!(stage, Stage::Refused { .. });
        prop_assert!(refused);
    }

    /// `step` is total and bounded: no event in any state panics or emits more
    /// than `EFFECT_CAPACITY` effects.
    ///
    /// Mutation: drop the `.min(TIMERS_PER_TICK)` and `.min(FDS_PER_TICK)`
    /// caps in `tick`; a tick that runs every pending one exceeds the bound
    /// and fails (`emit` asserts it).
    #[test]
    fn step_is_total_and_bounded(
        rules in rules(),
        n in any::<u8>(),
        timers in prop::collection::vec(0u32..40, 0..40),
        fds in prop::collection::vec(0i32..40, 0..40),
        events in prop::collection::vec(any_event(), 0..64),
    ) {
        let mut s = active(rules, prep(n));
        for t in timers {
            s = req(s, Request::Timer(TimerId(t)));
        }
        for f in fds {
            s = req(s, Request::Fd(FdId(f)));
        }
        for e in std::iter::once(Event::Tick).chain(events) {
            let (next, out) = step(s, e);
            s = next;
            prop_assert!(out.len() <= EFFECT_CAPACITY);
        }
    }

    /// A parked plan runs once, with the latest `Prepare`: `Returned` emits
    /// the rest of it, a second `Returned` emits nothing.
    ///
    /// Mutation: in `set_activate` push a second `Task::Activate` instead of
    /// replacing; the activation count fails. (The second `Returned` is pinned
    /// by the rule 9 table test.)
    #[test]
    fn a_parked_plan_runs_once_with_the_last_prepare(
        rules in rules(),
        ns in prop::collection::vec(any::<u8>(), 1..6),
    ) {
        let mut shell = Shell::new(rules, Refuse::Never, Rt::Thread, vec![1]);
        shell.op(HostOp::Activate(prep(0)));
        for n in &ns {
            shell.op(HostOp::Reconfigure(prep(*n)));
        }
        prop_assert!(shell.state.is_parked());
        let fx = shell.return_processor();
        prop_assert_eq!(count(&fx, |f| *f == Fx::Deactivate), 1);
        let acts: Vec<Fx> = fx.iter().copied().filter(|f| matches!(f, Fx::Activate(_))).collect();
        prop_assert_eq!(acts, vec![Fx::Activate(prep(*ns.last().unwrap()))]);
        let (_, none) = go(shell.state.clone(), Event::Returned);
        prop_assert!(none.is_empty());
    }
}

// -------------------------------------------------------------- state tables

// Mutation: drop the `!matches!(self.stage, Stage::Active)` test from
// `HostOp::Activate` (an active plugin is then activated again), or make
// `Reconfigure` on an active plugin plan only `Task::Activate`; fails.
#[test]
fn rule_1_activate_and_reconfigure() {
    let p = prep(1);
    let q = prep(2);
    // Loaded and Refused: Activate and Reconfigure both activate.
    for op in [HostOp::Activate(p), HostOp::Reconfigure(p)] {
        let (_, fx) = go(State::new(REST), Event::Op(op));
        assert_eq!(fx, [Fx::Activate(p)]);
        let (_, fx) = go(refused(REST, q), Event::Op(op));
        assert_eq!(fx, [Fx::Activate(p)]);
    }
    // Active: Activate does nothing, Reconfigure recalls first and then
    // deactivates and activates as the replies come.
    let s = active(REST, p);
    let (s, fx) = go(s, Event::Op(HostOp::Activate(q)));
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(q)));
    assert_eq!(fx, [Fx::Recall]);
    let (s, fx) = reply(s, Reply::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(q)]);
    assert_eq!(s.prepare(), Some(q));
    // Poisoned: nothing.
    let (dead, _) = go(State::new(REST), Event::Died);
    for op in [
        HostOp::Activate(p),
        HostOp::Reconfigure(p),
        HostOp::Deactivate,
    ] {
        assert!(go(dead.clone(), Event::Op(op)).1.is_empty());
    }
    // Deactivate on an active plugin recalls, rejoins and stays loaded.
    let (s, fx) = go(active(REST, p), Event::Op(HostOp::Deactivate));
    assert_eq!(fx, [Fx::Recall]);
    let (s, fx) = reply(s, Reply::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert!(fx.is_empty());
    assert_eq!(s.stage(), Stage::Loaded);
    // On a loaded or refused one it does nothing.
    assert!(
        go(State::new(REST), Event::Op(HostOp::Deactivate))
            .1
            .is_empty()
    );
    let (s, fx) = go(refused(REST, p), Event::Op(HostOp::Deactivate));
    assert!(fx.is_empty());
    assert!(matches!(s.stage(), Stage::Refused { .. }));
}

// Mutation: delete `p.latency = false;` and `p.tail = false;` from
// `settle_requests`; the bits raised before the deactivation survive it and
// the tick below restarts.
#[test]
fn rule_2_a_change_raised_before_the_deactivation_is_answered_by_the_reads() {
    let p = prep(1);
    let s = active(RESTARTS, p);
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(p)));
    let (s, _) = reply(s, Reply::Returned);
    let s = req(s, Request::LatencyChanged);
    let s = req(s, Request::TailChanged);
    let (s, _) = reply(s, Reply::Deactivated);
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(fx, [Fx::Latency, Fx::Tail, Fx::Layout, Fx::Lend]);
    assert!(go(s, Event::Tick).1.is_empty());
}

// Mutation: in the `Refused` arm, push `Effect::ReadTail` after the notice,
// or delete `self.stage = Stage::Refused { rate };`; fails.
#[test]
fn rule_3_a_refusal_reads_nothing_and_a_later_activation_retries() {
    let p = prep(3);
    let (s, _) = go(State::new(REST), Event::Op(HostOp::Activate(p)));
    let (s, fx) = reply(s, Reply::Refused(err()));
    assert_eq!(fx, [Fx::RefusedNotice]);
    assert_eq!(s.stage(), Stage::Refused { rate: p.rate });
    assert!(!s.renders());
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(prep(4))));
    assert_eq!(fx, [Fx::Activate(prep(4))]);
    let (s, _) = reply(s, Reply::Activated);
    assert!(s.renders());
}

// Mutation: make the `Latency` arm notify when `before.is_none()` too (the
// first read then notifies); drop `|| announced` and the announced case fails;
// drop `b != now` and the changed case fails. The same for `Tail`.
#[test]
fn rule_4_a_figure_is_notified_when_it_moved_or_was_announced() {
    // The first read after load only records the figure.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(prep(1))));
    let (s, _) = reply(s, Reply::Activated);
    let (s, fx) = reply(s, Reply::Latency(Samples(7)));
    assert!(fx.is_empty());
    assert_eq!(s.latency(), Some(Samples(7)));
    let (s, fx) = reply(s, Reply::Tail(PluginTail::Unbounded));
    assert!(fx.is_empty());
    assert_eq!(s.tail(), Some(PluginTail::Unbounded));
    let s = active(RESTARTS, prep(1));
    // Same figure, not announced: silent.
    let (s, fx) = reply(s, Reply::Latency(Samples(0)));
    assert!(fx.is_empty());
    // Moved: notified.
    let (_, fx) = reply(s, Reply::Latency(Samples(32)));
    assert_eq!(fx, [Fx::LatencyNotice(32)]);
    // Announced by a re-read format, same figure: notified.
    let s = active(REST, prep(1));
    let s = req(s, Request::LatencyChanged);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Latency]);
    let (_, fx) = reply(s, Reply::Latency(Samples(0)));
    assert_eq!(fx, [Fx::LatencyNotice(0)]);
    // Tail likewise.
    let s = active(REST, prep(1));
    let (s, fx) = reply(s, Reply::Tail(PluginTail::Unknown));
    assert!(fx.is_empty());
    let (s, fx) = reply(s, Reply::Tail(PluginTail::None));
    assert_eq!(fx, [Fx::TailNotice]);
    let s = req(s, Request::TailChanged);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Tail]);
    let (_, fx) = reply(s, Reply::Tail(PluginTail::None));
    assert_eq!(fx, [Fx::TailNotice]);
}

// Mutation: drop `revived ||` from the `Some(before)` arm; the revival is
// then silent and this fails.
#[test]
fn rule_4_a_layout_is_notified_when_it_differs_or_the_stage_changed() {
    let s = active(REST, prep(1));
    let (s, fx) = reply(s, Reply::Layout(stereo()));
    assert!(fx.is_empty());
    let (_, fx) = reply(s, Reply::Layout(Layout::default()));
    assert_eq!(fx, [Fx::IoNotice]);
    // A plugin revived from a refusal: the same layout still tells the host.
    let mut shell = Shell::easy(REST);
    shell.op(HostOp::Activate(prep(1)));
    shell.refuse = Refuse::Always;
    shell.op(HostOp::Reconfigure(prep(2)));
    assert!(matches!(shell.state.stage(), Stage::Refused { .. }));
    shell.refuse = Refuse::Never;
    let fx = shell.op(HostOp::Activate(prep(2)));
    assert!(fx.contains(&Fx::IoNotice));
}

// Mutation: make `Request::Restart` set no bit in `on_request`, or treat
// `p.io && rules.io` as `p.io` (a re-reading format then restarts); fails.
#[test]
fn rule_5_requests_set_bits_and_a_tick_plans() {
    let p = prep(1);
    for (request, restarts) in [
        (Request::Restart, true),
        (Request::Reload, true),
        (Request::RescanParams(RescanScope::All), true),
        (Request::IoChanged, true),
        (Request::LatencyChanged, true),
    ] {
        let s = req(active(RESTARTS, p), request);
        let (_, fx) = go(s, Event::Tick);
        assert_eq!(fx.first() == Some(&Fx::Recall), restarts, "{request:?}");
    }
    // A format that re-reads: the same requests restart nothing.
    let s = req(active(REST, p), Request::IoChanged);
    assert_eq!(go(s, Event::Tick).1, [Fx::Layout]);
    let s = req(active(REST, p), Request::LatencyChanged);
    assert_eq!(go(s, Event::Tick).1, [Fx::Latency]);
    let s = req(active(REST, p), Request::TailChanged);
    assert_eq!(go(s, Event::Tick).1, [Fx::Tail]);
    // Several collapse into one recall.
    let mut s = active(RESTARTS, p);
    for r in [
        Request::Restart,
        Request::IoChanged,
        Request::LatencyChanged,
    ] {
        s = req(s, r);
    }
    assert_eq!(go(s, Event::Tick).1, [Fx::Recall]);
}

// Mutation: skip the callback branch in `tick` (`if false && callback ..`),
// or delete `self.after_main = true;`; fails.
#[test]
fn rule_5_a_callback_runs_first_because_it_may_ask_for_the_rest() {
    let s = req(active(RESTARTS, prep(1)), Request::Callback);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Main]);
    // The callback asks for a restart; the plan follows its reply.
    let s = req(s, Request::Restart);
    let (_, fx) = reply(s, Reply::Done);
    assert_eq!(fx, [Fx::Recall]);
    // A callback that asks for another callback does not loop.
    let s = req(active(RESTARTS, prep(1)), Request::Callback);
    let (s, _) = go(s, Event::Tick);
    let s = req(s, Request::Callback);
    let (_, fx) = reply(s, Reply::Done);
    assert!(fx.is_empty());
}

// Mutation: make a `Values` or an `Info` rescan set `p.all` in `on_request`
// (a restart); fails.
#[test]
fn rule_5_a_rescan_restarts_only_when_it_must() {
    let p = prep(1);
    for scope in [RescanScope::Values, RescanScope::Text] {
        let s = req(active(RESTARTS, p), Request::RescanParams(scope));
        assert_eq!(go(s, Event::Tick).1, [Fx::ParamValues]);
    }
    let s = req(
        active(RESTARTS, p),
        Request::RescanParams(RescanScope::Info),
    );
    assert_eq!(
        go(s, Event::Tick).1,
        [Fx::Params(RescanScope::Info), Fx::ParamTitles]
    );
    // `All` restarts and reads after the activation.
    let s = req(active(RESTARTS, p), Request::RescanParams(RescanScope::All));
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Recall]);
    let (s, _) = reply(s, Reply::Returned);
    let (s, _) = reply(s, Reply::Deactivated);
    let (_, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx,
        [
            Fx::Latency,
            Fx::Tail,
            Fx::Layout,
            Fx::Lend,
            Fx::Params(RescanScope::All),
            Fx::ParamTitles,
            Fx::ParamValues
        ]
    );
}

// Mutation: make the `LatencyChanged | TailChanged` arm of `on_request`
// `active || activating`; the first tick below restarts. Make `IoChanged`
// ignored while activating and the last one does not.
#[test]
fn rule_6_a_request_during_an_activation_is_kept_except_latency_and_tail() {
    let p = prep(1);
    let s = active(RESTARTS, p);
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(p)));
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(p)]);
    let s = req(s, Request::LatencyChanged);
    let s = req(s, Request::TailChanged);
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
    // Anything else raised inside the activation is acted on after it.
    let (s, _) = go(active(RESTARTS, p), Event::Op(HostOp::Reconfigure(p)));
    let (s, _) = reply(s, Reply::Returned);
    let (s, _) = reply(s, Reply::Deactivated);
    let s = req(s, Request::IoChanged);
    let (s, _) = reply(s, Reply::Activated);
    assert_eq!(go(s, Event::Tick).1, [Fx::Recall]);
}

// Mutation: delete the `if !accepted { return; }` guard in `on_request`; the
// bits survive into the activation that follows and the last tick restarts.
#[test]
fn rule_7_a_request_on_a_plugin_that_is_not_running_changes_nothing() {
    let all = [
        Request::Restart,
        Request::IoChanged,
        Request::LatencyChanged,
        Request::TailChanged,
        Request::Reload,
        Request::RescanParams(RescanScope::All),
        Request::MidiCcAssignmentChanged,
        Request::NoteExpressionChanged,
    ];
    let (poisoned, _) = go(State::new(REST), Event::Died);
    for (s, name) in [
        (State::new(RESTARTS), "loaded"),
        (refused(RESTARTS, prep(1)), "refused"),
        (poisoned, "poisoned"),
    ] {
        let mut s = s;
        for r in all {
            s = req(s, r);
        }
        let (s, fx) = go(s, Event::Tick);
        assert!(fx.is_empty(), "{name}");
        if name != "poisoned" {
            // Nothing was kept for the activation that follows.
            let (s, _) = go(s, Event::Op(HostOp::Activate(prep(1))));
            let (s, _) = reply(s, Reply::Activated);
            assert!(go(s, Event::Tick).1.is_empty(), "{name} then active");
        }
    }
    // A loaded plugin still runs its callback, timers and descriptors.
    let mut s = State::new(RESTARTS);
    s = req(s, Request::Timer(TimerId(4)));
    s = req(s, Request::Fd(FdId(5)));
    s = req(s, Request::Callback);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Main]);
    let (_, fx) = reply(s, Reply::Done);
    assert_eq!(fx, [Fx::Timer(4), Fx::Fd(5)]);
}

// Mutation: leave the plan in place on `Event::Died` (delete
// `self.plan = Plan::default();` from `poison`); fails.
#[test]
fn rule_8_dying_drops_a_parked_plan() {
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(prep(2))));
    let (s, _) = reply(s, Reply::Lent);
    assert!(s.is_parked());
    let (s, fx) = go(s, Event::Died);
    assert!(fx.is_empty());
    assert!(!s.is_busy());
    assert_eq!(s.stage(), Stage::Poisoned);
    assert!(go(s, Event::Returned).1.is_empty());
}

// Mutation: drop the `Wait::Parked` guard on `Event::Returned` (the second
// `Returned` then emits the activation early).
#[test]
fn rule_9_a_plan_parks_behind_a_lent_processor_and_runs_once() {
    let (p, q, r) = (prep(1), prep(2), prep(3));
    let s = active(RESTARTS, p);
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(q)));
    assert_eq!(fx, [Fx::Recall]);
    let (s, fx) = reply(s, Reply::Lent);
    assert!(fx.is_empty());
    assert!(s.is_parked());
    // A second reconfigure rewrites the parked prepare.
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(r)));
    assert!(fx.is_empty());
    // A request only sets its bit, and a tick runs only what needs no restart.
    let s = req(s, Request::Restart);
    let s = req(s, Request::Timer(TimerId(1)));
    let s = req(s, Request::TailChanged);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Timer(1), Fx::Tail]);
    // The Processor is back: the tail runs.
    let (s, fx) = go(s, Event::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    // A second `Returned` before the reply emits nothing.
    let (s, fx) = go(s, Event::Returned);
    assert!(fx.is_empty());
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(r)]);
    // The restart raised meanwhile is answered by the plan itself.
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
}

// Mutation: make `HostOp::Deactivate` on a parked plan push to the tasks
// instead of replacing them; fails (an activation follows).
#[test]
fn rule_9_a_deactivate_replaces_a_parked_plan() {
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(prep(2))));
    let (s, _) = reply(s, Reply::Lent);
    let (s, fx) = go(s, Event::Op(HostOp::Deactivate));
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert!(fx.is_empty());
    assert_eq!(s.stage(), Stage::Loaded);
}

// Mutation: leave the plan in place on `Stuck` (drop `self.plan =
// Plan::default();`) or forget the `Lend`; fails.
#[test]
fn rule_10_a_plugin_that_will_not_stop_stays_active_and_is_lent_again() {
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(prep(2))));
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Stuck(err()));
    assert_eq!(fx, [Fx::StuckNotice, Fx::Lend]);
    assert_eq!(s.stage(), Stage::Active);
    assert!(!s.is_busy());
    assert!(go(s, Event::Tick).1.is_empty());
}

// Mutation: ignore `wait` in the `Deactivated` arm; a stray reply then moves
// an active plugin to `Loaded`.
#[test]
fn a_reply_that_answers_nothing_is_ignored() {
    let s = active(RESTARTS, prep(1));
    for r in [
        Reply::Deactivated,
        Reply::Rebuilt,
        Reply::Activated,
        Reply::Refused(err()),
        Reply::Stuck(err()),
        Reply::Returned,
        Reply::Lent,
    ] {
        let (next, fx) = reply(s.clone(), r);
        assert!(fx.is_empty());
        assert_eq!(next.stage(), Stage::Active);
        assert!(!next.is_busy());
    }
}

// Mutation: delete `p.restart = false;` from `settle_requests`; the tick
// after the re-activation recalls again.
#[test]
fn a_request_raised_while_a_plan_waits_is_answered_by_the_plan() {
    let q = prep(2);
    // Parks behind a lent Processor, then takes `request` while it waits.
    let park = |request| {
        let s = active(RESTARTS, prep(1));
        let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
        let (s, _) = reply(s, Reply::Lent);
        let s = req(s, request);
        let (s, fx) = go(s, Event::Returned);
        assert_eq!(fx, [Fx::Deactivate]);
        s
    };
    let finish = |s: State| {
        let (s, _) = reply(s, Reply::Activated);
        assert!(go(s, Event::Tick).1.is_empty());
    };
    for request in [
        Request::Restart,
        Request::IoChanged,
        Request::LatencyChanged,
    ] {
        let (s, fx) = reply(park(request), Reply::Deactivated);
        assert_eq!(fx, [Fx::Activate(q)], "{request:?}");
        finish(s);
    }
    // A rescan of everything re-reads the parameters after the activation.
    let (s, fx) = reply(
        park(Request::RescanParams(RescanScope::All)),
        Reply::Deactivated,
    );
    assert_eq!(fx, [Fx::Activate(q)]);
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx[4..],
        [
            Fx::Params(RescanScope::All),
            Fx::ParamTitles,
            Fx::ParamValues
        ]
    );
    assert!(go(s, Event::Tick).1.is_empty());
    // A reload is not lost: the rebuild joins the plan.
    let (s, fx) = reply(park(Request::Reload), Reply::Deactivated);
    assert_eq!(fx, [Fx::Rebuild]);
    let (s, fx) = reply(s, Reply::Rebuilt);
    assert_eq!(fx, [Fx::Reloaded, Fx::Activate(q)]);
    finish(s);
}

// Mutation: in `on_op` make `HostOp::Activate` on a busy plan do nothing
// (`_ => {}`); the activation is lost.
#[test]
fn an_activate_raised_while_a_plan_waits_is_the_latest_intent() {
    let (p, q) = (prep(1), prep(2));
    let s = active(RESTARTS, p);
    let (s, _) = go(s, Event::Op(HostOp::Deactivate));
    let (s, _) = reply(s, Reply::Lent);
    let (s, fx) = go(s, Event::Op(HostOp::Activate(q)));
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(q)]);
    assert_eq!(s.prepare(), Some(q));
}

// Mutation: delete the `self.replay(out);` call from the `Activated` arm, or
// store only the first op raised (`self.later.get_or_insert(op)`); fails.
#[test]
fn an_op_raised_during_an_activation_runs_when_it_ends() {
    let (p, q) = (prep(1), prep(2));
    // The latest of several wins.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(q)));
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Op(HostOp::Deactivate));
    assert!(fx.is_empty());
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx,
        [Fx::Latency, Fx::Tail, Fx::Layout, Fx::Lend, Fx::Recall]
    );
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert!(fx.is_empty());
    assert_eq!(s.stage(), Stage::Loaded);
    // A reconfigure runs the same way and lands on its prepare.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(fx.last(), Some(&Fx::Recall));
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(q)]);
    assert_eq!(s.prepare(), Some(q));
    // An activate raised during a refused activation retries it.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, _) = go(s, Event::Op(HostOp::Activate(q)));
    let (_, fx) = reply(s, Reply::Refused(err()));
    assert_eq!(fx, [Fx::RefusedNotice, Fx::Activate(q)]);
}

// Mutation: delete the `.min(TIMERS_PER_TICK)` and `.min(FDS_PER_TICK)` caps
// (everything runs at once), or make `has_backlog` always false; fails.
#[test]
fn timers_and_descriptors_over_the_cap_wait_for_the_next_tick() {
    let mut s = State::new(REST);
    for i in 0..5 {
        s = req(s, Request::Timer(TimerId(i)));
    }
    for i in 0..4 {
        s = req(s, Request::Fd(FdId(i)));
    }
    // A repeated id is one entry.
    s = req(s, Request::Timer(TimerId(4)));
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(
        fx,
        [
            Fx::Timer(0),
            Fx::Timer(1),
            Fx::Timer(2),
            Fx::Fd(0),
            Fx::Fd(1),
            Fx::Fd(2)
        ]
    );
    assert!(s.has_backlog());
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Timer(3), Fx::Timer(4), Fx::Fd(3)]);
    assert!(!s.has_backlog());
}

// Mutation: make `MAX_STEP_EFFECTS` one less (`+ 5`), or delete the
// `ReadLayout` branch of `read_without_restart`, or the `midi_cc` one; the count no longer equals it.
#[test]
fn the_busiest_tick_emits_exactly_the_documented_maximum() {
    let mut s = active(REST, prep(1));
    for i in 0..TIMERS_PER_TICK as u32 {
        s = req(s, Request::Timer(TimerId(i)));
    }
    for i in 0..FDS_PER_TICK as i32 {
        s = req(s, Request::Fd(FdId(i)));
    }
    for r in [
        Request::LatencyChanged,
        Request::TailChanged,
        Request::IoChanged,
        Request::RescanParams(RescanScope::Values),
        Request::RescanParams(RescanScope::Info),
        Request::MidiCcAssignmentChanged,
        Request::NoteExpressionChanged,
    ] {
        s = req(s, r);
    }
    let (_, fx) = go(s, Event::Tick);
    assert_eq!(fx.len(), MAX_STEP_EFFECTS);
    assert_eq!(MAX_STEP_EFFECTS, EFFECT_CAPACITY);
}

// Mutation: in the `Refused` arm keep the timers (`clear_requests` instead of
// `Pending::default()`); the stale timer runs after the revival.
#[test]
fn a_refusal_forgets_what_was_pending() {
    let p = prep(1);
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let s = req(s, Request::Timer(TimerId(1)));
    let s = req(s, Request::Fd(FdId(2)));
    let s = req(s, Request::Callback);
    let (s, _) = reply(s, Reply::Refused(err()));
    let (s, _) = go(s, Event::Op(HostOp::Activate(p)));
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
}

// Mutation: drop `self.rebuild_pending |= ...` from the `Refused` arm; the
// retry activates a plugin whose rebuild failed.
#[test]
fn a_refused_rebuild_is_retried_before_the_activation() {
    let p = prep(1);
    let s = req(active(RESTARTS, p), Request::Reload);
    let (s, _) = go(s, Event::Tick);
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Rebuild]);
    let (s, fx) = reply(s, Reply::Refused(err()));
    assert_eq!(fx, [Fx::RefusedNotice]);
    assert!(matches!(s.stage(), Stage::Refused { .. }));
    let (s, fx) = go(s, Event::Op(HostOp::Activate(p)));
    assert_eq!(fx, [Fx::Rebuild]);
    let (s, fx) = reply(s, Reply::Rebuilt);
    assert_eq!(fx, [Fx::Reloaded, Fx::Activate(p)]);
    // Once rebuilt, a later refusal retries the activation alone.
    let (s, _) = reply(s, Reply::Refused(err()));
    let (_, fx) = go(s, Event::Op(HostOp::Activate(p)));
    assert_eq!(fx, [Fx::Activate(p)]);
}

// Mutation: delete `std::mem::take(&mut self.after_main)` (clear only on
// `Done`); the unrelated `Done` below plans a restart.
#[test]
fn only_the_answer_to_the_main_thread_work_continues_the_tick() {
    let s = req(active(RESTARTS, prep(1)), Request::Callback);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Main]);
    // The work is answered with something other than `Done`.
    let (s, fx) = reply(s, Reply::NotActive);
    assert!(fx.is_empty());
    let s = req(s, Request::Restart);
    let (_, fx) = reply(s, Reply::Done);
    assert!(fx.is_empty());
}

proptest! {
    /// The host's latest intent wins: with ops raised between any effect and
    /// its reply, the plugin settles in the state the ops ask for, idle. The
    /// intent is a final stage and `Prepare`: `Reconfigure(p)` asks for an
    /// active plugin at `p`, `Deactivate` for a loaded one, and `Activate(p)`
    /// for an active one at `p` only when the intent is not active already.
    /// When the plugin may refuse or stick, nothing is promised about the end
    /// state, only that it settles without breaking a transition.
    ///
    /// Mutation: delete the `self.replay(out);` call from the `Activated` arm
    /// (an op raised during an activation is lost); make `HostOp::Reconfigure`
    /// on a busy plan do nothing (`_ => {}`); let `HostOp::Activate` rewrite a
    /// plan that ends active (drop the `leads_active` arm); or let `merge_later`
    /// replace a kept `Reconfigure` with an `Activate` (`(op, _) => op`); this
    /// fails.
    #[test]
    fn the_latest_op_wins_whenever_it_is_raised(
        rules in rules(),
        ops in prop::collection::vec((0u8..3, any::<u8>()), 1..8),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        local in any::<bool>(),
        flaky in any::<bool>(),
    ) {
        let rt = if local { Rt::Local } else { Rt::Thread };
        let refuse = if flaky { Refuse::Choice } else { Refuse::Never };
        let mut shell = Shell::new(rules, refuse, rt, choices);
        let op = |(kind, n): (u8, u8)| match kind {
            0 => HostOp::Reconfigure(prep(n)),
            1 => HostOp::Deactivate,
            _ => HostOp::Activate(prep(n)),
        };
        shell.inject = ops[1..].iter().copied().map(op).collect();
        shell.op(op(ops[0]));
        while !shell.inject.is_empty() {
            let next = shell.inject.remove(0);
            shell.op(next);
        }
        while shell.returning {
            shell.return_processor();
        }
        prop_assert!(!shell.state.is_busy());
        for rec in &shell.trace {
            prop_assert!(!rec.rejoin_while_out);
            prop_assert!(!rec.transition_out_of_turn);
        }
        if !flaky {
            let mut want = None;
            for o in ops {
                match op(o) {
                    HostOp::Reconfigure(p) => want = Some(p),
                    HostOp::Deactivate => want = None,
                    HostOp::Activate(p) => want = want.or(Some(p)),
                }
            }
            match want {
                Some(p) => {
                    prop_assert_eq!(shell.state.stage(), Stage::Active);
                    prop_assert_eq!(shell.state.prepare(), Some(p));
                }
                None => prop_assert_eq!(shell.state.stage(), Stage::Loaded),
            }
        }
    }
}

// Mutation: drop the `_ if self.leads_active() => {}` arm of
// `HostOp::Activate` in `on_op`; the restart is rewritten to `r`.
#[test]
fn an_activate_leaves_an_active_plugins_restart_as_it_is() {
    let (q, r) = (prep(2), prep(3));
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, _) = reply(s, Reply::Lent);
    let (s, fx) = go(s, Event::Op(HostOp::Activate(r)));
    assert!(fx.is_empty());
    let (s, _) = go(s, Event::Returned);
    let (_, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(q)]);
}

// Mutation: make `merge_later` return `op` first (`(op, _) => op` as its
// first arm); the `Activate` displaces the kept `Reconfigure` and the plugin
// ends at the first `Prepare`. Drop its `Deactivate` arm and the last case
// stays unloaded.
#[test]
fn an_activate_does_not_displace_a_kept_op_that_leaves_the_plugin_active() {
    let (p, q, r) = (prep(1), prep(2), prep(3));
    let settle = |s: State| {
        let (s, _) = reply(s, Reply::Activated);
        let (s, _) = reply(s, Reply::Returned);
        let (s, fx) = reply(s, Reply::Deactivated);
        (s, fx)
    };
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, _) = go(s, Event::Op(HostOp::Activate(r)));
    let (_, fx) = settle(s);
    assert_eq!(fx, [Fx::Activate(q)]);
    // After a kept deactivate, an activate asks for an activation at its own
    // `Prepare`.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, _) = go(s, Event::Op(HostOp::Deactivate));
    let (s, _) = go(s, Event::Op(HostOp::Activate(r)));
    let (_, fx) = settle(s);
    assert_eq!(fx, [Fx::Activate(r)]);
}

// Mutation: delete `self.reread_params = true;` in `settle_requests`; the
// parameters are not read after the next activation.
#[test]
fn a_rescan_raised_before_a_deactivation_is_read_after_the_next_activation() {
    let q = prep(2);
    let s = req(
        active(RESTARTS, prep(1)),
        Request::RescanParams(RescanScope::All),
    );
    let (s, fx) = go(s, Event::Op(HostOp::Deactivate));
    assert_eq!(fx, [Fx::Recall]);
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert!(fx.is_empty());
    let (s, _) = go(s, Event::Op(HostOp::Activate(q)));
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx[4..],
        [
            Fx::Params(RescanScope::All),
            Fx::ParamTitles,
            Fx::ParamValues
        ]
    );
    // It is read once.
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, _) = reply(s, Reply::Returned);
    let (s, _) = reply(s, Reply::Deactivated);
    let (_, fx) = reply(s, Reply::Activated);
    assert_eq!(fx.len(), 4);
}

// Mutation: accept `Request::Callback` in every stage (`active ||
// matches!(self.stage, Stage::Loaded | Stage::Refused { .. })`); the
// activation that follows then runs it.
#[test]
fn a_callback_raised_on_a_refused_plugin_is_dropped() {
    let s = req(refused(RESTARTS, prep(1)), Request::Callback);
    assert!(go(s.clone(), Event::Tick).1.is_empty());
    // Nor is it kept for the activation that follows.
    let (s, _) = go(s, Event::Op(HostOp::Activate(prep(1))));
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
}

/// A MIDI-mapping change is rebuilt in place, once, with no restart; a
/// note-expression change is told to the host once.
///
/// Mutation: in `read_without_restart` drop the `p.midi_cc = false` reset and
/// the second tick repeats the rebuild; drop the `p.note_expr = false` reset
/// and the second tick repeats the notice; delete the `if p.midi_cc` block and
/// the rebuild is never emitted.
#[test]
fn a_midi_mapping_or_note_expression_change_needs_no_restart() {
    for rules in [REST, RESTARTS] {
        let s = req(active(rules, prep(1)), Request::MidiCcAssignmentChanged);
        let s = req(s, Request::NoteExpressionChanged);
        let (s, fx) = go(s, Event::Tick);
        assert_eq!(fx, [Fx::MidiMapping, Fx::NoteExprNotice]);
        assert!(go(s, Event::Tick).1.is_empty());
    }
}

/// Raised beside a restart, the mapping is rebuilt once after the activation
/// and the note-expression notice is told once.
///
/// Mutation: delete the `if midi_cc { tasks.push(..) }` in `plan_restart` and
/// the rebuild is lost; delete the notice emit there and the notice is lost.
#[test]
fn a_mapping_change_beside_a_restart_is_rebuilt_after_the_activation() {
    let s = req(active(RESTARTS, prep(1)), Request::Restart);
    let s = req(s, Request::MidiCcAssignmentChanged);
    let s = req(s, Request::NoteExpressionChanged);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::NoteExprNotice, Fx::Recall]);
    let (s, fx) = reply(s, Reply::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(prep(1))]);
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx,
        [Fx::Latency, Fx::Tail, Fx::Layout, Fx::Lend, Fx::MidiMapping]
    );
    assert!(go(s, Event::Tick).1.is_empty());
}

// Mutation (run): make `State::set_mode` a no-op; the restart's activation
// carries the old mode and this fails. Make it emit an effect (a plan); the
// emptiness check fails.
#[test]
fn set_mode_changes_the_next_activation_and_restarts_nothing() {
    let p = prep(1);
    let mut s = active(RESTARTS, p);
    s.set_mode(RenderMode::Offline);
    assert_eq!(s.prepare().map(|p| p.mode), Some(RenderMode::Offline));
    assert_eq!(s.stage(), Stage::Active);
    assert!(
        go(s.clone(), Event::Tick).1.is_empty(),
        "nothing is planned"
    );

    let s = req(s, Request::Restart);
    let (s, _) = go(s, Event::Tick);
    let (s, _) = reply(s, Reply::Returned);
    let (_, fx) = reply(s, Reply::Deactivated);
    let want = Prepare {
        mode: RenderMode::Offline,
        ..p
    };
    assert_eq!(fx, [Fx::Activate(want)]);
}
