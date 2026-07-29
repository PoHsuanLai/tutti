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

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin, TransportRes,
};
use bevy_tutti::modulation::{
    LfoShape, ModParamRange, ModRate, ModRoute, ModSource, ModTargetRegistry, ModulationMatrix,
    TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{AudioUnit as _, Net};
use tutti_core::transport::Transport;
use tutti_core::{AudioNode, SampleRate};
use tutti_types::{Depth, Drive, Hz, ParamAddr, Unit, UnitParam};
use tutti_units::{DistortionNode, ShapeKind};

const SAMPLE_RATE: f64 = 48_000.0;
/// Frames advanced per update — 10ms, roughly a UI frame.
const FRAMES_PER_TICK: i64 = 480;
const TICKS: usize = 24;

type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;
type CutoffParam = AudioParam<Hz, { UnitParam::Cutoff as u16 }>;

/// Marks the node whose params the "UI" draws.
#[derive(Component)]
struct Strip;

/// Everything a fader widget needs for one parameter.
///
/// The shape this example exists to pin down: what a widget actually reads, and
/// from where. `authored` comes from the ECS component the UI writes; `live`
/// and `range` come from the modulation accumulator. An unmodulated param has
/// no accumulator, so `live` falls back to `authored` and `range` to the
/// declared range — the widget needs no branch of its own.
struct FaderView {
    authored: f32,
    live: f32,
    range: (f32, f32),
    modulated: bool,
}

impl FaderView {
    /// Read one param the way a widget would.
    fn read<U: Unit<Raw = f32>>(
        matrix: &ModulationMatrix,
        entity: Entity,
        addr: ParamAddr,
        authored: U,
        declared_range: (f32, f32),
    ) -> Self {
        let authored = authored.to_raw();
        match matrix.target(entity, addr) {
            // `final_value` is the collapsed accumulator: base + Σ offsets. It
            // is sampled here at UI rate; the audio thread samples the same
            // accumulator at frame rate. Same state, different collapse rate —
            // neither has to know about the other.
            Some(target) => Self {
                authored,
                live: target.final_value(),
                range: target.range(),
                modulated: true,
            },
            None => Self {
                authored,
                live: authored,
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
        let handle = (self.normalized(self.authored) * WIDTH as f32) as usize;
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

    let mut net = Net::new(0, 1);
    let node = net.push(Box::new(DistortionNode::new(ShapeKind::Tanh, 1.0)));
    net.pipe_output(node);
    net.set_sample_rate(SampleRate(SAMPLE_RATE));

    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    // Stands in for a running device; nothing here opens one.
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>()
        .add_audio_param::<Hz, { UnitParam::Cutoff as u16 }>();

    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    let strip = app
        .world_mut()
        .spawn((
            Strip,
            AudioNode(node),
            DriveParam::new(Drive(5.0)),
            CutoffParam::new(Hz(2_000.0)),
            // Declaring the range is what makes a param modulatable at all.
            // Only `Drive` gets one here — `Cutoff` stays unmodulated.
            ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0),
        ))
        .id();

    let lfo = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModRate::free_running(Hz(2.0)),
        ))
        .id();
    // Depth scales the raw [-1, 1] LFO by the target's *span*, so 0.2 over a
    // 0..10 range swings ±2. Much more and the swing clips against the range
    // ends and the ghost simply parks there — correct, but it stops showing
    // the mechanism.
    app.world_mut()
        .spawn(ModRoute::new(lfo, strip, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)));

    println!("A modulated fader (|) and its LFO ghost (:), 10ms per line.\n");
    println!("{:<34} {:<8} {:<8}", "drive  0..10", "handle", "live");

    for tick in 0..TICKS {
        // Halfway through, the "user" drags the fader. The handle jumps and the
        // ghost follows it — modulation rides *on top of* the authored value
        // rather than replacing it, which is the whole point of routing an
        // authored write to the accumulator's base.
        if tick == TICKS / 2 {
            app.world_mut()
                .entity_mut(strip)
                .insert(DriveParam::new(Drive(3.0)));
            println!("{:-<52}  user drags fader to 3.0", "");
        }

        advance_transport(&mut app, FRAMES_PER_TICK);
        app.update();

        let world = app.world();
        let matrix = world.resource::<ModulationMatrix>();
        let authored = world.get::<DriveParam>(strip).unwrap().value;
        let view = FaderView::read(
            matrix,
            strip,
            ParamAddr::Unit(UnitParam::Drive),
            authored,
            (0.0, 10.0),
        );

        println!("{} {:>7.2} {:>8.2}", view.draw(), view.authored, view.live);
    }

    // The same widget code on a param nobody modulates: handle and ghost
    // coincide, and the view reports it so a UI can skip the ghost entirely.
    let world = app.world();
    let matrix = world.resource::<ModulationMatrix>();
    let cutoff = world.get::<CutoffParam>(strip).unwrap().value;
    let view = FaderView::read(
        matrix,
        strip,
        ParamAddr::Unit(UnitParam::Cutoff),
        cutoff,
        (20.0, 20_000.0),
    );
    println!(
        "\ncutoff (unmodulated): modulated={} handle={:.0}Hz live={:.0}Hz",
        view.modulated, view.authored, view.live
    );
}

/// Advance the transport as the audio clock would.
fn advance_transport(app: &mut App, frames: i64) {
    let transport = app.world().resource::<TransportRes>().clone();
    let now = transport.settings.steady_time();
    transport
        .settings
        .steady_time
        .store(now + frames, std::sync::atomic::Ordering::Relaxed);
}
