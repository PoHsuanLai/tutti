//! [`Lifecycle`] against a [`FakePlugin`]: the interpreter's contract without
//! a plugin.
//!
//! Each test's comment names the mutation that makes it fail. Every mutation
//! was applied to `driver.rs` (or `fake.rs`), the test watched to fail, and
//! the code restored.
//!
//! Most tests hold the Processor here (a local `Hand`). The lent-Processor
//! paths (a recall that answers `Lent`, a parked plan resumed by
//! `Event::Returned`) run with a threaded one, whose realtime end the test
//! polls itself (`Rig::threaded`).

use super::*;
use crate::fake::{Call, FakeLoaded, FakePlugin};
use crate::{
    AudioBuffer, AudioBufferMut, HostOp, MaxBlock, PluginError, PluginIo, PluginTail,
    ProcessContext, ProcessOutput, RenderMode, Request, RequestWord, SampleFormat, Samples, Stage,
    taken,
};

/// The test process's main thread: nextest runs each test in its own process,
/// so the claim always succeeds.
fn main() -> MainThread {
    MainThread::claim().expect("one test, one process")
}

fn prepare(rate: f64) -> Prepare {
    Prepare::new(SampleRate(rate), MaxBlock::of(Samples(64)))
        .expect("the tests use real rates")
        .with_format(SampleFormat::Float32)
        .with_mode(RenderMode::Realtime)
}

struct Rig {
    plugin: FakePlugin,
    lifecycle: Lifecycle<FakeLoaded>,
    state: State,
    hand: Hand<<FakeLoaded as Loaded>::Processor>,
    notices: Vec<Notice>,
}

impl Rig {
    fn new() -> Self {
        let (plugin, loaded) = FakePlugin::new();
        Self {
            plugin,
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<FakeLoaded>::initial_state(),
            hand: Hand::local(),
            notices: Vec::new(),
        }
    }

    /// A rig whose Processor goes to a realtime end the test holds.
    fn threaded() -> (Self, tutti_rt::Rt<<FakeLoaded as Loaded>::Processor>) {
        let (hand, rt) = Hand::threaded();
        (
            Self {
                hand,
                ..Self::new()
            },
            rt,
        )
    }

    fn drive(&mut self, event: Event) {
        let state = self.state.clone();
        let notices = &mut self.notices;
        self.state = self
            .lifecycle
            .drive(state, event, &mut self.hand, &main(), |n| notices.push(n));
    }

    fn op(&mut self, op: HostOp) {
        self.drive(Event::Op(op));
    }

    /// Delivers what the plugin raised, then the idle pass.
    fn tick(&mut self) {
        let requests: Vec<_> = taken(self.plugin.request_word()).collect();
        for request in requests {
            self.drive(Event::Request(request));
        }
        self.drive(Event::Tick);
    }

    fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    /// Renders one block of `frames` through the held Processor.
    fn render(&mut self, frames: usize) -> PluginResult<()> {
        let rate = self.state.prepare().map_or(48_000.0, |p| p.rate().0);
        let input = vec![0.0f32; frames];
        let mut left = vec![1.0f32; frames];
        let mut right = vec![1.0f32; frames];
        let inputs = [input.as_slice(), input.as_slice()];
        let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
        let ctx = ProcessContext::new();
        let mut out = ProcessOutput::default();
        let mut io = PluginIo {
            buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, rate)),
            ctx: &ctx,
            out: &mut out,
        };
        self.lifecycle
            .processor()
            .expect("the Processor is held")
            .process(&mut io)
    }
}

fn activations(calls: &[Call]) -> usize {
    calls
        .iter()
        .filter(|c| matches!(c, Call::Activate(..)))
        .count()
}

