//! Integration tests against a real VST2 plugin.
//!
//! These are `#[ignore]`'d by default — install the test plugin and run
//! `cargo test -p vst2-host -- --ignored` to exercise them. CI does not
//! ship plugins.

use std::path::Path;
use std::sync::Mutex;

use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_vst2_host::{
    ChannelLayout, MidiEvent, ProcessContext, RenderScratch, TransportInfo, Vst2Instance,
};

const VST2_PLUGIN: &str = "/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst";

/// Serialize plugin loads — loading the same .vst from parallel test
/// threads can race the plugin's static-init code (some plugins crash).
static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

fn load() -> Vst2Instance {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    Vst2Instance::load(Path::new(VST2_PLUGIN), 44_100.0, 512)
        .expect("Failed to load VST2 plugin (is TAL-NoiseMaker installed?)")
}

fn render_block(
    instance: &mut Vst2Instance,
    scratch: &mut RenderScratch,
    midi: &[MidiEvent],
) -> Vec<Vec<f32>> {
    let num_samples = 512;
    let input_data = vec![vec![0.0f32; num_samples]; 2];
    let mut output_data = vec![vec![0.0f32; num_samples]; 2];

    let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f32]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let ctx = ProcessContext::new(44_100.0).midi(midi);
    let _midi_out = instance.process_f32(
        &input_slices,
        &mut output_slices,
        num_samples,
        &ctx,
        scratch,
    );

    output_data
}

#[test]
#[ignore]
fn load_and_metadata() {
    let instance = load();
    let meta = instance.metadata();
    assert!(!meta.name.is_empty());
    assert!(!meta.id.is_empty());
    assert!(meta.num_outputs.count() > 0);
}

#[test]
#[ignore]
fn parameter_count_nonzero() {
    let instance = load();
    let params = instance.parameters();
    assert!(!params.is_empty(), "TAL-NoiseMaker should have parameters");
    for param in &params {
        assert!(!param.name.is_empty(), "param {} has empty name", param.id);
    }
}

#[test]
#[ignore]
fn parameter_set_get_roundtrip() {
    let instance = load();
    instance.set_parameter(0, 0.5);
    assert!((instance.parameter(0) - 0.5).abs() < 0.01);
    instance.set_parameter(0, 0.0);
    assert!(instance.parameter(0).abs() < 0.01);
    instance.set_parameter(0, 1.0);
    assert!((instance.parameter(0) - 1.0).abs() < 0.01);
}

#[test]
#[ignore]
fn parameter_info_lookup() {
    let instance = load();
    let info = instance.parameter_info(0).expect("param 0 should exist");
    assert_eq!(info.id, 0);
    assert!(!info.name.is_empty());
    assert!(instance.parameter_info(99_999).is_none());
}

#[test]
#[ignore]
fn process_silence_then_note() {
    let mut instance = load();
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, 512);

    // Silence first — TAL-NoiseMaker emits very small denormal-protection
    // values (~1e-10), so allow a tiny epsilon rather than strict zero.
    let silent = render_block(&mut instance, &mut scratch, &[]);
    for ch in &silent {
        for &s in ch {
            assert!(
                s.abs() < 1e-6,
                "expected near-silence before NoteOn, got {s}"
            );
        }
    }

    // NoteOn
    let note_on = [MidiEvent::note_on(0, 1, 60, midi1_velocity_to_midi2(100))];
    render_block(&mut instance, &mut scratch, &note_on);

    // Process more blocks until we hear the synth ramp up
    let mut has_nonzero = false;
    for _ in 0..4 {
        let out = render_block(&mut instance, &mut scratch, &[]);
        for ch in &out {
            for &s in ch {
                if s != 0.0 {
                    has_nonzero = true;
                }
            }
        }
    }
    assert!(has_nonzero, "synth should produce sound after NoteOn");
}

