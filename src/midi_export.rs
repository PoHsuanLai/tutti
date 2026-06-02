//! Bridge between live MIDI receivers and the offline export pipeline.
//!
//! [`inject_snapshot_reader`] swaps every MIDI-consuming node's source
//! from its live [`tutti_midi_runtime::MidiReceiver`] to a
//! [`tutti_midi_runtime::MidiSnapshotReader`] for the duration of one
//! offline render. Use it while preparing a [`crate::export::GraphExport`]
//! terminal so MIDI-driven nodes read from the captured snapshot instead
//! of their live receivers.
//!
//! Lives in the umbrella `tutti` crate (rather than `tutti-export`)
//! because it has to downcast to the concrete node types declared across
//! `tutti-synth` and `tutti-plugin`.

use tutti_core::AudioUnit;

/// Replace the MIDI source on every PolySynth/SoundFontUnit/PluginClient
/// in `net` with a clone of `reader`.
pub fn inject_snapshot_reader(
    net: &mut tutti_core::dsp::Net,
    reader: &tutti_midi_runtime::MidiSnapshotReader,
) {
    let node_ids: Vec<_> = net.ids().copied().collect();
    for node_id in node_ids {
        let unit = net.node_mut(node_id);

        #[cfg(feature = "synth")]
        if let Some(synth) =
            <dyn AudioUnit>::as_any_mut(unit).downcast_mut::<tutti_synth::PolySynth>()
        {
            synth.set_midi_source(std::sync::Arc::new(reader.clone()));
            continue;
        }

        #[cfg(feature = "soundfont")]
        if let Some(sf_unit) =
            <dyn AudioUnit>::as_any_mut(unit).downcast_mut::<tutti_synth::SoundFontUnit>()
        {
            sf_unit.set_midi_source(std::sync::Arc::new(reader.clone()));
            continue;
        }
    }
}