/// **An activation splits the plugin, reads latency, tail and layout, and
/// leaves the Processor in the seat.** The reads are the Controller's, after
/// `activate` and before anything renders.
///
/// Mutation (run): `Lifecycle::activate` putting `Seat::Lent` instead of
/// `Held` → `processor()` is `None` and `render` fails; `ReadTail` answering
/// `NotActive` → the call log has no `Tail`.
#[test]
fn activation_splits_the_plugin_and_reads_its_figures() {
    let mut rig = Rig::new();
    rig.plugin.set_latency(Samples(32));
    rig.op(HostOp::Activate(prepare(48_000.0)));

    assert_eq!(rig.state.stage(), Stage::Active);
    assert!(matches!(rig.lifecycle, Lifecycle::Active { .. }));
    assert_eq!(
        rig.plugin.calls(),
        [
            Call::Activate(SampleRate(48_000.0), Samples(64)),
            Call::Latency,
            Call::Tail,
            Call::Wiring,
        ]
    );
    rig.render(64).unwrap();
    assert_eq!(rig.plugin.calls().last(), Some(&Call::Process(64)));
}

/// **A refused activation leaves `Refused` with the plugin intact and no half
/// pair; a later rate revives it.** The host is told once.
///
/// Mutation (run): `Lifecycle::activate` dropping the plugin on `Err`
/// (leaving `Poisoned`) → the stage assertion fails.
#[test]
fn a_refused_rate_leaves_the_plugin_loaded_and_a_later_rate_revives_it() {
    let mut rig = Rig::new();
    rig.plugin.refuse_rate(Some(SampleRate(22_050.0)));
    rig.op(HostOp::Activate(prepare(22_050.0)));

    assert!(matches!(
        rig.state.stage(),
        Stage::Refused { rate } if rate == SampleRate(22_050.0)
    ));
    assert!(matches!(rig.lifecycle, Lifecycle::Refused { .. }));
    assert!(rig.lifecycle.processor().is_none(), "no half pair");
    assert!(rig.lifecycle.control().is_some(), "the plugin is intact");
    let notices = rig.take_notices();
    assert!(
        matches!(notices.as_slice(), [Notice::Refused { rate, .. }] if *rate == SampleRate(22_050.0)),
        "{notices:?}"
    );

    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
    rig.render(64).unwrap();
}

/// **A refusal that hands no plugin back poisons, after the host was told.**
///
/// Mutation (run): `drive` not stepping `Died` for a poisoned lifecycle → the
/// stage stays `Refused`; clearing the queue without delivering its notices →
/// the `Refused` notice is missing.
#[test]
fn a_lost_plugin_poisons_and_the_refusal_is_still_told() {
    let mut rig = Rig::new();
    rig.plugin.refuse_rate(Some(SampleRate(8_000.0)));
    rig.plugin.lose_plugin_on_refusal(true);
    rig.op(HostOp::Activate(prepare(8_000.0)));

    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::Refused { .. }]
    ));
    assert!(rig.lifecycle.control().is_none());
    assert!(Hosted::info(&rig.lifecycle).is_none());
    assert!(Hosted::save_state(&mut rig.lifecycle, &main()).is_err());
}

/// **A panic in `activate` poisons the same way.**
///
/// Mutation (run): `Lifecycle::activate` without `catch_unwind` → the test
/// panics instead of observing `Poisoned`.
#[test]
fn a_panic_in_activate_poisons() {
    let mut rig = Rig::new();
    rig.plugin.panic_in_activate(true);
    rig.op(HostOp::Activate(prepare(48_000.0)));

    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
}

/// **A plugin that will not stop stays active with both halves, and the host
/// is told; the next deactivation takes.**
///
/// Mutation (run): `Lifecycle::deactivate` leaving `Poisoned` on `Err(Stuck)` →
/// `render` cannot find the Processor.
#[test]
fn stuck_restores_both_halves() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.stick_deactivations(1);
    rig.take_notices();

    rig.op(HostOp::Deactivate);
    assert_eq!(rig.state.stage(), Stage::Active);
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::Stuck { .. }]
    ));
    rig.render(64).unwrap();

    rig.op(HostOp::Deactivate);
    assert_eq!(rig.state.stage(), Stage::Loaded);
    assert!(matches!(rig.lifecycle, Lifecycle::Loaded(_)));
    assert_eq!(
        rig.plugin
            .calls()
            .iter()
            .filter(|c| **c == Call::Deactivate)
            .count(),
        1
    );
}

