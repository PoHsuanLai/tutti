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

use tutti_graph::Harmony;
use tutti_midi_runtime::{HarmonyNode, TimedHarmony};
use tutti_plugin::catalog::{Plugin, PluginRole};
use tutti_plugin::handles::{LfoCurve, LfoShape, ParamAddress, ParamId, TimedParam};
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

    // Chords and scales are a node of their own too, wired to the plugin's
    // event input: `takes_harmony` answers whether the plugin will read them
    // (it declared sequencer context). C major, the C major scale.
    let took_harmony = plugin.takes_harmony();
    let _harmony = HarmonyNode::new([
        TimedHarmony::new(Beat(0.0), Harmony::chord(60, 60, 0b0000_1001_0001)),
        TimedHarmony::new(Beat(0.0), Harmony::scale(60, 0b1010_1011_0101)),
    ]);

    // Parameter automation is a node of its own, an event source to insert
    // beside the plugin and wire to its event input, so the graph's delay
    // compensation covers it. Ungated — every format carries it; `None` only
    // for a node with no event input (the in-process VST2 node). One slow LFO
    // on the plugin's first parameter.
    let automation = plugin.automation([TimedParam {
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
    }]);

    // MIDI travels on the node's event ports: a clip node, a keyboard's
    // queue node, a hardware input node wire to its event input.
    let takes_midi = plugin.takes_midi();

    report("transport", took_transport);
    report("harmony", took_harmony);
    report("automation", automation.is_some());
    report("midi in", takes_midi);

    if plugin.role() == PluginRole::Instrument && !takes_midi {
        // Worth saying out loud: a synth that takes no MIDI will never sound.
        // A *declined harmony* is unremarkable — most formats have no such
        // concept — which is why the answers are per-input rather than one
        // pass/fail for the whole wiring.
        eprintln!("warning: this reports as an instrument but declined MIDI input");
    }

    // 3. Hand over the node. Consuming and explicit: a plugin *has* a node, it
    //    is not one — its audio ports count buses only, and would describe a
    //    synth as taking nothing while it consumes MIDI every block. `Plugin`
    //    is itself an `IntoNode`: `editor.insert(key, kind, plugin)` puts it in
    //    a graph and hands back its controls.
    let (node, _controls) = tutti_graph::IntoNode::into_node(plugin);
    let shape = node.shape();
    println!(
        "\nnode ready: {} audio in, {} audio out",
        shape.audio_in.count(),
        shape.audio_out.count()
    );

    Ok(())
}

fn report(name: &str, accepted: bool) {
    println!(
        "  {name:<10} {}",
        if accepted { "wired" } else { "declined" }
    );
}