#[test]
#[ignore]
fn note_on_off_lifecycle_decays() {
    let mut instance = load();
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, 512);

    let note_on = [MidiEvent::note_on(0, 1, 60, midi1_velocity_to_midi2(100))];
    render_block(&mut instance, &mut scratch, &note_on);
    for _ in 0..4 {
        render_block(&mut instance, &mut scratch, &[]);
    }
    let note_off = [MidiEvent::note_off(0, 1, 60, 0)];
    render_block(&mut instance, &mut scratch, &note_off);

    // Drain release tail; final RMS should be small.
    let mut energy = 0.0f64;
    for _ in 0..20 {
        let out = render_block(&mut instance, &mut scratch, &[]);
        for ch in &out {
            for &s in ch {
                energy += (s as f64) * (s as f64);
            }
        }
    }
    let rms = (energy / (20.0 * 512.0 * 2.0)).sqrt();
    assert!(rms < 0.5, "expected low energy after NoteOff, got {rms}");
}

#[test]
#[ignore]
fn state_save_restore_roundtrip() {
    let instance = load();
    instance.set_parameter(0, 0.25);
    instance.set_parameter(1, 0.75);

    let state = instance.save_state().expect("save_state should succeed");
    assert!(!state.is_empty());

    instance.set_parameter(0, 0.9);
    instance.set_parameter(1, 0.1);

    instance
        .load_state(&state)
        .expect("load_state should succeed");

    assert!((instance.parameter(0) - 0.25).abs() < 0.02);
    assert!((instance.parameter(1) - 0.75).abs() < 0.02);
}

#[test]
#[ignore]
fn state_restore_invalid() {
    let instance = load();
    assert!(instance.load_state(&[]).is_err());
    assert!(instance.load_state(&[0, 1, 2]).is_err());
    assert!(instance.load_state(&[0xFF; 4]).is_err());

    let mut bad = Vec::new();
    bad.extend_from_slice(b"PRM\0");
    bad.extend_from_slice(&2i32.to_le_bytes()); // claims 2 params
    bad.extend_from_slice(&0.5f32.to_le_bytes()); // only 1 value
    assert!(instance.load_state(&bad).is_err());
}

#[test]
#[ignore]
fn process_with_transport() {
    let mut instance = load();
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, 512);

    let mut transport = TransportInfo::new()
        .with_playing(true)
        .with_position_quarters(2.0, 44_100)
        .with_tempo(120.0)
        .with_time_signature(4, 4);
    transport.loop_region.end_quarters = 4.0;

    let num_samples = 512;
    let input_data = vec![vec![0.0f32; num_samples]; (meta.num_inputs.count() as usize).max(1)];
    let mut output_data = vec![vec![0.0f32; num_samples]; meta.num_outputs.count() as usize];

    let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f32]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let ctx = ProcessContext::new(44_100.0).transport(&transport);
    instance.process_f32(
        &input_slices,
        &mut output_slices,
        num_samples,
        &ctx,
        &mut scratch,
    );
}

#[test]
#[ignore]
fn process_empty_buffer() {
    let mut instance = load();
    let mut scratch = RenderScratch::new(
        ChannelLayout::from(0u16),
        ChannelLayout::from(0u16),
        512,
    );
    let input_slices: Vec<&[f32]> = vec![];
    let mut output_slices: Vec<&mut [f32]> = vec![];
    let ctx = ProcessContext::new(44_100.0);
    instance.process_f32(&input_slices, &mut output_slices, 0, &ctx, &mut scratch);
}

#[test]
#[ignore]
fn sample_rate_change() {
    let mut instance = load();
    instance.set_sample_rate(48_000.0);
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, 512);
    render_block(&mut instance, &mut scratch, &[]);
}

#[test]
#[ignore]
fn has_editor() {
    let instance = load();
    assert!(
        instance.metadata().has_editor,
        "TAL-NoiseMaker has an editor"
    );
}

#[test]
#[ignore]
fn many_midi_events_in_one_block() {
    let mut instance = load();
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, 512);

    let events: Vec<MidiEvent> = (0..10u32)
        .map(|i| {
            MidiEvent::note_on(0, 1, 60 + i as u8, midi1_velocity_to_midi2(100))
                .with_frame_offset(i)
        })
        .collect();
    render_block(&mut instance, &mut scratch, &events);

    let note_offs: Vec<MidiEvent> = (0..10)
        .map(|i| MidiEvent::note_off(0, 1, 60 + i as u8, 0))
        .collect();
    render_block(&mut instance, &mut scratch, &note_offs);
}

#[test]
fn load_nonexistent_path() {
    // No #[ignore] — doesn't need a real plugin.
    let result = Vst2Instance::load(Path::new("/nonexistent/plugin.vst"), 44_100.0, 512);
    assert!(result.is_err());
}