/// **What a `Loaded` must do with a pair from two activations: refuse it, and
/// hand both halves back.** This pins the fake's implementation of the
/// contract; each format's own `deactivate` carries its own check and test.
///
/// Mutation (run): `FakeLoaded::deactivate` skipping the `Arc::ptr_eq` check →
/// the second pair's plugin is returned `Ok` and the assertion fails.
#[test]
fn a_pair_from_two_activations_is_refused() {
    let (plugin, loaded) = FakePlugin::new();
    let (p1, c1) = loaded.activate(&main(), prepare(48_000.0)).unwrap();
    let c1 = FakeLoaded::deactivate(&main(), p1, c1).unwrap();
    let (p1, c1) = c1.activate(&main(), prepare(48_000.0)).unwrap();
    let (_, other) = FakePlugin::new();
    let (p2, c2) = other.activate(&main(), prepare(48_000.0)).unwrap();

    let stuck = FakeLoaded::deactivate(&main(), p1, c2).unwrap_err();
    assert!(stuck.reason.to_string().contains("two activations"));
    // Both halves are back and still pair with their own.
    FakeLoaded::deactivate(&main(), p2, c1).expect_err("crossed again");
    drop(plugin);
}

/// **A reconfigure deactivates then activates once, and re-reads.**
///
/// Mutation (run): `drive` dropping the reply of `CallDeactivate` → the plan
/// stops and the second activation is missing.
#[test]
fn a_reconfigure_is_one_deactivate_and_one_activate() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.clear_calls();

    rig.op(HostOp::Reconfigure(prepare(96_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
    let calls = rig.plugin.calls();
    assert_eq!(
        calls,
        [
            Call::Deactivate,
            Call::Activate(SampleRate(96_000.0), Samples(64)),
            Call::Latency,
            Call::Tail,
            Call::Wiring,
        ]
    );
    rig.render(64).unwrap();
}

/// **A latency the plugin announces restarts it once and reaches the host as a
/// notice; the restart's own re-read is not a second restart.**
///
/// Mutation (run): `Lifecycle::apply` answering `ReadLatency` with
/// `NotActive` → no `LatencyChanged` notice.
#[test]
fn a_latency_change_restarts_once_and_is_announced() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();

    rig.plugin.set_latency(Samples(128));
    Request::LatencyChanged.raise(rig.plugin.request_word());
    rig.tick();

    assert_eq!(activations(&rig.plugin.calls()), 2);
    let notices = rig.take_notices();
    assert!(
        matches!(notices.as_slice(), [Notice::LatencyChanged(Samples(128))]),
        "{notices:?}"
    );
    rig.tick();
    assert_eq!(activations(&rig.plugin.calls()), 2, "nothing left to do");
}

/// **A tail that changes is re-read in place, without a restart.**
///
/// Mutation (run): `Lifecycle::apply` answering `ReadTail` with `Done` → no
/// notice.
#[test]
fn a_tail_change_is_read_in_place() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();

    rig.plugin.set_tail(Samples(500));
    Request::TailChanged.raise(rig.plugin.request_word());
    rig.tick();

    assert_eq!(activations(&rig.plugin.calls()), 1);
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::TailChanged(PluginTail::Finite(Samples(500)))]
    ));
}

