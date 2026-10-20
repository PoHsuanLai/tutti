//! What a DAW's UI layer needs from a modulated parameter, headless.
//!
//! A fader on a modulated param has to draw two things: the **handle**, where
//! the user put it, and the **ghost**, where modulation has pushed it. They
//! differ every frame an LFO is running, and a UI that shows only one of them is
//! either lying about the sound or unable to be dragged.
//!
//! Rather than render, this prints the numbers a widget would consume. Run it:
//!
//! ```sh
//! cargo run -p bevy-tutti --features modulation --example modulated_fader
//! ```
//!
//! Two params are wired so the read path has to handle both cases: `Drive` is
//! modulated by an LFO, `Cutoff` is not. The same widget code covers both.
//!
//! The handle is the param's knob, the node's own cell, which the
//! `AudioParam` writes; the ghost is the lane's program evaluated at the
//! play clock, which is what the lane sends into the param's port. One
//! writer per knob: modulation never moves the handle.

use bevy_app::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin, SpawnAudioNode, TransportRes,
};
use bevy_tutti::modulation::{
    Lfo, ModRange, ModRoute, ModSource, ModulationState, ParamRef, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::Transport;
use tutti_core::SampleRate;
use tutti_mod::ProgramTime;
use tutti_nodes::{DistortionNode, ShapeKind};
use tutti_types::{Beat, Depth, Drive, Hz, SamplePosition, Unit, UnitParam};

const SAMPLE_RATE: f64 = 48_000.0;
/// Frames advanced per update — 10ms, roughly a UI frame.
const FRAMES_PER_TICK: u64 = 480;
const TICKS: usize = 24;

type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;
type CutoffParam = AudioParam<Hz, { UnitParam::Cutoff as u16 }>;

/// Everything a fader widget needs for one parameter.
///
/// The shape this example exists to pin down: what a widget actually reads, and
/// from where. `knob` comes from the ECS component the UI writes; `live`
/// and `range` come from the param's modulation lane. An unmodulated param has
/// no lane, so `live` falls back to `knob` and `range` to the declared
/// range — the widget needs no branch of its own.
struct FaderView {
    knob: f32,
    live: f32,
    range: (f32, f32),
    modulated: bool,
}

impl FaderView {
    /// Read one param the way a widget would, at play-clock frame `play`.
    fn read<U: Unit<Raw = f32>>(
        state: &ModulationState<'_>,
        param: ParamRef,
        knob: U,
        declared_range: (f32, f32),
        play: u64,
    ) -> Self {
        let knob = knob.to_raw();
        match state.lane(param) {
            // The lane's program is a pure function of the play clock: the
            // value it sends at `play` is this (`knob + Σ offsets`, clamped).
            // The audio thread evaluates the same program on a 32-frame grid;
            // same program, different rate — neither has to know about the
            // other.
            Some((_, lane)) => {
                let (min, max) = lane.range();
                let at = ProgramTime {
                    beat: Beat(0.0),
                    play: SamplePosition(play as f64),
                    play_epoch: 0,
                    rate: SampleRate(SAMPLE_RATE),
                };
                let live = lane
                    .program()
                    .value_at(lane.knob(), &at, f64::from(max - min), &[])
                    .map_or(knob, |v| v.clamp(min, max));
                Self {
                    knob,
                    live,
                    range: (min, max),
                    modulated: true,
                }
            }
            None => Self {
                knob,
                live: knob,
                range: declared_range,
                modulated: false,
            },
        }
    }

    /// Position in 0..1 widget space. `range` is the runtime unit contract —
    /// the reason a widget must not hardcode its own bounds.
    fn normalized(&self, value: f32) -> f32 {
        let (min, max) = self.range;
        if (max - min).abs() < f32::EPSILON {
            return 0.0;
        }
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    }

    /// A fader drawn in text: `|` is the handle, `:` the modulated ghost.
    fn draw(&self) -> String {
        const WIDTH: usize = 32;
        let handle = (self.normalized(self.knob) * WIDTH as f32) as usize;
        let ghost = (self.normalized(self.live) * WIDTH as f32) as usize;

        (0..WIDTH)
            .map(
                |i| match (i == handle.min(WIDTH - 1), i == ghost.min(WIDTH - 1)) {
                    (true, _) => '|',
                    (false, true) => ':',
                    _ => '-',
                },
            )
            .collect()
    }
}

fn main() {
    let mut app = App::new();

    let mut graph = AudioGraphRes::headless(0, 1);
    graph.set_sample_rate(SampleRate(SAMPLE_RATE));

    app.insert_resource(graph);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    // Stands in for a running device; nothing here opens one.
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>()
        .add_audio_param::<Hz, { UnitParam::Cutoff as u16 }>();

    // The node's params are captured on the way in (`spawn_audio_node`), so
    // an `AudioParam` writes the node's own cells: the knobs.
    let strip = app
        .world_mut()
        .commands()
        .spawn_audio_node(DistortionNode::new(ShapeKind::Tanh, 1.0))
        .insert((DriveParam::new(Drive(5.0)), CutoffParam::new(Hz(2_000.0))))
        .id();
    app.world_mut().flush();

    // Declaring the range is what makes a param modulatable at all. Only
    // `Drive` gets one here — `Cutoff` stays unmodulated.
    let drive = ParamRef::unit(strip, UnitParam::Drive);
    let cutoff = ParamRef::unit(strip, UnitParam::Cutoff);
    app.world_mut().spawn(ModRange::new(drive, 0.0, 10.0));
    let lfo = app
        .world_mut()
        .spawn(ModSource::new(Lfo::sine().hz(Hz(2.0))))
        .id();
    // Depth scales the raw [-1, 1] LFO by the target's *span*, so 0.2 over a
    // 0..10 range swings ±2. Much more and the swing clips against the range
    // ends and the ghost simply parks there — correct, but it stops showing
    // the mechanism.
    app.world_mut()
        .spawn(ModRoute::new(lfo, drive).with_depth(Depth(0.2)));

    println!("A modulated fader (|) and its LFO ghost (:), 10ms per line.\n");
    println!("{:<34} {:<8} {:<8}", "drive  0..10", "handle", "live");

    for tick in 0..TICKS {
        // Halfway through, the "user" drags the fader. The handle jumps and the
        // ghost follows it — modulation rides *on top of* the knob rather than
        // replacing it, because the knob is what the lane adds its offsets to.
        if tick == TICKS / 2 {
            app.world_mut()
                .entity_mut(strip)
                .insert(DriveParam::new(Drive(3.0)));
            println!("{:-<52}  user drags fader to 3.0", "");
        }

        app.update();

        let world = app.world();
        let state = world.resource::<AudioGraphRes>().modulation();
        let knob = world.get::<DriveParam>(strip).unwrap().value;
        let play = tick as u64 * FRAMES_PER_TICK;
        let view = FaderView::read(&state, drive, knob, (0.0, 10.0), play);

        println!("{} {:>7.2} {:>8.2}", view.draw(), view.knob, view.live);
    }

    // The same widget code on a param nobody modulates: handle and ghost
    // coincide, and the view reports it so a UI can skip the ghost entirely.
    let world = app.world();
    let state = world.resource::<AudioGraphRes>().modulation();
    let value = world.get::<CutoffParam>(strip).unwrap().value;
    let view = FaderView::read(&state, cutoff, value, (20.0, 20_000.0), 0);
    println!(
        "\ncutoff (unmodulated): modulated={} handle={:.0}Hz live={:.0}Hz",
        view.modulated, view.knob, view.live
    );
}
