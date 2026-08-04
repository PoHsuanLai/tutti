//! Wire every per-block input a plugin can take, then hand the node to a graph.
//!
//! Run with a plugin path:
//!
//! ```text
//! cargo run -p tutti-plugin --example wire_all_inputs -- /path/to/Foo.vst3
//! ```
//!
//! The point is not the audio — nothing is rendered here — it is that one
//! wiring sequence serves every format and every process boundary. A VST3
//! instrument accepts all four inputs; a VST2 accepts MIDI and transport and
//! declines the rest; an `aufx` effect declines MIDI too. Nothing below
//! branches on which.
//!
//! Each installer answers whether the plugin took the input, so a host can log
//! what a plugin will and will not receive *at wiring time* rather than
//! wondering later why a stream had no effect.

use std::sync::Arc;

use tutti_plugin::catalog::{Plugin, PluginRole};
use tutti_plugin::handles::{
    ChordValue, LfoCurve, LfoShape, ParamAddress, ParamId, ScaleValue, TimedChord, TimedParam,
    TimedScale,
};
use tutti_plugin::Result;

use tutti_core::transport::Transport;
use tutti_core::{Beat, BeatDuration, Depth, PhaseIncrement, RtPublish, SampleRate};

const SAMPLE_RATE: SampleRate = SampleRate::SR_48K;

fn main() -> Result<()> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: wire_all_inputs <plugin-path>");
        eprintln!("  e.g. /Library/Audio/Plug-Ins/VST3/Foo.vst3");
        std::process::exit(2);
    };

    // 1. Open. A path, not a catalog entry — the catalog is for discovery, and
    //    this host already knows what it wants.
    let mut plugin = Plugin::open(&path, SAMPLE_RATE)?;

    println!("opened {:?}", plugin.descriptor().name);
    println!("  format {}", plugin.descriptor().class.format_name());
    println!("  role   {:?}", plugin.role());

    // 2. Wire. The host offers everything it has; the plugin takes what it can
    //    use. `role` above answers *where this belongs* (a browser bucket, a
    //    track type); the answers below are *what it will receive*, and the two
    //    are deliberately independent — an arpeggiator takes MIDI and is not an
    //    instrument.
    let transport = Transport::new(SAMPLE_RATE);
    let meter = Arc::new(RtPublish::new(tutti_core::meter::MeterMap::default()));

    let took_transport = plugin.set_transport_source(transport.clone(), Arc::clone(&meter));

    let took_harmony = plugin.set_harmony_source(
        [TimedChord {
            beat: Beat(0.0),
            value: ChordValue {
                sample_offset: 0,
                root: 0,
                bass_note: 0,
                // C major triad, as a 12-bit degree mask.
                mask: 0b0000_1001_0001,
                text: "Cmaj".to_string(),
            },
        }],
        [TimedScale {
            beat: Beat(0.0),
            value: ScaleValue {
                sample_offset: 0,
                root: 0,
                mask: 0b1010_1101_0101,
                text: "C major".to_string(),
            },
        }],
        transport.clone(),
    );

    // Parameter automation is ungated — every format carries it, so there is
    // no "declined" answer and the call returns nothing. One slow LFO on the
    // plugin's first parameter.
    plugin.set_param_automation_source(
        [TimedParam {
            param_id: ParamAddress::Opaque(ParamId::new(0)),
            curve: Arc::new(LfoCurve::new(
                LfoShape::Sine,
                BeatDuration(4.0),
                Depth(1.0),
                PhaseIncrement(0.0),
                0.5, // base
                0.0, // min
                1.0, // max
            )),
        }],
        transport.clone(),
    );

    // MIDI has two paths that coexist: a live sender for hardware and panel
    // previews, and an installed source polled per block for clip playback.
    // Both are gated on the same capability.
    let midi_sender = plugin.midi_sender();

    report("transport", took_transport);
    report("harmony", took_harmony);
    report("midi in", midi_sender.is_some());

    if plugin.role() == PluginRole::Instrument && midi_sender.is_none() {
        // Worth saying out loud: a synth that takes no MIDI will never sound.
        // A *declined harmony* is unremarkable — most formats have no such
        // concept — which is why the answers are per-input rather than one
        // pass/fail for the whole wiring.
        eprintln!("warning: this reports as an instrument but declined MIDI input");
    }

    // 3. Hand over the node. Consuming and explicit: a plugin *has* a node, it
    //    is not one — `inputs()` counts audio buses only, and would describe a
    //    synth as taking nothing while it consumes MIDI every block.
    let unit = plugin.into_unit();
    println!(
        "\nnode ready: {} audio in, {} audio out",
        unit.inputs(),
        unit.outputs()
    );

    Ok(())
}

fn report(name: &str, accepted: bool) {
    println!(
        "  {name:<10} {}",
        if accepted { "wired" } else { "declined" }
    );
}