/// **A failed rebuild leaves `Refused`, tells the host once, and the next
/// activation rebuilds before it activates.**
///
/// Mutation (run): `Lifecycle::rebuild` returning `Reply::Rebuilt` on `Err` →
/// the stage is `Loaded` and no notice is sent.
#[test]
fn a_failed_rebuild_is_refused_and_the_next_activation_rebuilds_first() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();
    rig.plugin.fail_rebuild(true);
    Request::Reload.raise(rig.plugin.request_word());
    rig.tick();

    assert!(matches!(rig.state.stage(), Stage::Refused { .. }));
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::Refused { .. }]
    ));
    assert!(matches!(
        rig.lifecycle,
        Lifecycle::Refused { rate: None, .. }
    ));

    rig.plugin.fail_rebuild(false);
    rig.plugin.clear_calls();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let calls = rig.plugin.calls();
    assert_eq!(calls[0], Call::Rebuild, "{calls:?}");
    assert!(matches!(calls[1], Call::Activate(..)));
    assert_eq!(rig.state.stage(), Stage::Active);
}

/// **A parameter set on an active plugin reaches the Processor once, at the
/// head of its next block, in order; on a loaded plugin it is applied now.**
///
/// Mutation (run): `FakeController::set_parameter` not sending the edit → no
/// `Edit` call; `FakeProcessor::process` draining after logging `Process` →
/// the order assertion fails.
#[test]
fn a_parameter_set_reaches_the_processor_at_the_next_block() {
    let mut rig = Rig::new();
    let (a, b) = (ParamId::new(3), ParamId::new(9));
    rig.lifecycle
        .control()
        .unwrap()
        .set_parameter(&main(), a, Normalized::of(0.5));
    assert_eq!(rig.plugin.calls(), [Call::SetParameter(a)]);
    rig.plugin.clear_calls();

    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert_eq!(
        Hosted::parameter(&rig.lifecycle, &main(), a).map(Normalized::get),
        Some(0.5),
        "values survive the split"
    );
    rig.plugin.clear_calls();
    Hosted::set_parameter(&mut rig.lifecycle, &main(), a, Normalized::of(0.25));
    Hosted::set_parameter(&mut rig.lifecycle, &main(), b, Normalized::of(0.75));
    rig.render(64).unwrap();
    rig.render(64).unwrap();
    assert_eq!(
        rig.plugin.calls(),
        [
            Call::SetParameter(a),
            Call::SetParameter(b),
            Call::Edit(a, Normalized::of(0.25)),
            Call::Edit(b, Normalized::of(0.75)),
            Call::Process(64),
            Call::Process(64),
        ]
    );
}

/// **After `Died` no effect reaches the plugin.**
///
/// The property is the core's (`Poisoned` absorbs every later event, mutation-
/// tested with `step`); this pins that `drive` then applies nothing. No
/// mutation of `driver.rs` is specific to it.
#[test]
fn nothing_runs_after_died() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.drive(Event::Died);
    assert_eq!(rig.state.stage(), Stage::Poisoned);
    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    assert_eq!(rig.state.stage(), Stage::Poisoned);
    assert_eq!(
        activations(&rig.plugin.calls()),
        1,
        "nothing runs after Died"
    );
}

/// **The control surface is reachable in every state that has a plugin.**
///
/// Mutation (run): `Lifecycle::control` answering `None` for `Refused` → the
/// last assertion fails; for `Active` → the second fails.
#[test]
fn control_is_reachable_loaded_active_and_refused() {
    let mut rig = Rig::new();
    assert!(rig.lifecycle.control().is_some());
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(rig.lifecycle.control().is_some());
    assert!(Hosted::wiring(&rig.lifecycle, &main()).is_some());
    rig.plugin.refuse_rate(Some(SampleRate(1_000.0)));
    rig.op(HostOp::Reconfigure(prepare(1_000.0)));
    assert!(matches!(rig.lifecycle, Lifecycle::Refused { .. }));
    assert!(rig.lifecycle.control().is_some());
}

/// **State round-trips through the control surface, loaded and active.**
///
/// Mutation (run): `FakeController::load_state` not storing the bytes → the
/// saved state is empty and the assertion fails.
#[test]
fn state_round_trips_through_hosted() {
    let mut rig = Rig::new();
    Hosted::load_state(&mut rig.lifecycle, &main(), b"abc").unwrap();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert_eq!(
        Hosted::save_state(&mut rig.lifecycle, &main()).unwrap(),
        b"abc"
    );
}

/// **A block through the Processor, and the edit drain at its head, neither
/// allocate nor lock.**
///
/// Mutation (run): `FakeProcessor::process` collecting the drained edits into a
/// `Vec` → the gate aborts.
#[test]
fn the_processor_renders_without_allocating() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let frames = 64;
    let input = vec![0.0f32; frames];
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    let inputs = [input.as_slice(), input.as_slice()];
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    let processor = rig.lifecycle.processor().expect("held");
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let mut io = PluginIo {
        buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, 48_000.0)),
        ctx: &ctx,
        out: &mut out,
    };
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            processor.process(&mut io).unwrap();
        }
        processor.stop();
    });
    assert_eq!(
        rig.plugin
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Process(_)))
            .count(),
        100
    );
}

/// **Requests raised on another thread, while the main thread takes, arrive
/// once each.**
///
/// Mutation (run): `taken` yielding every request whether or not its bit is
/// set → the first take returns all twelve and the final comparison fails.
#[test]
fn requests_raised_while_taking_arrive_once() {
    let word = std::sync::Arc::new(RequestWord::new());
    let raiser = {
        let word = word.clone();
        std::thread::spawn(move || {
            Request::Restart.raise(&word);
            Request::TailChanged.raise(&word);
        })
    };
    let mut got = Vec::new();
    while got.len() < 2 {
        got.extend(taken(&word));
        std::thread::yield_now();
    }
    raiser.join().unwrap();
    got.extend(taken(&word));
    got.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(got, [Request::Restart, Request::TailChanged]);
}

/// **A panic in `deactivate` poisons and the death is stepped.**
///
/// Mutation (run): `Lifecycle::deactivate` without `catch_unwind` → the test
/// panics.
#[test]
fn a_panic_in_deactivate_poisons() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.panic_in_deactivate(true);
    rig.op(HostOp::Deactivate);
    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
}

/// **A panic in `rebuild` poisons.**
///
/// Mutation (run): `Lifecycle::rebuild` without `catch_unwind` → the test
/// panics.
#[test]
fn a_panic_in_rebuild_poisons() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.panic_in_rebuild(true);
    Request::Reload.raise(rig.plugin.request_word());
    rig.tick();
    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
}

/// **A panic out of a read poisons, and the `State` is not lost with the
/// unwind.** `drive` takes the state by value, so a panic that escaped would
/// drop it.
///
/// Mutation (run): `Lifecycle::apply` calling `apply_in_place` without
/// `guarded` → the test panics.
#[test]
fn a_panic_in_a_read_poisons_without_losing_the_state() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.panic_in_latency(true);
    rig.plugin.set_latency(Samples(8));
    Request::LatencyChanged.raise(rig.plugin.request_word());
    rig.tick();
    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
}

/// **A deactivation that is stuck, then a reconfigure, ends active at the new
/// rate once the plugin lets go.**
///
/// Mutation (run): `Lifecycle::deactivate` poisoning on `Err(Stuck)` → the
/// stage is `Poisoned`.
#[test]
fn stuck_then_reconfigure_settles_at_the_new_rate() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.stick_deactivations(1);
    rig.op(HostOp::Reconfigure(prepare(96_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
    assert_eq!(
        rig.state.prepare().map(|p| p.rate()),
        Some(SampleRate(48_000.0))
    );
    rig.op(HostOp::Reconfigure(prepare(96_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
    assert_eq!(
        rig.state.prepare().map(|p| p.rate()),
        Some(SampleRate(96_000.0))
    );
    rig.render(64).unwrap();
}

/// **A rebuild that is refused twice stays `Refused` with no rate, and the
/// attempt that takes revives it.**
///
/// Mutation (run): `Lifecycle::rebuild` recording `Some(rate)` from a stale
/// field instead of carrying `rate` → the `rate: None` match fails.
#[test]
fn a_refused_rebuild_is_retried_with_no_rate() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.fail_rebuild(true);
    Request::Reload.raise(rig.plugin.request_word());
    rig.tick();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(matches!(
        rig.lifecycle,
        Lifecycle::Refused { rate: None, .. }
    ));
    rig.plugin.fail_rebuild(false);
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
}

/// **An edit the full ring refused arrives, latest value only, when the idle
/// pass retries.**
///
/// Mutation (run): `drive` not calling `flush_edits` on `Tick` → no `Edit`
/// after the block; `FakeController::flush_edits` resending zero
/// instead of the latest → the value assertion fails.
#[test]
fn a_refused_edit_is_retried_on_the_next_tick() {
    let mut rig = Rig::new();
    rig.plugin.edit_capacity(1);
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let (a, b) = (ParamId::new(1), ParamId::new(2));
    Hosted::set_parameter(&mut rig.lifecycle, &main(), a, Normalized::of(0.1));
    // The ring holds one: these are refused, `b` twice.
    Hosted::set_parameter(&mut rig.lifecycle, &main(), b, Normalized::of(0.2));
    Hosted::set_parameter(&mut rig.lifecycle, &main(), b, Normalized::of(0.9));
    rig.plugin.clear_calls();
    rig.render(64).unwrap();
    assert_eq!(
        rig.plugin.calls(),
        [Call::Edit(a, Normalized::of(0.1)), Call::Process(64)]
    );

    rig.plugin.clear_calls();
    rig.tick();
    rig.render(64).unwrap();
    assert_eq!(
        rig.plugin.calls(),
        [Call::Edit(b, Normalized::of(0.9)), Call::Process(64)]
    );
    assert_eq!(
        Hosted::parameter(&rig.lifecycle, &main(), b).map(Normalized::get),
        Some(0.9)
    );
    rig.tick();
    rig.plugin.clear_calls();
    rig.render(64).unwrap();
    assert_eq!(
        rig.plugin.calls(),
        [Call::Process(64)],
        "nothing left to send"
    );
}

/// **A VST3 mapping change reaches the plugin as one rebuild, and a
/// note-expression change as one notice.**
///
/// Mutation (run): `Lifecycle::apply_in_place` answering `RebuildMidiMapping`
/// with `Done` without calling the control → no `RebuildMidiMapping` call.
#[test]
fn a_mapping_change_rebuilds_the_mapping_once() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();
    rig.plugin.clear_calls();
    Request::MidiCcAssignmentChanged.raise(rig.plugin.request_word());
    Request::NoteExpressionChanged.raise(rig.plugin.request_word());
    rig.tick();
    rig.tick();
    assert_eq!(rig.plugin.calls(), [Call::RebuildMidiMapping]);
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::NoteExpressionChanged]
    ));
}

/// **A panic out of the idle flush or a read tells the host the plugin is
/// lost.** Without the notice the lifecycle poisons silently and the host
/// keeps a plugin that renders nothing.
///
/// Mutation (run): removing both `Notice::Lost` sends in `drive` → no notice
/// arrives and the assertions fail.
#[test]
fn a_panic_outside_a_transition_is_told_to_the_host() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();
    rig.plugin.panic_in_latency(true);
    Request::LatencyChanged.raise(rig.plugin.request_word());
    rig.tick();
    assert!(rig.lifecycle.is_poisoned());
    let notices = rig.take_notices();
    assert!(
        notices.iter().any(|n| matches!(n, Notice::Lost { .. })),
        "{notices:?}"
    );
}

/// **A panic in a transition is told once, by `Refused`.**
///
/// Mutation (run): sending `Notice::Lost` for a transition too → the count
/// of `Lost` notices is one and the assertion fails.
#[test]
fn a_panic_in_a_transition_is_not_told_twice() {
    let mut rig = Rig::new();
    rig.plugin.panic_in_activate(true);
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let notices = rig.take_notices();
    assert!(rig.lifecycle.is_poisoned());
    assert!(notices.iter().any(|n| matches!(n, Notice::Refused { .. })));
    assert!(!notices.iter().any(|n| matches!(n, Notice::Lost { .. })));
}

/// The realtime end's poll between blocks, with `stop` as its last step.
fn poll(rt: &mut tutti_rt::Rt<<FakeLoaded as Loaded>::Processor>) -> bool {
    rt.poll(Processor::stop).is_some()
}

/// **With a threaded hand an activation lends the Processor, and a rate change
/// recalls it, parks, and resumes when the realtime thread returns it.** The
/// realtime end stops it before it hands it back; the next tick collects it,
/// rejoins, activates at the new rate and lends again.
///
/// Mutation (run): `drive` not collecting a returned Processor on a tick
/// (`trade` answering `false` for `Lent`) → the plan stays parked and the
/// second activation never comes; `recall` answering `Returned` while lent →
/// the plan runs without the Processor, and this test and the next fail.
#[test]
fn a_rate_change_recalls_the_lent_processor_and_lends_the_new_one() {
    let (mut rig, mut rt) = Rig::threaded();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(rig.lifecycle.processor().is_none(), "lent at activation");
    assert!(poll(&mut rt), "the realtime end takes it");

    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    assert!(rig.state.is_parked(), "waits for the Processor");
    assert!(!rig.plugin.calls().contains(&Call::Deactivate));

    assert!(!poll(&mut rt), "returned");
    assert_eq!(rig.plugin.calls().last(), Some(&Call::Stop));
    rig.tick();
    assert!(!rig.state.is_parked());
    let calls = rig.plugin.calls();
    assert!(calls.contains(&Call::Deactivate));
    assert!(calls.contains(&Call::Activate(SampleRate(44_100.0), Samples(64))));
    assert_eq!(activations(&calls), 2);
    assert!(poll(&mut rt), "the new Processor reaches the realtime end");
}

/// **A recall before the realtime thread took the Processor withdraws it at
/// once:** the plan runs in the same drive, with no tick and no stop.
///
/// Mutation (run): `recall` ignoring what `tutti_rt::Hand::recall` withdrew
/// (answering `Lent`) → the plan parks and the second activation is missing.
#[test]
fn a_recall_before_the_realtime_thread_took_it_is_immediate() {
    let (mut rig, mut rt) = Rig::threaded();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    assert!(!rig.state.is_parked());
    let calls = rig.plugin.calls();
    assert_eq!(activations(&calls), 2);
    assert!(
        !calls.contains(&Call::Stop),
        "it never reached the realtime end"
    );
    assert!(poll(&mut rt), "the new one is lent");
}

/// **A lend the handoff refuses keeps the Processor here and is offered again
/// on a tick.** A recall raised with nothing lent refuses the next lend until
/// the realtime end clears it.
///
/// Mutation (run): `trade` not offering a held Processor again → the realtime
/// end never gets it.
#[test]
fn a_refused_lend_is_offered_again_on_a_tick() {
    let (mut rig, mut rt) = Rig::threaded();
    // A stale recall: nothing is lent, so the realtime end clears it at its
    // next poll, and until then a lend is refused.
    assert!(rig.hand.end.as_mut().unwrap().recall().is_none());
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(rig.lifecycle.processor().is_some(), "refused, kept here");
    assert!(!poll(&mut rt), "nothing lent; the stale recall is cleared");
    rig.tick();
    assert!(rig.lifecycle.processor().is_none(), "offered again");
    assert!(poll(&mut rt));
}

/// **The fake's explicit answers for the editor, render-mode, request and
/// reported-edit entries of `Control` reach the host through
/// [`Lifecycle::control`] in both states that hold a plugin**: no editor (and
/// the floating window refused by its one default), no live render mode, no
/// requests, no reported edits.
///
/// Mutation (run): the fake's `set_render_mode` returning `true` fails the
/// first assertion; its `has_editor` returning `true` fails the second.
#[test]
fn the_fake_control_answers_nothing_in_either_state() {
    let mut rig = Rig::new();
    for active in [false, true] {
        if active {
            rig.op(HostOp::Activate(prepare(48_000.0)));
            assert!(rig.lifecycle.processor().is_some());
        }
        let main = main();
        let control = rig.lifecycle.control().expect("a plugin is held");

        assert!(!control.set_render_mode(&main, RenderMode::Offline));
        assert!(!control.has_editor(&main));
        assert!(matches!(
            control.open_floating_editor(&main),
            Err(PluginError::Editor(_))
        ));
        control.close_editor(&main);
        control.editor_idle(&main);

        let mut requests = Vec::new();
        control.take_requests(&main, &mut requests);
        assert!(requests.is_empty());
        let mut edits = Vec::new();
        control.take_reported_edits(&main, &mut edits);
        assert!(edits.is_empty());
    }
}

/// **A plugin its shell activated itself is in the state a driven activation
/// leaves, with its Processor lent.** `adopt` steps the pure core with the
/// replies an activation gets, so the stage, the `Prepare`, the latency, the
/// tail and the layout are what `drive` records, and the Processor is on the
/// realtime thread's end, not in the seat.
///
/// Mutation (run): `adopt` answering `Reply::Activated` and nothing else (no
/// reads) -> the latency, tail and layout are `None` -> fails. Mutation
/// (run): `adopt` not calling `lend` -> the realtime end polls nothing and
/// the seat holds the Processor -> fails. Mutation (run): the `ReadLayout`
/// arm dropped (moved into the `unreachable!` group) -> the core's layout read
/// panics -> fails. Mutation (run): the latency reply withheld -> the core
/// still awaits it, and the post-condition (active, nothing planned) fails.
#[test]
fn an_adopted_plugin_is_in_the_state_a_driven_activation_leaves() {
    let mut driven = Rig::new();
    driven.plugin.set_latency(Samples(32));
    driven.op(HostOp::Activate(prepare(48_000.0)));

    let (plugin, loaded) = FakePlugin::new();
    plugin.set_latency(Samples(32));
    let wiring = Control::wiring(&loaded, &main());
    let (processor, controller) = match loaded.activate(&main(), prepare(48_000.0)) {
        Ok(halves) => halves,
        Err(_) => panic!("the fake activates"),
    };
    let (mut hand, mut rt) = Hand::threaded();
    let (lifecycle, state) = Lifecycle::<FakeLoaded>::adopt(
        processor,
        controller,
        prepare(48_000.0),
        &wiring,
        &mut hand,
    );

    assert_eq!(state.stage(), driven.state.stage());
    assert_eq!(state.prepare(), driven.state.prepare());
    assert_eq!(state.latency(), Some(Samples(32)));
    assert_eq!(state.latency(), driven.state.latency());
    assert_eq!(state.tail(), driven.state.tail());
    assert!(state.tail().is_some());
    assert_eq!(state.layout(), driven.state.layout());
    assert!(state.layout().is_some());
    assert!(state.is_quiet(), "no plan is left under way");
    assert!(matches!(
        lifecycle,
        Lifecycle::Active {
            seat: Seat::Lent,
            ..
        }
    ));
    assert!(
        rt.poll(Processor::stop).is_some(),
        "lent to the realtime end"
    );
}
