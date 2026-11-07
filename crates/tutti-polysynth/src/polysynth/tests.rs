use super::*;
use crate::synth_voice::SynthVoice;
use crate::{
    Adsr, FilterType, OscillatorType, PortamentoConfig, PortamentoCurve, PortamentoMode,
    SynthConfig, UnisonConfig, VoiceMode,
};
use tutti_midi_types::cc;
use tutti_midi_types::convert::{
    bend_u32_to_signed_f32, midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
};
use tutti_types::{Amplitude, Hz, Resonance, Seconds, Spread};
use tutti_ump::{
    Address, Attribute, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, U14, Ump,
    Voice1, Voice2,
};

/// A note number that is in range.
fn note_number(n: u8) -> NoteNumber {
    NoteNumber::new(n).expect("a 7-bit note")
}

/// A synth driven by hand the way a graph drives it
/// (`tutti_graph::contract::drive_input`): what these tests queue with
/// [`queue_midi`] is the next block's event input. It derefs to the synth.
///
/// - [`tick`](Hand::tick) is a one-frame block: every queued event lands
///   on its frame.
/// - [`render_planar`](Hand::render_planar) is one block of `left.len()`
///   frames: each queued event on its `frame_offset`, and one past the
///   block applied after it (as the renderer applies an event at or past
///   the block's end).
struct Hand {
    synth: PolySynth,
    queued: Vec<MidiEvent>,
}

impl std::ops::Deref for Hand {
    type Target = PolySynth;
    fn deref(&self) -> &PolySynth {
        &self.synth
    }
}

impl std::ops::DerefMut for Hand {
    fn deref_mut(&mut self) -> &mut PolySynth {
        &mut self.synth
    }
}

impl Hand {
    fn new(synth: PolySynth) -> Self {
        Self {
            synth,
            queued: Vec::new(),
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        let mut queued = std::mem::take(&mut self.queued);
        for e in &mut queued {
            e.frame_offset = 0;
        }
        let [l, r] = self.drive(1, queued);
        output[0] = l[0];
        if output.len() > 1 {
            output[1] = r[0];
        }
    }

    fn render_planar(&mut self, left: &mut [f32], right: &mut [f32]) {
        assert_eq!(left.len(), right.len());
        let queued = std::mem::take(&mut self.queued);
        let [l, r] = self.drive(left.len(), queued);
        left.copy_from_slice(&l);
        right.copy_from_slice(&r);
    }

    /// One block of `frames` under `queued` (stable-sorted by offset),
    /// through `Node::process`.
    fn drive(&mut self, frames: usize, mut queued: Vec<MidiEvent>) -> [Vec<f32>; 2] {
        queued.sort_by_key(|e| e.frame_offset);
        let block = tutti_types::Samples(frames);
        let (inside, past): (Vec<MidiEvent>, Vec<MidiEvent>) = queued
            .into_iter()
            .partition(|e| (e.frame_offset as usize) < frames);
        let events: Vec<tutti_graph::Event> = inside
            .iter()
            .map(|e| {
                let at = tutti_graph::Offset::new(e.frame_offset as usize, block)
                    .expect("inside the block");
                tutti_graph::Event::midi(at, e.data)
            })
            .collect();
        let env = tutti_graph::Env {
            frame: tutti_types::Frame(0),
            sample_rate: self.synth.config.sample_rate,
            block_len: block,
            transport: tutti_graph::Transport::default(),
            changes: tutti_graph::TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        };
        let out =
            tutti_graph::contract::drive_input(&mut self.synth, &env, &[], &[], &[&events]).audio;
        for e in &past {
            self.synth.apply_midi(Ump(e.data));
        }
        out.try_into().expect("stereo")
    }
}

/// Build a hand-driven synth from a config, unwrapping the result.
fn synth(config: SynthConfig) -> Hand {
    Hand::new(PolySynth::new(config).expect("synth builds"))
}

/// A long block renders every frame it was asked for.
///
/// A graph block is as long as the graph was prepared for, so this renders
/// four 512-frame blocks, each read in full.
/// `a_long_block_renders_every_frame_like_short_blocks` covers longer.
#[test]
fn a_full_block_renders_every_frame() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        // Near-instant attack, so the note is audible within the first block
        // rather than still ramping up from silence.
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.5),
        },
        ..Default::default()
    });
    queue_midi(&mut synth, &[ev_note_on(0, 69, 100)]);

    // Several blocks, not one: the voice's own smoothing ramps mean the
    // first frames after a note-on are legitimately near-silent.
    let mut peak = 0.0f32;
    let (mut left, mut right) = (vec![0.0f32; 512], vec![0.0f32; 512]);
    for _ in 0..4 {
        synth.render_planar(&mut left, &mut right);
        peak = left.iter().fold(peak, |a, s| a.max(s.abs()));
    }
    assert!(
        left[448..].iter().any(|&s| s != 0.0),
        "the block's last frames were rendered"
    );

    assert!(
        peak > 0.0,
        "a held note must produce audio across the blocks"
    );
    assert_eq!(
        synth.active_voice_count(),
        1,
        "the note is still held after rendering"
    );
}

/// Queue events for the synth's next hand-driven block ([`Hand`]).
fn queue_midi(synth: &mut Hand, events: &[MidiEvent]) {
    synth.queued.extend_from_slice(events);
}

// --- Test event builders (MIDI 1.0 7-bit values upconverted to UMP CV2) ---

fn ev_note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
    MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        note,
        midi1_velocity_to_midi2(vel),
    )
}

fn ev_note_off(channel: u8, note: u8) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::of(channel), note, 0)
}

fn ev_cc(channel: u8, cc_num: CCNumber, value: u8) -> MidiEvent {
    MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        cc_num,
        midi1_cc_to_midi2(value),
    )
}

fn ev_bend(channel: u8, bend14: u16) -> MidiEvent {
    MidiEvent::pitch_bend(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        midi1_pitch_bend_to_midi2(bend14),
    )
}

fn ev_per_note_management(channel: u8, note: u8, detach: bool, reset: bool) -> MidiEvent {
    MidiEvent::per_note_management(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        note,
        detach,
        reset,
    )
}

fn ev_aftertouch(channel: u8, pressure: u8) -> MidiEvent {
    MidiEvent::channel_pressure(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        midi1_cc_to_midi2(pressure),
    )
}

/// **A fork carries no sounding note**: a note playing on the live synth
/// is not in its fork, which plays only what its own event input brings.
///
/// Mutation (run): `fork_instance` not resetting the voices → the fork
/// sounds the live note → fails.
#[test]
fn a_fork_carries_no_sounding_note() {
    let mut live = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        ..Default::default()
    });
    queue_midi(&mut live, &[ev_note_on(0, 60, 100)]);
    let mut out = [0.0f32; 2];
    live.tick(&[], &mut out);
    assert_eq!(live.active_voice_count(), 1, "the original played its note");

    let mut render = Hand::new(live.fork_instance());
    assert_eq!(render.active_voice_count(), 0, "the fork holds no voice");
    let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
    render.render_planar(&mut l, &mut r);
    assert!(l.iter().all(|&s| s == 0.0), "and sounds nothing");
}

/// A clone's voice state is its own. If a voice held its gate, pitch or
/// filter state in shared cells, `clone()` would alias them, and a worker
/// rendering the clone would overwrite what the live voice reads. Driving
/// the clone's voice must leave the live voice's gate untouched.
#[test]
fn a_clone_unaliases_voice_state() {
    let mut live = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        ..Default::default()
    });

    // Activate a voice on the live synth and tick so its gate Shared = 1.0.
    queue_midi(&mut live, &[ev_note_on(0, 60, 100)]);
    let mut out = [0.0f32; 2];
    live.tick(&[], &mut out);
    assert_eq!(live.active_voice_count(), 1);
    let live_gate_before = live.voices[0].gate_value();
    assert_eq!(live_gate_before, 1.0, "live voice gate should be open");

    // A clone (a fork's template), then a note-off driven through the
    // clone's *own* machinery: gate the clone's voice shut.
    let mut render = Hand::new(live.clone());
    // A clone that still ALIASED the voice Shared would, by note_off on its
    // voice, also slam the live voice's gate to 0.0.
    if let Some(v) = render.voices.get_mut(0) {
        v.note_off();
    }
    render.tick(&[], &mut out);

    // The live voice's gate must be untouched by the clone's note_off.
    assert_eq!(
        live.voices[0].gate_value(),
        1.0,
        "live voice gate must stay open — clone's voice Shared is unaliased"
    );
}

#[test]
fn test_unison_creates_subvoices() {
    // Create synth with 3-voice unison
    let synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Saw,
        unison: Some(UnisonConfig {
            voice_count: 3,
            detune_cents: tutti_types::Cents(15.0),
            stereo_spread: Spread(0.5),
            phase_randomize: false,
        }),
        ..Default::default()
    });

    // Each voice should have 3 sub-voices
    assert_eq!(synth.voices[0].sub_voice_count(), 3);
    assert_eq!(synth.voices[1].sub_voice_count(), 3);

    // Unison engine should be present
    assert!(synth.unison.is_some());
}

/// `synth`'s param `p` set through its `ParamSet`, as a host writes a
/// knob (`AudioParam`, `write_param`) and as a control lane's knob is
/// the same cell: `false` for a param the set lacks. (These tests
/// pinned the synth's `ModParams` impl and then an `AtomicTarget` over
/// the set's cell; doc 014 §10 deleted both, and the `ParamSet` is the
/// one address.)
fn set(synth: &PolySynth, p: tutti_types::UnitParam, value: f32) -> bool {
    tutti_graph::ParamNode::param_set(synth).set(p, value)
}

/// Mutation (run): `param_set` hands `Volume` a fresh cell instead of
/// the master atomic → the atomic stays 1.0 → fails.
#[test]
fn a_volume_write_moves_the_master_atomic() {
    use tutti_types::UnitParam;

    let synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 2,
        ..Default::default()
    });
    let vol_atomic = synth.volume_atomic();
    assert!(set(&synth, UnitParam::Volume, 0.6), "volume is addressable");
    assert!(
        (vol_atomic.load(core::sync::atomic::Ordering::Acquire) - 0.6).abs() < 1e-4,
        "the synth's master-volume atomic reflects the write"
    );
    // (A foreign `ParamAddr::Id` is not expressible: a `ParamSet` is
    // addressed by `UnitParam`.)
}

/// Mutation (run): `param_set` adds `Detune` with unison absent → the
/// last assertion fails.
#[test]
fn detune_is_addressable_with_unison_absent_without() {
    use tutti_types::UnitParam;

    let with_unison = synth(SynthConfig {
        unison: Some(UnisonConfig {
            voice_count: 3,
            detune_cents: tutti_types::Cents(10.0),
            stereo_spread: Spread(0.5),
            phase_randomize: false,
        }),
        ..Default::default()
    });
    assert!(set(&with_unison, UnitParam::Detune, 10.0));
    assert!(set(&with_unison, UnitParam::StereoSpread, 0.5));

    let no_unison = synth(SynthConfig::default());
    assert!(!set(&no_unison, UnitParam::Detune, 0.0));
}

/// Mutation (run): `sync_from_atomics` skips the unison recompute →
/// the ratio stays 1.0 → fails.
#[test]
fn a_detune_write_recomputes_unison_on_process() {
    use tutti_types::UnitParam;

    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 2,
        oscillator: OscillatorType::Saw,
        unison: Some(UnisonConfig {
            voice_count: 3,
            detune_cents: tutti_types::Cents(0.0),
            stereo_spread: Spread(0.0),
            phase_randomize: false,
        }),
        ..Default::default()
    });

    // Detune starts at 0 → all sub-voices at unity freq ratio.
    // Index 0 is an edge sub-voice (position -1), so detune actually moves it
    // (the center voice at index 1 stays at unity by construction).
    let ratio_before = synth.unison.as_ref().unwrap().voice_params(0).freq_ratio;
    assert!((ratio_before - 1.0).abs() < 1e-4, "no detune yet");

    // Write the detune cell, then run a block: `sync_from_atomics` must
    // fold it into a recompute.
    assert!(set(&synth, UnitParam::Detune, 30.0), "detune addressable");

    let mut out = [0.0f32; 64];
    synth.tick(&[], &mut out);

    let ratio_after = synth.unison.as_ref().unwrap().voice_params(0).freq_ratio;
    assert!(
        (ratio_after - 1.0).abs() > 1e-4,
        "the detune write reached the unison voice params after a block \
         (before={ratio_before}, after={ratio_after})"
    );
}

#[test]
fn test_unison_stereo_output() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 2,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Saw,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.1),
            sustain: Amplitude(0.8),
            release: Seconds(0.1),
        }, // Fast attack to get output quickly
        unison: Some(UnisonConfig {
            voice_count: 3,
            detune_cents: tutti_types::Cents(15.0),
            stereo_spread: Spread(1.0), // Full stereo spread
            phase_randomize: false,
        }),
        ..Default::default()
    });

    // Verify unison is set up
    assert!(synth.unison.is_some());
    assert_eq!(synth.voices[0].sub_voice_count(), 3);

    // Trigger a note via registry
    let note_on = ev_note_on(0, 60, 100);
    queue_midi(&mut synth, &[note_on]);

    // Process samples and accumulate max output
    // Note: the envelope is evaluated once per control step (at most
    // `CONTROL_BLOCK` frames) and ramps from zero on its attack, so several
    // hundred samples are rendered before the output is judged
    let mut output = [0.0f32; 2];
    let mut max_left = 0.0f32;
    let mut max_right = 0.0f32;

    for _ in 0..2000 {
        synth.tick(&[], &mut output);
        max_left = max_left.max(output[0].abs());
        max_right = max_right.max(output[1].abs());
    }

    // With full stereo spread, both channels should carry output
    // (the left/right sub-voices should be panned to opposite sides)
    assert!(
        max_left > 0.0,
        "Expected non-zero left channel output, got {}",
        max_left
    );
    assert!(
        max_right > 0.0,
        "Expected non-zero right channel output, got {}",
        max_right
    );
}

#[test]
fn test_no_unison_single_subvoice() {
    // Create synth without unison
    let synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        ..Default::default()
    });

    // Each voice should have 1 sub-voice (no unison)
    assert_eq!(synth.voices[0].sub_voice_count(), 1);

    // Unison engine should be None
    assert!(synth.unison.is_none());
}

/// One sub-voice's chain — oscillator, envelope, gains — is silent while
/// its gate is closed and sounds once it opens.
///
/// *Mutation:* dropping `gate_on`'s `enter(Attack)` (so the gate never
/// opens the envelope) fails the second half.
#[test]
fn test_basic_dsp_chain() {
    use crate::bank::VoiceBank;
    use tutti_types::Phase;

    let mut bank = VoiceBank::new(
        OscillatorType::Saw,
        &FilterType::None,
        &Adsr::new(Seconds(0.001), Seconds(0.1), Amplitude(0.8), Seconds(0.1)),
        tutti_types::SampleRate(44100.0),
        1,
        1,
    );
    bank.set_pitch(0, Hz(440.0));
    bank.set_gains(0, Amplitude::UNITY, Amplitude::UNITY);
    bank.snap_gain(0);
    bank.start(0, Phase(0.3));

    // Gate closed: the oscillator runs, the envelope holds it at zero.
    let mut closed = 0.0f32;
    for _ in 0..32 {
        bank.mark_live(0);
        let (l, _) = bank.render(16);
        closed = l.iter().fold(closed, |a, s| a.max(s.abs()));
    }
    assert_eq!(closed, 0.0, "a closed gate must be exact silence");

    bank.gate_on(0);
    let mut max_out = 0.0f32;
    for _ in 0..32 {
        bank.mark_live(0);
        let (l, _) = bank.render(16);
        max_out = l.iter().fold(max_out, |a, s| a.max(s.abs()));
    }
    assert!(
        max_out > 0.1,
        "Chain should produce output after triggering gate, max={}",
        max_out
    );
}

#[test]
fn test_dynamic_unison_resize() {
    // Create synth with 2-voice unison
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 2,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Saw,
        unison: Some(UnisonConfig {
            voice_count: 2,
            detune_cents: tutti_types::Cents(10.0),
            stereo_spread: Spread(0.5),
            phase_randomize: false,
        }),
        ..Default::default()
    });

    // Initial state: 2 sub-voices per polyphonic voice
    assert_eq!(synth.voices[0].sub_voice_count(), 2);
    assert_eq!(synth.voices[1].sub_voice_count(), 2);
    assert_eq!(synth.unison_voice_count(), 2);

    // Increase to 5 unison voices
    synth.set_unison_voice_count(5);
    assert_eq!(synth.voices[0].sub_voice_count(), 5);
    assert_eq!(synth.voices[1].sub_voice_count(), 5);
    assert_eq!(synth.unison_voice_count(), 5);

    // Decrease to 3 unison voices
    synth.set_unison_voice_count(3);
    assert_eq!(synth.voices[0].sub_voice_count(), 3);
    assert_eq!(synth.voices[1].sub_voice_count(), 3);
    assert_eq!(synth.unison_voice_count(), 3);

    // Verify it still produces sound
    let note_on = ev_note_on(0, 60, 100);
    queue_midi(&mut synth, &[note_on]);

    let mut output = [0.0f32; 2];
    let mut max_out = 0.0f32;
    for _ in 0..2000 {
        synth.tick(&[], &mut output);
        max_out = max_out.max(output[0].abs().max(output[1].abs()));
    }

    assert!(
        max_out > 0.0,
        "Synth should produce output after resize, got max={}",
        max_out
    );
}

#[test]
fn test_note_off_respects_channel() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.1),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        ..Default::default()
    });

    // Play same note (C4=60) on channel 0 and channel 1
    // note_on(MidiGroup::of(frame_offset), MidiChannel::of(channel), note, velocity)
    let note_on_ch0 = ev_note_on(0, 60, 100);
    let note_on_ch1 = ev_note_on(1, 60, 100);
    queue_midi(&mut synth, &[note_on_ch0, note_on_ch1]);

    // Process to trigger both notes
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 2, "Should have 2 active voices");

    // Note off on channel 0 only
    // note_off(MidiGroup::of(frame_offset), MidiChannel::of(channel), note, velocity)
    let note_off_ch0 = ev_note_off(0, 60);
    queue_midi(&mut synth, &[note_off_ch0]);
    synth.tick(&[], &mut output);

    // Channel 1's voice should still be active (gate=1.0)
    // Channel 0's voice should be releasing (gate=0.0) but still active
    // until envelope finishes
    let mut ch1_still_gated = false;
    for voice in &synth.voices {
        if voice.is_active()
            && voice.channel() == MidiChannel::of(1)
            && voice.note() == note_number(60)
        {
            assert!(
                voice.gate_value() > 0.0,
                "Channel 1 voice should still have gate open"
            );
            ch1_still_gated = true;
        }
    }
    assert!(
        ch1_still_gated,
        "Channel 1 voice should still be active and gated"
    );
}

#[test]
fn test_cc64_sustain_pedal_holds_notes() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.05),
        },
        ..Default::default()
    });

    // Play note
    let note_on = ev_note_on(0, 60, 100);
    queue_midi(&mut synth, &[note_on]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 1);

    // Press sustain pedal (CC64 >= 64 = on)
    let sustain_on = ev_cc(0, cc::SUSTAIN, 127);
    queue_midi(&mut synth, &[sustain_on]);
    synth.tick(&[], &mut output);

    // Release note — voice should stay active (sustained)
    let note_off = ev_note_off(0, 60);
    queue_midi(&mut synth, &[note_off]);
    synth.tick(&[], &mut output);

    // Voice is still active due to sustain pedal
    assert!(
        synth.voices[0].gate_value() > 0.0 || synth.voices[0].is_active(),
        "Voice should still be held by sustain pedal"
    );

    // Release sustain pedal (CC64 < 64 = off)
    let sustain_off = ev_cc(0, cc::SUSTAIN, 0);
    queue_midi(&mut synth, &[sustain_off]);
    synth.tick(&[], &mut output);

    // Voice should now be releasing (gate off)
    let voice = &synth.voices[0];
    assert!(
        voice.gate_value() == 0.0,
        "Voice should release after sustain pedal off"
    );
}

#[test]
fn test_cc66_sostenuto_pedal_holds_notes() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.05),
        },
        ..Default::default()
    });

    // Play note, then press sostenuto
    let note_on = ev_note_on(0, 60, 100);
    queue_midi(&mut synth, &[note_on]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    let sostenuto_on = ev_cc(0, cc::SOSTENUTO, 127);
    queue_midi(&mut synth, &[sostenuto_on]);
    synth.tick(&[], &mut output);

    // Release note — should be held by sostenuto
    let note_off = ev_note_off(0, 60);
    queue_midi(&mut synth, &[note_off]);
    synth.tick(&[], &mut output);

    assert!(
        synth.voices[0].is_active(),
        "Voice should be held by sostenuto"
    );

    // Play a NEW note AFTER sostenuto — this should NOT be held
    let note_on2 = ev_note_on(0, 64, 100);
    queue_midi(&mut synth, &[note_on2]);
    synth.tick(&[], &mut output);
    let note_off2 = ev_note_off(0, 64);
    queue_midi(&mut synth, &[note_off2]);
    synth.tick(&[], &mut output);

    // Second note's voice should be releasing (gate off)
    let voice2 = &synth.voices[1];
    assert_eq!(
        voice2.gate_value(),
        0.0,
        "New note after sostenuto should release normally"
    );

    // Release sostenuto — original note should now release
    let sostenuto_off = ev_cc(0, cc::SOSTENUTO, 0);
    queue_midi(&mut synth, &[sostenuto_off]);
    synth.tick(&[], &mut output);

    assert_eq!(
        synth.voices[0].gate_value(),
        0.0,
        "Original note should release after sostenuto off"
    );
}

#[test]
fn test_cc120_all_sound_off() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(5.0),
        }, // Very long release
        ..Default::default()
    });

    // Play multiple notes
    let events: Vec<MidiEvent> = (60..64).map(|n| ev_note_on(0, n, 100)).collect();
    queue_midi(&mut synth, &events);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 4);

    // CC120 = All Sound Off (immediate silence)
    let all_sound_off = ev_cc(0, cc::ALL_SOUND_OFF, 0);
    queue_midi(&mut synth, &[all_sound_off]);
    synth.tick(&[], &mut output);

    // All voices should be immediately reset (not just releasing)
    assert_eq!(
        synth.active_voice_count(),
        0,
        "All Sound Off should immediately silence all voices"
    );
}

#[test]
fn test_cc123_all_notes_off() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(5.0),
        }, // Long release
        ..Default::default()
    });

    // Play multiple notes
    let events: Vec<MidiEvent> = (60..64).map(|n| ev_note_on(0, n, 100)).collect();
    queue_midi(&mut synth, &events);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 4);

    // CC123 = All Notes Off (release with envelope tail)
    let all_notes_off = ev_cc(0, cc::ALL_NOTES_OFF, 0);
    queue_midi(&mut synth, &[all_notes_off]);
    synth.tick(&[], &mut output);

    // Voices should still be active (releasing with long tail)
    // but gates should be off
    for voice in &synth.voices {
        if voice.is_active() {
            assert_eq!(
                voice.gate_value(),
                0.0,
                "All Notes Off should release (gate off), not silence"
            );
        }
    }
}

#[test]
fn test_cc123_respects_channel() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(5.0),
        },
        ..Default::default()
    });

    // Play notes on channel 0 and channel 1
    let note_ch0 = ev_note_on(0, 60, 100);
    let note_ch1 = ev_note_on(1, 64, 100);
    queue_midi(&mut synth, &[note_ch0, note_ch1]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 2);

    // All Notes Off on channel 0 only
    let all_notes_off = ev_cc(0, cc::ALL_NOTES_OFF, 0);
    queue_midi(&mut synth, &[all_notes_off]);
    synth.tick(&[], &mut output);

    // Channel 1 voice should still have gate open
    let ch1_voice = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.channel() == MidiChannel::of(1));
    assert!(
        ch1_voice.is_some(),
        "Channel 1 voice should still be active"
    );
    assert!(
        ch1_voice.unwrap().gate_value() > 0.0,
        "Channel 1 voice gate should still be open"
    );
}

#[test]
fn test_velocity_zero_note_on_is_note_off() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.05),
        },
        ..Default::default()
    });

    // Play note
    let note_on = ev_note_on(0, 60, 100);
    queue_midi(&mut synth, &[note_on]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 1);
    assert!(synth.voices[0].gate_value() > 0.0);

    // Note on with velocity 0 = note off (MIDI standard)
    let vel0_off = ev_note_on(0, 60, 0);
    queue_midi(&mut synth, &[vel0_off]);
    synth.tick(&[], &mut output);

    assert_eq!(
        synth.voices[0].gate_value(),
        0.0,
        "Note-on with velocity 0 should act as note-off"
    );
}

#[test]
fn test_legato_mode_no_retrigger() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 1,
        voice_mode: VoiceMode::Legato,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        ..Default::default()
    });

    assert_eq!(synth.config.voice_mode, VoiceMode::Legato);

    // First note triggers normally
    let note1 = ev_note_on(0, 60, 100);
    queue_midi(&mut synth, &[note1]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 1);
    assert_eq!(synth.voices[0].note(), note_number(60));

    // Second note should legato (update pitch, no retrigger)
    let note2 = ev_note_on(0, 64, 100);
    queue_midi(&mut synth, &[note2]);
    synth.tick(&[], &mut output);

    assert_eq!(
        synth.active_voice_count(),
        1,
        "Legato should use same voice"
    );
    assert_eq!(
        synth.voices[0].note(),
        note_number(64),
        "Voice should have new note"
    );
    assert!(
        synth.voices[0].gate_value() > 0.0,
        "Gate should stay open (no retrigger)"
    );
}

#[test]
fn test_mono_mode_retrigger() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 1,
        voice_mode: VoiceMode::Mono,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        ..Default::default()
    });

    assert_eq!(synth.config.voice_mode, VoiceMode::Mono);

    // First note
    let note1 = ev_note_on(0, 60, 100);
    queue_midi(&mut synth, &[note1]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.voices[0].note(), note_number(60));

    // Second note should retrigger (new allocation, not legato)
    let note2 = ev_note_on(0, 64, 100);
    queue_midi(&mut synth, &[note2]);
    synth.tick(&[], &mut output);
    assert_eq!(synth.voices[0].note(), note_number(64));
}

/// One wheel position means one pitch, whether the note was already
/// sounding or is struck after the bend.
///
/// It did not. `handle_note_on` multiplies the base frequency by
/// `2^(semitones/12)`, while `apply_pitch_bend` fed `note + semitones` into
/// `Tuning::fractional_note_to_freq`, which interpolates between *table*
/// entries. Under equal temperament the table is geometric and the two
/// agree exactly, which is why every existing test passed. Under any
/// unequal scale they do not: measured at **3.9 cents** on
/// `Tuning::just_intonation` — a held note and a new note, same wheel,
/// audibly different pitch.
///
/// Pinned in cents rather than Hz because the defect is a pitch error, and
/// because a ratio comparison stays meaningful if the fixture's reference
/// frequency ever changes.
#[test]
fn a_bend_lands_on_the_same_pitch_whether_the_note_was_already_sounding() {
    let config = || SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 2,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        // An unequal table is the whole point: on equal temperament the two
        // derivations coincide and this test cannot fail.
        tuning: crate::Tuning::just_intonation(),
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        ..Default::default()
    };
    // Full up-bend: +1 semitone at the default 2-semitone range.
    let bend_up = ev_bend(0, 16383);

    // Held: sound the note, then bend it.
    let mut held = synth(config());
    let mut out = [0.0f32; 2];
    queue_midi(&mut held, &[ev_note_on(0, 60, 100)]);
    held.tick(&[], &mut out);
    queue_midi(&mut held, &[bend_up]);
    held.tick(&[], &mut out);
    let held_freq = held.voices[0].sounding_freq();

    // Struck: bend first, then sound the note.
    let mut struck = synth(config());
    queue_midi(&mut struck, &[bend_up]);
    struck.tick(&[], &mut out);
    queue_midi(&mut struck, &[ev_note_on(0, 60, 100)]);
    struck.tick(&[], &mut out);
    let struck_freq = struck.voices[0].sounding_freq();

    let delta = tutti_types::Cents::from_pitch_ratio(held_freq.get() / struck_freq.get());
    assert!(
        delta.get().abs() < 0.5,
        "same wheel position, two pitches: held {held_freq:?} vs struck {struck_freq:?} \
         ({delta:?} apart)"
    );
}

#[test]
fn test_portamento_with_pitch_bend() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 2,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        portamento: Some(PortamentoConfig {
            mode: PortamentoMode::Always,
            curve: PortamentoCurve::Linear,
            time: tutti_types::Seconds(0.05), // 50ms glide
            constant_time: true,
        }),
        ..Default::default()
    });

    // Play first note to initialize portamento
    let note1 = ev_note_on(0, 60, 100);
    queue_midi(&mut synth, &[note1]);
    let mut output = [0.0f32; 2];
    for _ in 0..4410 {
        synth.tick(&[], &mut output);
    }

    // Play second note — triggers portamento glide
    let note2 = ev_note_on(0, 72, 100);
    queue_midi(&mut synth, &[note2]);
    synth.tick(&[], &mut output);

    // Apply pitch bend while portamento is gliding
    let bend_up = ev_bend(0, 16383);
    queue_midi(&mut synth, &[bend_up]);

    // Process samples during glide — should not crash or produce silence
    let mut max_out = 0.0f32;
    for _ in 0..2205 {
        synth.tick(&[], &mut output);
        max_out = max_out.max(output[0].abs().max(output[1].abs()));
    }

    assert!(
        max_out > 0.0,
        "Should produce audio during portamento+bend, got max={}",
        max_out
    );
}

#[test]
fn test_zero_voices_returns_error() {
    let result = PolySynth::new(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 0,
        ..Default::default()
    });
    assert!(result.is_err(), "max_voices=0 should return error");
}

#[test]
fn test_polysynth_reset() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(5.0),
        },
        ..Default::default()
    });

    // Play notes
    let events: Vec<MidiEvent> = (60..64).map(|n| ev_note_on(0, n, 100)).collect();
    queue_midi(&mut synth, &events);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 4);

    // Reset
    tutti_graph::Node::reset(&mut synth.synth);
    assert_eq!(
        synth.active_voice_count(),
        0,
        "Reset should clear all voices"
    );
}

#[test]
fn test_mpe_per_voice_pitch_bend() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        mpe_pitch_bend_range: tutti_types::Semitones(48.0),
        ..Default::default()
    });

    // Play note on channel 1 (MPE member channel)
    let note_on = ev_note_on(1, 60, 100);
    queue_midi(&mut synth, &[note_on]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 1);

    // Play note on channel 2
    let note_on2 = ev_note_on(2, 64, 100);
    queue_midi(&mut synth, &[note_on2]);
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 2);

    // A native per-note pitch bend addressed to the ch1 note (60) must affect
    // only that voice. Classic-MPE channel bends are rewritten to this native
    // form at the input edge (`MpeIngest`); the synth is zone-agnostic and only
    // ever sees native per-note messages.
    let bend =
        MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(1), 60, 0xFFFF_FFFF);
    queue_midi(&mut synth, &[bend]);
    synth.tick(&[], &mut output);

    let voice_ch1 = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.channel() == MidiChannel::of(1))
        .unwrap();
    let voice_ch2 = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.channel() == MidiChannel::of(2))
        .unwrap();

    assert!(
        voice_ch1.mpe_state().pitch_bend_semitones.get() > 40.0,
        "the addressed note should have a large pitch bend, got {}",
        voice_ch1.mpe_state().pitch_bend_semitones
    );
    assert!(
        voice_ch2.mpe_state().pitch_bend_semitones.get().abs() < 0.01,
        "the other note should be untouched, got {}",
        voice_ch2.mpe_state().pitch_bend_semitones
    );
}

#[test]
fn test_midi2_per_note_pitch_bend_addresses_one_voice() {
    // MIDI 2.0 native per-note pitch bend carries the note number on the
    // wire, so it must move ONLY the addressed voice — even when both notes
    // sound on the same channel (where the classic per-channel MPE handler
    // would have bent both). This is the per-note-addressing proof.
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        mpe_pitch_bend_range: tutti_types::Semitones(48.0),
        ..Default::default()
    });

    // Two notes, same channel, different pitches.
    queue_midi(
        &mut synth,
        &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)],
    );
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 2);

    // Native per-note pitch bend addressed to note 60 only (full positive).
    let bend =
        MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(1), 60, 0xFFFF_FFFF);
    queue_midi(&mut synth, &[bend]);
    synth.tick(&[], &mut output);

    let voice_60 = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.note() == note_number(60))
        .unwrap();
    let voice_64 = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.note() == note_number(64))
        .unwrap();

    assert!(
        voice_60.mpe_state().pitch_bend_semitones.get() > 40.0,
        "note 60 should be bent, got {}",
        voice_60.mpe_state().pitch_bend_semitones
    );
    assert!(
        voice_64.mpe_state().pitch_bend_semitones.get().abs() < 0.01,
        "note 64 (same channel) must be untouched, got {}",
        voice_64.mpe_state().pitch_bend_semitones
    );
}

#[test]
fn test_midi2_per_note_management_reset_zeroes_only_addressed_voice() {
    // MIDI 2.0 Per-Note Management {reset:true} must reset ONLY the addressed
    // note's per-note expression, leaving another sounding voice's bend intact.
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        mpe_pitch_bend_range: tutti_types::Semitones(48.0),
        ..Default::default()
    });

    // Two notes on the same channel.
    queue_midi(
        &mut synth,
        &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)],
    );
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 2);

    // Bend BOTH notes fully (per-note, so each is addressed independently).
    queue_midi(
        &mut synth,
        &[
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(1), 60, 0xFFFF_FFFF),
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(1), 64, 0xFFFF_FFFF),
        ],
    );
    synth.tick(&[], &mut output);

    let bent = |synth: &PolySynth, note: u8| {
        synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == note_number(note))
            .unwrap()
            .mpe_state()
            .pitch_bend_semitones
            .get()
    };
    assert!(bent(&synth, 60) > 40.0, "note 60 should start bent");
    assert!(bent(&synth, 64) > 40.0, "note 64 should start bent");

    // Per-Note Management Reset addressed to note 60 only.
    queue_midi(&mut synth, &[ev_per_note_management(1, 60, false, true)]);
    synth.tick(&[], &mut output);

    assert!(
        bent(&synth, 60).abs() < 0.01,
        "note 60 per-note expression must be reset to 0, got {}",
        bent(&synth, 60)
    );
    assert!(
        bent(&synth, 64) > 40.0,
        "note 64 (unaddressed) must keep its bend, got {}",
        bent(&synth, 64)
    );
}

#[test]
fn test_pitch_7_25_retunes_only_the_addressed_note() {
    // Registered Per-Note Controller #3: Pitch 7.25 (M2-104 §7.4.15.2) sets an
    // absolute pitch for the addressed note. Note 60 retuned to note 69.0
    // (A440) should sound at 440 Hz; another note is untouched.
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        ..Default::default()
    });

    queue_midi(
        &mut synth,
        &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)],
    );
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    // Pitch 7.25 for note 60 = 69.0 (A440). Q7.25: 69 << 25. Registered
    // per-note controller index 3 (Pitch 7.25).
    let pitch = MidiEvent::per_note_controller(
        MidiGroup::FIRST,
        MidiChannel::of(1),
        60,
        3,
        69u32 << 25,
        true,
    );
    queue_midi(&mut synth, &[pitch]);
    synth.tick(&[], &mut output);

    let freq = |synth: &PolySynth, note: u8| {
        synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == note_number(note))
            .unwrap()
            .base_note_freq()
            .get()
    };
    assert!(
        (freq(&synth, 60) - 440.0).abs() < 1.0,
        "note 60 retuned to A440, got {}",
        freq(&synth, 60)
    );
    // Note 64's default pitch (~329.6 Hz) must be untouched.
    assert!(
        (freq(&synth, 64) - 329.6).abs() < 2.0,
        "note 64 must keep its default pitch, got {}",
        freq(&synth, 64)
    );
}

#[test]
fn test_per_note_management_detach_freezes_the_note() {
    // Detach (D=1): the addressed voice keeps its current per-note bend but
    // stops responding to further per-note controllers (M2-104 §7.4.5).
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        mpe_pitch_bend_range: tutti_types::Semitones(48.0),
        ..Default::default()
    });

    queue_midi(&mut synth, &[ev_note_on(1, 60, 100)]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    // Bend fully, then Detach.
    queue_midi(
        &mut synth,
        &[MidiEvent::per_note_pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            0xFFFF_FFFF,
        )],
    );
    synth.tick(&[], &mut output);
    let bent = |synth: &PolySynth| {
        synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == note_number(60))
            .unwrap()
            .mpe_state()
            .pitch_bend_semitones
            .get()
    };
    let frozen_at = bent(&synth);
    assert!(frozen_at > 40.0, "note should be bent before detach");

    // Detach (D=1, S=0).
    queue_midi(&mut synth, &[ev_per_note_management(1, 60, true, false)]);
    synth.tick(&[], &mut output);

    // A further per-note bend to zero must be IGNORED — the note holds its value.
    queue_midi(
        &mut synth,
        &[MidiEvent::per_note_pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            0x8000_0000,
        )],
    );
    synth.tick(&[], &mut output);
    assert!(
        (bent(&synth) - frozen_at).abs() < 0.01,
        "detached note must ignore further per-note bend (held {frozen_at}, got {})",
        bent(&synth)
    );
}

#[test]
fn test_per_note_pitch_bend_sensitivity_rpn_updates_range() {
    // RPN #00/07 sets the per-note pitch-bend range (M2-104 §7.4.13). After a
    // 12-semitone sensitivity, a full per-note bend should reach ~12 semitones
    // (not the default 48).
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        mpe_pitch_bend_range: tutti_types::Semitones(48.0),
        ..Default::default()
    });

    // Sensitivity RPN: 12 semitones (7.25 fixed-point).
    let sens = tutti_midi_types::mpe::PitchBendSensitivity::of(12);
    let rpn = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(1),
        tutti_midi_types::ump::RPN_BANK_MPE,
        tutti_midi_types::ump::RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
        sens.to_rpn_bits(),
    );
    queue_midi(&mut synth, &[ev_note_on(1, 60, 100), rpn]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    // Full per-note bend up — now clamped to the 12-semitone range.
    queue_midi(
        &mut synth,
        &[MidiEvent::per_note_pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            0xFFFF_FFFF,
        )],
    );
    synth.tick(&[], &mut output);
    let bend = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.note() == note_number(60))
        .unwrap()
        .mpe_state()
        .pitch_bend_semitones
        .get();
    assert!(
        (bend - 12.0).abs() < 0.5,
        "per-note bend should clamp to the 12-semitone sensitivity, got {bend}"
    );
}

#[test]
fn test_reset_all_controllers_spares_per_note() {
    // Reset All Controllers (CC121) resets channel controllers + global pitch
    // bend but must NOT touch per-note controllers (M2-104 Appendix B.2).
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        mpe_pitch_bend_range: tutti_types::Semitones(48.0),
        ..Default::default()
    });

    queue_midi(&mut synth, &[ev_note_on(1, 60, 100)]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    // Set a per-note bend.
    queue_midi(
        &mut synth,
        &[MidiEvent::per_note_pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            0xFFFF_FFFF,
        )],
    );
    synth.tick(&[], &mut output);

    // Reset All Controllers on channel 1.
    queue_midi(&mut synth, &[ev_cc(1, cc::RESET_ALL, 0)]);
    synth.tick(&[], &mut output);

    let bend = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.note() == note_number(60))
        .unwrap()
        .mpe_state()
        .pitch_bend_semitones
        .get();
    assert!(
        bend > 40.0,
        "Reset All Controllers must NOT clear per-note bend, got {bend}"
    );
}

#[test]
fn test_midi2_registered_per_note_controller_brightness_sets_only_addressed_slide() {
    // A Registered Per-Note Controller for Brightness (CC74 / SoundController
    // index 5) addressed to note X must set ONLY note X's slide.
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Saw,
        filter: FilterType::Moog {
            cutoff: Hz(1000.0),
            resonance: Resonance(0.5),
        },
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        ..Default::default()
    });

    // Two notes on the same channel.
    queue_midi(
        &mut synth,
        &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)],
    );
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 2);

    // Registered per-note Brightness (index 74) addressed to note 60, full value.
    queue_midi(
        &mut synth,
        &[MidiEvent::per_note_controller(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            74,
            0xFFFF_FFFF,
            true,
        )],
    );
    synth.tick(&[], &mut output);

    let slide = |synth: &PolySynth, note: u8| {
        synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == note_number(note))
            .unwrap()
            .mpe_state()
            .slide
    };
    assert!(
        (slide(&synth, 60) - 1.0).abs() < 0.01,
        "note 60 slide should be full, got {}",
        slide(&synth, 60)
    );
    // The unaddressed voice must keep its neutral default (center = no timbre
    // shift), not be dragged along with note 60.
    assert!(
        (slide(&synth, 64) - crate::mpe::SLIDE_CENTER).abs() < 0.01,
        "note 64 (unaddressed) slide must stay at center, got {}",
        slide(&synth, 64)
    );
}

#[test]
fn test_midi2_per_note_gain_addresses_only_one_voice() {
    // Per-note Volume (CC7) — as clip Gain lanes emit it (Assignable index 7)
    // and as a Registered Volume controller — must reach the addressed voice
    // as gain, and leave others at unity.
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        ..Default::default()
    });

    queue_midi(
        &mut synth,
        &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)],
    );
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    let gain = |synth: &PolySynth, note: u8| {
        synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == note_number(note))
            .unwrap()
            .mpe_state()
            .gain
            .get()
    };
    // Fresh voices are at unity gain.
    assert!((gain(&synth, 60) - 1.0).abs() < 0.01);
    assert!((gain(&synth, 64) - 1.0).abs() < 0.01);

    // Assignable per-note CC7 (the clip Gain-lane encoding) → half gain on 60.
    queue_midi(
        &mut synth,
        &[MidiEvent::per_note_controller(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            7,
            0x8000_0000,
            false,
        )],
    );
    synth.tick(&[], &mut output);
    assert!(
        (gain(&synth, 60) - 0.5).abs() < 0.02,
        "note 60 gain should follow CC7, got {}",
        gain(&synth, 60)
    );
    assert!(
        (gain(&synth, 64) - 1.0).abs() < 0.01,
        "note 64 (unaddressed) gain must stay unity, got {}",
        gain(&synth, 64)
    );
}

#[test]
fn test_mpe_per_voice_pressure() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        ..Default::default()
    });

    // Play notes on two different channels
    let note1 = ev_note_on(1, 60, 100);
    let note2 = ev_note_on(2, 64, 100);
    queue_midi(&mut synth, &[note1, note2]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    // Native per-note pressure addressed to the ch1 note (60) only. Classic-MPE
    // channel pressure is rewritten to this form at the input edge.
    let pressure = MidiEvent::poly_pressure(MidiGroup::FIRST, MidiChannel::of(1), 60, 0xFFFF_FFFF);
    queue_midi(&mut synth, &[pressure]);
    synth.tick(&[], &mut output);

    let voice_ch1 = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.channel() == MidiChannel::of(1))
        .unwrap();
    let voice_ch2 = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.channel() == MidiChannel::of(2))
        .unwrap();

    assert!(
        (voice_ch1.mpe_state().pressure - 1.0).abs() < 0.01,
        "the addressed note should have full pressure"
    );
    assert!(
        voice_ch2.mpe_state().pressure.abs() < 0.01,
        "the other note should have no pressure"
    );
}

#[test]
fn test_mpe_per_voice_slide() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Saw,
        filter: FilterType::Moog {
            cutoff: Hz(1000.0),
            resonance: Resonance(0.5),
        },
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        ..Default::default()
    });

    // Play note on channel 1
    let note1 = ev_note_on(1, 60, 100);
    queue_midi(&mut synth, &[note1]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    // Native per-note CC74 (slide) addressed to the ch1 note (60). Classic-MPE
    // channel CC74 is rewritten to this form at the input edge.
    let slide = MidiEvent::per_note_controller(
        MidiGroup::FIRST,
        MidiChannel::of(1),
        60,
        74,
        0xFFFF_FFFF,
        false,
    );
    queue_midi(&mut synth, &[slide]);
    synth.tick(&[], &mut output);

    let voice = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.channel() == MidiChannel::of(1))
        .unwrap();
    assert!(
        (voice.mpe_state().slide - 1.0).abs() < 0.01,
        "the addressed note should have full slide"
    );
}

#[test]
fn test_mpe_disabled_global_pitch_bend() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        ..Default::default()
    });

    // MPE is disabled (default) - pitch bend should be global
    let note1 = ev_note_on(0, 60, 100);
    let note2 = ev_note_on(0, 64, 100);
    queue_midi(&mut synth, &[note1, note2]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    // Global pitch bend should NOT set MPE state
    let bend = ev_bend(0, 16383);
    queue_midi(&mut synth, &[bend]);
    synth.tick(&[], &mut output);

    // Both voices should have zero MPE pitch bend (global bend is handled differently)
    for voice in &synth.voices {
        if voice.is_active() {
            assert!(
                voice.mpe_state().pitch_bend_semitones.get().abs() < 0.01,
                "Non-MPE mode should not set MPE pitch bend"
            );
        }
    }
}

#[test]
fn test_mpe_pressure_affects_amplitude() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 1,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.1),
        },
        mpe_enabled: true,
        ..Default::default()
    });

    // Play note
    let note = ev_note_on(1, 60, 100);
    queue_midi(&mut synth, &[note]);

    // Render baseline with no pressure
    let mut output = [0.0f32; 2];
    let mut max_no_pressure = 0.0f32;
    for _ in 0..2000 {
        synth.tick(&[], &mut output);
        max_no_pressure = max_no_pressure.max(output[0].abs().max(output[1].abs()));
    }

    // Apply full pressure as a native per-note pressure on the note (60).
    let pressure = MidiEvent::poly_pressure(MidiGroup::FIRST, MidiChannel::of(1), 60, 0xFFFF_FFFF);
    queue_midi(&mut synth, &[pressure]);

    let mut max_with_pressure = 0.0f32;
    for _ in 0..2000 {
        synth.tick(&[], &mut output);
        max_with_pressure = max_with_pressure.max(output[0].abs().max(output[1].abs()));
    }

    assert!(
        max_with_pressure > max_no_pressure * 1.1,
        "Pressure should increase amplitude: no_pressure={}, with_pressure={}",
        max_no_pressure,
        max_with_pressure
    );
}

#[test]
fn test_mpe_note_on_resets_expression() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(0.05),
        },
        mpe_enabled: true,
        ..Default::default()
    });

    // Play note, set pressure, release, play again
    let note_on = ev_note_on(1, 60, 100);
    queue_midi(&mut synth, &[note_on]);
    let mut output = [0.0f32; 2];
    synth.tick(&[], &mut output);

    let pressure = ev_aftertouch(1, 127);
    queue_midi(&mut synth, &[pressure]);
    synth.tick(&[], &mut output);

    // Release and wait for voice to finish
    let note_off = ev_note_off(1, 60);
    queue_midi(&mut synth, &[note_off]);
    for _ in 0..10000 {
        synth.tick(&[], &mut output);
    }

    // Play again on channel 1
    let note_on2 = ev_note_on(1, 60, 100);
    queue_midi(&mut synth, &[note_on2]);
    synth.tick(&[], &mut output);

    let voice = synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.channel() == MidiChannel::of(1))
        .unwrap();
    assert!(
        voice.mpe_state().pressure.abs() < 0.01,
        "New note should have reset MPE pressure"
    );
}

/// Whether MPE is on must be *askable*.
///
/// It was construction-only with no getter, while the per-note setters accept
/// unconditionally and `apply_mpe_modulation` drops everything at render
/// time. Since the pitch-bend-sensitivity RPN *is* live either way, a host
/// could send an MCM, watch sensitivity apply, and reasonably conclude MPE
/// was on — with no way to find out otherwise.
#[test]
fn mpe_enabled_is_observable() {
    let off = PolySynth::new(SynthConfig {
        mpe_enabled: false,
        ..Default::default()
    })
    .unwrap();
    assert!(!off.mpe_enabled());

    let on = PolySynth::new(SynthConfig {
        mpe_enabled: true,
        ..Default::default()
    })
    .unwrap();
    assert!(on.mpe_enabled());
}

/// Enabling MPE at runtime reaches voices that are already sounding.
///
/// Per-note expression sent while disabled is dropped at render, so the
/// setter has to affect live voices for a mid-performance enable to mean
/// anything.
#[test]
fn enabling_mpe_applies_to_a_sounding_voice() {
    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            max_voices: 4,
            oscillator: OscillatorType::Sine,
            mpe_enabled: false,
            mpe_pitch_bend_range: tutti_types::Semitones(48.0),
            ..Default::default()
        })
        .unwrap(),
    );

    let mut output = [0.0f32; 2];
    queue_midi(&mut synth, &[ev_note_on(1, 60, 100)]);
    synth.tick(&[], &mut output);
    assert_eq!(synth.active_voice_count(), 1);

    synth.set_mpe_enabled(true);
    assert!(synth.mpe_enabled());
    assert!(
        synth
            .voices
            .iter()
            .find(|v| v.is_active())
            .unwrap()
            .mpe_enabled(),
        "the sounding voice must adopt the new mode, or a mid-performance \
         enable silently does nothing until the next note"
    );
}

/// Disabling MPE clears per-note state rather than merely ignoring it.
///
/// The setters store unconditionally, so expression received while disabled
/// would otherwise sit latent and snap into effect the instant MPE is turned
/// back on — a sounding note jumping in pitch from a bend it received much
/// earlier.
#[test]
fn disabling_mpe_clears_latent_per_note_state() {
    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            max_voices: 4,
            oscillator: OscillatorType::Sine,
            mpe_enabled: true,
            mpe_pitch_bend_range: tutti_types::Semitones(48.0),
            ..Default::default()
        })
        .unwrap(),
    );

    let mut output = [0.0f32; 2];
    queue_midi(&mut synth, &[ev_note_on(1, 60, 100)]);
    synth.tick(&[], &mut output);

    // Bend the sounding note well off centre.
    let bend =
        MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(1), 60, 0xFFFF_FFFF);
    queue_midi(&mut synth, &[bend]);
    synth.tick(&[], &mut output);
    assert!(
        synth
            .voices
            .iter()
            .find(|v| v.is_active())
            .unwrap()
            .mpe_state()
            .pitch_bend_semitones
            .get()
            .abs()
            > 0.01,
        "precondition: the bend applied while MPE was on"
    );

    synth.set_mpe_enabled(false);

    assert!(
        synth
            .voices
            .iter()
            .find(|v| v.is_active())
            .unwrap()
            .mpe_state()
            .pitch_bend_semitones
            .get()
            .abs()
            < 0.01,
        "per-note state survived being disabled — it would snap back into \
         effect on the next enable"
    );
}

/// A block far longer than 64 frames renders every frame, with
/// MIDI landing inside it past frame 64 — and is sample-identical to the
/// same audio rendered as eight 64-frame blocks.
///
/// The renderer has no block-sized scratch; the identity with short
/// blocks is what shows the long block is not merely non-silent but
/// *right* — control steps, glides and event offsets all land where they
/// would have.
///
/// *Mutation:* clamping `size` to 64 at the top
/// of `render_events` fails this (frame 64 onward goes silent and the event
/// at 200 is never reached in the long block).
#[test]
fn a_long_block_renders_every_frame_like_short_blocks() {
    let config = || SynthConfig {
        sample_rate: tutti_types::SampleRate::from(48_000.0),
        max_voices: 4,
        oscillator: OscillatorType::Saw,
        filter: FilterType::Svf {
            cutoff: Hz(3_000.0),
            q: tutti_types::Q(1.0),
            mode: crate::SvfMode::Lowpass,
        },
        portamento: Some(PortamentoConfig {
            mode: PortamentoMode::Always,
            curve: PortamentoCurve::Linear,
            time: Seconds(0.004),
            constant_time: true,
        }),
        ..Default::default()
    };

    // One 512-frame block: a note at 0, a second note at 200 (gliding in),
    // the first released at 300.
    let mut long = synth(config());
    queue_midi(
        &mut long,
        &[
            ev_note_on(0, 60, 100),
            ev_note_on(0, 67, 100).with_frame_offset(200),
            ev_note_off(0, 60).with_frame_offset(300),
        ],
    );
    let (mut l_long, mut r_long) = (vec![0.0f32; 512], vec![0.0f32; 512]);
    long.render_planar(&mut l_long, &mut r_long);

    // The same events, rebased into eight 64-frame blocks.
    let mut short = synth(config());
    let (mut l_short, mut r_short) = (Vec::new(), Vec::new());
    for block in 0..8 {
        match block {
            0 => queue_midi(&mut short, &[ev_note_on(0, 60, 100)]),
            3 => queue_midi(&mut short, &[ev_note_on(0, 67, 100).with_frame_offset(8)]),
            4 => queue_midi(&mut short, &[ev_note_off(0, 60).with_frame_offset(44)]),
            _ => {}
        }
        let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
        short.render_planar(&mut l, &mut r);
        l_short.extend_from_slice(&l);
        r_short.extend_from_slice(&r);
    }

    let tail_peak = l_long[448..].iter().fold(0.0f32, |a, s| a.max(s.abs()));
    assert!(
        tail_peak > 0.01,
        "the last 64 frames of the long block are silent"
    );
    assert_eq!(
        l_long, l_short,
        "left: one 512-frame block differs from eight 64-frame blocks"
    );
    assert_eq!(
        r_long, r_short,
        "right: one 512-frame block differs from eight 64-frame blocks"
    );
    assert_eq!(long.active_voice_count(), short.active_voice_count());
}

/// Sub-voices added under a held note sound at once, without waiting for
/// the next note-on.
///
/// Growing unison from 1 to 3 at full spread moves the existing sub-voice
/// hard left and adds two more, centre and hard right. If the added lanes
/// started idle the right channel would go silent until the key was struck
/// again.
///
/// *Mutation:* leaving the added lanes as allocated (idle) in
/// `VoiceBank::resize_stride` fails this.
#[test]
fn unison_grown_under_a_held_note_sounds_immediately() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::from(48_000.0),
        max_voices: 2,
        oscillator: OscillatorType::Saw,
        unison: Some(UnisonConfig {
            voice_count: 1,
            detune_cents: tutti_types::Cents(12.0),
            stereo_spread: Spread(1.0),
            phase_randomize: false,
        }),
        ..Default::default()
    });
    queue_midi(&mut synth, &[ev_note_on(0, 57, 100)]);
    let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
    for _ in 0..16 {
        synth.render_planar(&mut l, &mut r);
    }

    synth.set_unison_voice_count(3);
    let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for _ in 0..8 {
        synth.render_planar(&mut l, &mut r);
        left.extend_from_slice(&l);
        right.extend_from_slice(&r);
    }
    assert!(rms(&left) > 0.05, "the held note went quiet on resize");
    assert!(
        rms(&right) > 0.5 * rms(&left),
        "the added sub-voices are silent: left rms {}, right rms {}",
        rms(&left),
        rms(&right)
    );
}

/// A portamento glide sounds the same through `process` as through `tick`.
///
/// Rendering a whole MIDI sub-block at the glide's final pitch would make
/// a staircase, one step per block, always ahead of the true glide; the
/// phase error that leaves accumulates, so the waveforms would drift apart
/// even though their pitch at any block boundary agrees. Both paths run
/// the same renderer, and inside a control step the pitch ramps per
/// sample.
///
/// Not asserted bit-identical: `tick` takes a control step every sample and
/// `process` every 16, and between them the pitch ramps linearly rather
/// than along the glide's exponential curve.
///
/// *Mutation:* rendering each control step at its target pitch (zeroing
/// `inc_step` and loading `inc_target` at the top of `render_group`) fails
/// this at 0.33 against a 0.0056 tolerance; the correct renderer measures
/// 0.0018.
#[test]
fn process_and_tick_agree_through_a_glide() {
    let config = || SynthConfig {
        sample_rate: tutti_types::SampleRate::from(48_000.0),
        max_voices: 1,
        voice_mode: VoiceMode::Mono,
        oscillator: OscillatorType::Sine,
        envelope: Adsr::new(Seconds(0.001), Seconds(0.0), Amplitude(1.0), Seconds(0.1)),
        portamento: Some(PortamentoConfig {
            mode: PortamentoMode::Always,
            curve: PortamentoCurve::Linear,
            time: Seconds(0.05),
            constant_time: true,
        }),
        ..Default::default()
    };
    const BLOCKS: usize = 60;

    let mut ticked = synth(config());
    let mut blocked = synth(config());
    let mut via_tick = Vec::new();
    let mut via_process = Vec::new();
    for block in 0..BLOCKS {
        let events: &[MidiEvent] = match block {
            0 => &[ev_note_on(0, 48, 100)],
            // Three octaves up over 50 ms, well after the attack.
            10 => &[ev_note_on(0, 84, 100)],
            _ => &[],
        };
        queue_midi(&mut ticked, events);
        queue_midi(&mut blocked, events);

        let mut frame = [0.0f32; 2];
        for _ in 0..64 {
            ticked.tick(&[], &mut frame);
            via_tick.push(frame[0]);
        }
        let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
        blocked.render_planar(&mut l, &mut r);
        via_process.extend_from_slice(&l);
    }

    let glide = 10 * 64..BLOCKS * 64;
    let worst = glide
        .clone()
        .map(|i| (via_tick[i] - via_process[i]).abs())
        .fold(0.0f32, f32::max);
    let level = via_tick[glide].iter().fold(0.0f32, |a, s| a.max(s.abs()));
    assert!(level > 0.1, "the glide is inaudible");
    assert!(
        worst < 0.01 * level,
        "tick and process diverge by {worst} (level {level}) during the glide"
    );
}

/// Each note of a chord struck together gets its own stack of random
/// start phases, and the lanes really start on them.
///
/// Phases drawn once per note-on into a shared table would make every
/// note of a chord read the last draw, so all stacks would start
/// identically: the transient spike `phase_randomize` exists to avoid,
/// summed across the chord.
///
/// Both notes are the same pitch (on two channels) with no detune, so all
/// six lanes run at one increment and their phases after the first block
/// differ exactly as their start phases did.
///
/// *Mutation:* starting lanes from the shared table again
/// (`unison.voice_params(sub).phase_offset` in `drive`) fails this.
#[test]
fn a_chord_gets_a_distinct_phase_stack_per_voice() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::from(48_000.0),
        max_voices: 2,
        oscillator: OscillatorType::Saw,
        unison: Some(UnisonConfig {
            voice_count: 3,
            detune_cents: tutti_types::Cents(0.0),
            stereo_spread: Spread(0.0),
            phase_randomize: true,
        }),
        ..Default::default()
    });
    synth.seed_unison_rng(99);
    queue_midi(
        &mut synth,
        &[ev_note_on(0, 60, 100), ev_note_on(1, 60, 100)],
    );
    let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
    synth.render_planar(&mut l, &mut r);

    let phases: Vec<Vec<f32>> = (0..2)
        .map(|v| {
            (0..3)
                .map(|s| synth.bank.phase(synth.bank.lane(v, s)))
                .collect()
        })
        .collect();
    assert!(
        synth.voices.iter().all(|v| v.is_active()),
        "both notes should be sounding"
    );
    assert_ne!(
        phases[0], phases[1],
        "both voices of the chord started in phase: {phases:?}"
    );
    for stack in &phases {
        assert!(
            stack.windows(2).all(|w| (w[0] - w[1]).abs() > 1e-4),
            "a stack's sub-voices share a phase: {stack:?}"
        );
    }
}

/// A stolen voice changes level without a click.
///
/// The new note's velocity differs from the old one's. The voice is still
/// sounding, so its gain must ramp to the new level; stepping it in one
/// sample puts a discontinuity of `x * (g_old - g_new)` in the output.
///
/// The steal is timed to land near a peak of a 110 Hz sine, where that step
/// is largest: the steady sine moves at most `2π·110/48000 ≈ 0.014` of its
/// peak per sample, and the step would be about 0.7 of it.
///
/// *Mutations, each run:* snapping gains on every note-on (`snap_gain =
/// true` rather than `fresh` in `note_on`) fails this; so does replacing the
/// gain glide with a jump to the target, which is what a ramp across the
/// control step amounts to in `tick` (one frame per step).
#[test]
fn a_steal_at_a_different_velocity_does_not_click() {
    let mut synth = synth(SynthConfig {
        sample_rate: tutti_types::SampleRate::from(48_000.0),
        max_voices: 1,
        allocation_strategy: crate::AllocationStrategy::Oldest,
        oscillator: OscillatorType::Sine,
        envelope: Adsr::new(Seconds(0.001), Seconds(0.0), Amplitude(1.0), Seconds(0.1)),
        ..Default::default()
    });
    queue_midi(&mut synth, &[ev_note_on(0, 45, 127)]);
    let mut frame = [0.0f32; 2];
    let mut peak = 0.0f32;
    for _ in 0..4800 {
        synth.tick(&[], &mut frame);
        peak = peak.max(frame[0].abs());
    }
    // Advance to near a peak, then steal on another channel at low velocity.
    let mut prev = frame[0];
    while frame[0].abs() < 0.8 * peak {
        synth.tick(&[], &mut frame);
        prev = frame[0];
    }
    queue_midi(&mut synth, &[ev_note_on(1, 45, 20)]);
    let mut worst = 0.0f32;
    for _ in 0..256 {
        synth.tick(&[], &mut frame);
        worst = worst.max((frame[0] - prev).abs());
        prev = frame[0];
    }
    assert_eq!(
        synth.active_voice_count(),
        1,
        "the steal should reuse the one voice"
    );
    assert!(
        worst < 0.2 * peak,
        "the steal stepped the output by {worst} (peak {peak})"
    );
}

/// Reset All Controllers leaves the cutoff where the patch put it.
///
/// Setting the CC74 position to 0.0 instead would read, through the
/// `4^(v - 0.5)` map, as half the cutoff.
///
/// *Mutation:* `set_cc_cutoff(0.0)` in the `RESET_ALL` arm fails this.
#[test]
fn reset_all_controllers_leaves_the_cutoff_alone() {
    let config = || SynthConfig {
        sample_rate: tutti_types::SampleRate::from(48_000.0),
        max_voices: 1,
        oscillator: OscillatorType::Saw,
        filter: FilterType::Svf {
            cutoff: Hz(600.0),
            q: tutti_types::Q(0.707),
            mode: crate::SvfMode::Lowpass,
        },
        ..Default::default()
    };
    let render = |reset: bool| {
        let mut s = synth(config());
        let mut events = vec![ev_note_on(0, 45, 100)];
        if reset {
            events.push(ev_cc(0, cc::RESET_ALL, 0));
        }
        queue_midi(&mut s, &events);
        let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
        let mut out = Vec::new();
        for _ in 0..60 {
            s.render_planar(&mut l, &mut r);
            out.extend_from_slice(&l);
        }
        out
    };
    let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
    let (plain, reset) = (render(false), render(true));
    let (a, b) = (rms(&plain[1024..]), rms(&reset[1024..]));
    assert!(
        (a - b).abs() / a < 0.01,
        "Reset All Controllers moved the level from {a} to {b}"
    );
}

/// The voices and the bank use the engine's clamped unison count, not the
/// config's raw one.
///
/// *Mutation:* sizing from `u.voice_count` (the raw config value) in
/// `PolySynth::new` fails this.
#[test]
fn an_oversized_unison_count_is_clamped_everywhere() {
    let synth = synth(SynthConfig {
        max_voices: 2,
        unison: Some(UnisonConfig {
            voice_count: 40,
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(synth.unison_voice_count(), 16);
    assert_eq!(synth.voices[0].sub_voice_count(), 16);
    assert_eq!(
        synth.bank.lane(1, 0),
        16,
        "the bank's stride disagrees with the engine"
    );
}

/// A fork of the synth renders the master volume and unison it was taken
/// with, audibly: for each param, a move set through the synth's
/// `ParamSet` after a fork is not in that fork's render, and one before a
/// fork is (the `ParamNode` fork `param_parts` inserts). Each rendered fork is handed its own
/// chord, as its clip would.
///
/// Mutations (run): `fork_instance` not detaching `master_volume` →
/// "Volume: a live move reached the fork" → fails; not detaching the
/// unison → "Detune" and "StereoSpread" fail the same way.
#[test]
fn a_fork_renders_the_volume_and_unison_it_was_taken_with() {
    use tutti_graph::ParamFork;
    let live = || {
        let synth = synth(SynthConfig {
            sample_rate: tutti_types::SampleRate(48_000.0),
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            unison: Some(UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_types::Cents(15.0),
                stereo_spread: Spread(0.5),
                phase_randomize: false,
            }),
            ..Default::default()
        });
        synth.set_volume(0.5);
        synth.synth
    };
    let render = |fork: PolySynth| {
        let mut fork = Hand::new(fork);
        queue_midi(&mut fork, &[ev_note_on(0, 60, 100), ev_note_on(0, 64, 100)]);
        let (mut l, mut r) = (vec![0.0f32; 2_048], vec![0.0f32; 2_048]);
        fork.render_planar(&mut l, &mut r);
        (l, r)
    };
    for (param, value) in [
        (tutti_types::UnitParam::Volume, 0.9),
        (tutti_types::UnitParam::Detune, 40.0),
        (tutti_types::UnitParam::StereoSpread, 1.0),
    ] {
        let synth = live();
        let source = ParamFork::new(&synth);
        let baseline = render(source.fork_node());
        assert!(baseline.0.iter().any(|&s| s != 0.0), "the chord sounds");
        let taken = source.fork_node();
        assert!(
            source.params().set(param, value),
            "{param:?} is addressable"
        );
        assert_eq!(
            render(taken),
            baseline,
            "{param:?}: a live move reached the fork"
        );
        assert_ne!(
            render(source.fork_node()),
            baseline,
            "{param:?}: a fork taken after the move does not hear it"
        );
    }
}

// --- One behaviour on either wire ---

fn ump(message: Message) -> Ump {
    Ump::from(message)
}

fn address(channel: u8) -> Address {
    Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::of(channel),
    }
}

fn u7(v: u8) -> U7 {
    U7::new(v).expect("a 7-bit value")
}

/// Voices whose key is still held.
fn sounding(synth: &PolySynth) -> usize {
    synth
        .allocator
        .slots()
        .iter()
        .filter(|s| s.is_key_held())
        .count()
}

fn poly_synth() -> PolySynth {
    PolySynth::new(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        ..Default::default()
    })
    .expect("synth builds")
}

/// A MIDI 1.0 Note On and the MIDI 2.0 Note On that widens it claim the same
/// voice: same note, channel and velocity, which is the figure translation
/// scales to. Mutations, each failing this test: reading the MIDI 1.0
/// velocity as `velocity / 127.0`; ignoring `Midi1` messages.
#[test]
fn a_midi1_and_a_midi2_note_on_claim_the_same_voice() {
    let wide = midi1_velocity_to_midi2(100);
    let m1 = Message::Midi1(
        address(3),
        Voice1::NoteOn {
            note: note_number(64),
            velocity: u7(100),
        },
    );
    let m2 = Message::Midi2(
        address(3),
        Voice2::NoteOn {
            note: note_number(64),
            velocity: wide,
            attribute: Attribute::None,
        },
    );
    for message in [m1, m2] {
        let mut synth = poly_synth();
        synth.apply_midi(ump(message));
        assert_eq!(synth.active_voice_count(), 1, "{message:?}");
        let slot = &synth.allocator.slots()[0];
        assert_eq!(slot.note(), note_number(64), "{message:?}");
        assert_eq!(slot.channel(), MidiChannel::of(3), "{message:?}");
        assert_eq!(
            slot.velocity(),
            u16_to_unit_f32(wide),
            "{message:?} sounds at the widened velocity"
        );
    }
}

/// Every way to end a note ends it: a Note Off on either wire, and a Note On
/// at velocity 0 on either wire. A MIDI 2.0 Note On at velocity 0 is a Note On
/// in M2-104 §7.4.2; the synth has always read it as a Note Off, and this pins
/// that for the translation stage to change. Mutations, each failing this
/// test: the MIDI 2.0 velocity-0 arm sounding a note; the MIDI 1.0 velocity-0
/// guard removed; the MIDI 1.0 Note Off ignored.
#[test]
fn every_note_off_form_releases_the_note() {
    let note = note_number(60);
    let on = Message::Midi1(
        address(0),
        Voice1::NoteOn {
            note,
            velocity: u7(90),
        },
    );
    let offs = [
        Message::Midi1(
            address(0),
            Voice1::NoteOff {
                note,
                velocity: u7(0),
            },
        ),
        Message::Midi1(
            address(0),
            Voice1::NoteOn {
                note,
                velocity: u7(0),
            },
        ),
        Message::Midi2(
            address(0),
            Voice2::NoteOff {
                note,
                velocity: 0,
                attribute: Attribute::None,
            },
        ),
        Message::Midi2(
            address(0),
            Voice2::NoteOn {
                note,
                velocity: 0,
                attribute: Attribute::None,
            },
        ),
    ];
    for off in offs {
        let mut synth = poly_synth();
        synth.apply_midi(ump(on));
        assert_eq!(sounding(&synth), 1, "{off:?}");
        synth.apply_midi(ump(off));
        assert_eq!(sounding(&synth), 0, "{off:?}");
    }
}

/// Pitch bend and the sustain pedal act alike from either wire. Mutations,
/// each failing this test: a MIDI 1.0 bend divided by 16383 instead of widened
/// by translation's scaler; MIDI 1.0 messages ignored.
#[test]
fn midi1_and_midi2_controllers_reach_the_same_state() {
    let bend14 = 12_288u16;
    let wide_bend = midi1_pitch_bend_to_midi2(bend14);
    let expected = bend_u32_to_signed_f32(wide_bend);
    for message in [
        Message::Midi1(
            address(0),
            Voice1::PitchBend(U14::new(bend14).expect("14 bits")),
        ),
        Message::Midi2(address(0), Voice2::PitchBend(wide_bend)),
    ] {
        let mut synth = poly_synth();
        synth.apply_midi(ump(message));
        // The synth keeps where the pitch stands: the wheel times the range.
        assert_eq!(
            synth.bends[usize::from(MidiChannel::FIRST.get())],
            synth.config.pitch_bend_range * expected,
            "{message:?}"
        );
    }

    let sustain = |message: Message| {
        let mut synth = poly_synth();
        let note = note_number(60);
        synth.apply_midi(ump(Message::Midi1(
            address(0),
            Voice1::NoteOn {
                note,
                velocity: u7(90),
            },
        )));
        synth.apply_midi(ump(message));
        synth.apply_midi(ump(Message::Midi1(
            address(0),
            Voice1::NoteOff {
                note,
                velocity: u7(0),
            },
        )));
        synth.allocator.slots()[0].is_sustained()
    };
    let pedal = cc::SUSTAIN;
    assert!(sustain(Message::Midi1(
        address(0),
        Voice1::ControlChange {
            controller: pedal,
            value: u7(127)
        }
    )));
    assert!(sustain(Message::Midi2(
        address(0),
        Voice2::ControlChange {
            controller: pedal,
            value: midi1_cc_to_midi2(127)
        }
    )));
    assert!(!sustain(Message::Midi1(
        address(0),
        Voice1::ControlChange {
            controller: pedal,
            value: u7(0)
        }
    )));
}

fn mpe_synth() -> PolySynth {
    PolySynth::new(SynthConfig {
        sample_rate: tutti_types::SampleRate::SR_44K1,
        max_voices: 4,
        voice_mode: VoiceMode::Poly,
        mpe_enabled: true,
        ..Default::default()
    })
    .expect("synth builds")
}

fn voice_of(synth: &PolySynth, note: u8) -> &SynthVoice {
    synth
        .voices
        .iter()
        .find(|v| v.is_active() && v.note() == note_number(note))
        .expect("the note sounds")
}

/// Per-Note Management read from literal words (M2-104 Figure 51: D is bit 1
/// and S bit 0), so neither the encoder nor the decoder can be wrong in step
/// with the other. Mutation: testing `reset` before `detach` in `voice2` made
/// both halves fail (as did `if reset` for `if detach` in the handler).
#[test]
fn per_note_management_flags_are_read_from_the_spec_bits() {
    let on = ump(Message::Midi2(
        address(1),
        Voice2::NoteOn {
            note: note_number(60),
            velocity: 0xC000,
            attribute: Attribute::None,
        },
    ));
    let bend = |value: u32| {
        ump(Message::Midi2(
            address(1),
            Voice2::PerNotePitchBend {
                note: note_number(60),
                value,
            },
        ))
    };

    // D = 1: the bend freezes.
    let mut synth = mpe_synth();
    synth.apply_midi(on);
    synth.apply_midi(bend(0xFFFF_FFFF));
    let held = voice_of(&synth, 60).mpe_state().pitch_bend_semitones.get();
    assert!(held > 0.0);
    synth.apply_midi(Ump([0x40F1_3C02, 0, 0, 0]));
    synth.apply_midi(bend(0x8000_0000));
    assert_eq!(
        voice_of(&synth, 60).mpe_state().pitch_bend_semitones.get(),
        held
    );

    // S = 1: the bend resets, and the note keeps responding.
    let mut synth = mpe_synth();
    synth.apply_midi(on);
    synth.apply_midi(bend(0xFFFF_FFFF));
    synth.apply_midi(Ump([0x40F1_3C01, 0, 0, 0]));
    assert_eq!(
        voice_of(&synth, 60).mpe_state().pitch_bend_semitones.get(),
        0.0
    );
    synth.apply_midi(bend(0xFFFF_FFFF));
    assert_eq!(
        voice_of(&synth, 60).mpe_state().pitch_bend_semitones.get(),
        held
    );
}

/// A note with a reserved attribute type (4 and up) sounds and ends: Table 8
/// (M2-104 §7.4.14) reserves the types for senders and gives a receiver no rule
/// for them, so the attribute is not read. `midi2` refused the packet, so the note was dropped and an off for it left the voice stuck.
/// Mutation: ignoring `Attribute::Reserved` on the Note On, then on the Note
/// Off, each failed here.
#[test]
fn a_reserved_attribute_type_does_not_stop_a_note() {
    let reserved = Attribute::Reserved {
        kind: 5,
        data: 0x1234,
    };
    let mut synth = poly_synth();
    synth.apply_midi(ump(Message::Midi2(
        address(0),
        Voice2::NoteOn {
            note: note_number(60),
            velocity: 0x8000,
            attribute: reserved,
        },
    )));
    assert_eq!(sounding(&synth), 1);
    synth.apply_midi(ump(Message::Midi2(
        address(0),
        Voice2::NoteOff {
            note: note_number(60),
            velocity: 0,
            attribute: reserved,
        },
    )));
    assert_eq!(sounding(&synth), 0);
}

/// An Assignable Per-Note Controller whose index byte has the high bit set is
/// not CC74 or CC7: `CCNumber::new` used to mask 0xCA to 74. Mutation:
/// restoring the mask made the 0xCA case move the slide.
#[test]
fn an_assignable_per_note_index_is_a_whole_byte() {
    let drive = |controller: u8| {
        let mut synth = mpe_synth();
        synth.apply_midi(ump(Message::Midi2(
            address(1),
            Voice2::NoteOn {
                note: note_number(60),
                velocity: 0x8000,
                attribute: Attribute::None,
            },
        )));
        let before = voice_of(&synth, 60).mpe_state().slide;
        synth.apply_midi(ump(Message::Midi2(
            address(1),
            Voice2::AssignablePerNote {
                note: note_number(60),
                controller,
                value: u32::MAX,
            },
        )));
        (before, voice_of(&synth, 60).mpe_state().slide)
    };
    let (before, after) = drive(74);
    assert_ne!(before, after, "index 74 is slide");
    let (before, after) = drive(0xCA);
    assert_eq!(before, after, "index 0xCA is not");
}

/// MIDI 1.0 controller and pressure values are widened by translation's
/// scaler, to exact figures: 64 is the centre, 127 is full. Mutations, each
/// failing here: `value / 127.0` for the CC (64 gives 0.504); the poly
/// pressure forced to `0.0 *`.
#[test]
fn midi1_values_scale_to_exact_figures() {
    let mut synth = mpe_synth();
    let note = note_number(60);
    synth.apply_midi(ump(Message::Midi1(
        address(1),
        Voice1::NoteOn {
            note,
            velocity: u7(100),
        },
    )));
    for (value, expected) in [(0, 0.0), (64, 0.5), (127, 1.0)] {
        synth.apply_midi(ump(Message::Midi1(
            address(1),
            Voice1::ControlChange {
                controller: cc::MOD_WHEEL,
                value: u7(value),
            },
        )));
        assert_eq!(synth.voices[0].mod_wheel(), expected, "CC1 = {value}");
        synth.apply_midi(ump(Message::Midi1(
            address(1),
            Voice1::PolyPressure {
                note,
                pressure: u7(value),
            },
        )));
        assert_eq!(
            voice_of(&synth, 60).mpe_state().pressure,
            expected,
            "pressure = {value}"
        );
    }
}

/// Reset All Controllers (RP-015) returns the modulation wheel to 0 and does
/// not reset the sound controllers (CC 70-79), so CC 71 and CC 74 keep their
/// positions. The wheel is the synth's global one, so every voice's resets.
///
/// *Mutation (run):* `set_cc_cutoff(0.5)` and `set_filter_resonance(0.0)` back
/// in the `RESET_ALL` arm fail the sound-controller assertions;
/// dropping `set_mod_wheel(0.0)` fails the wheel one.
#[test]
fn reset_all_controllers_keeps_the_sound_controllers() {
    let mut s = synth(SynthConfig {
        max_voices: 2,
        ..Default::default()
    });
    queue_midi(
        &mut s,
        &[
            ev_note_on(0, 60, 100),
            ev_cc(0, cc::MOD_WHEEL, 100),
            ev_cc(0, cc::BRIGHTNESS, 0),
            ev_cc(0, cc::RESONANCE, 127),
        ],
    );
    let mut out = [0.0f32; 2];
    s.tick(&[], &mut out);
    let (cutoff, resonance) = s.voices[0].sound_controllers();
    assert_eq!((cutoff, resonance), (0.0, 1.0), "the controllers were set");
    assert!(s.voices[0].mod_wheel() > 0.5);

    queue_midi(&mut s, &[ev_cc(0, cc::RESET_ALL, 0)]);
    s.tick(&[], &mut out);
    assert_eq!(s.voices[0].sound_controllers(), (cutoff, resonance));
    assert_eq!(s.voices[0].mod_wheel(), 0.0);
}

/// The MPE switch is live: turning it on after construction makes the master
/// channel's bend reach every channel, and turning it off takes that back.
///
/// Mutation (run): `set_mpe_enabled` not telling the note logic
/// (`allocator.set_zone_masters`) leaves channel 5 centred.
#[test]
fn the_mpe_switch_reaches_the_note_logic_while_the_synth_runs() {
    let bend = |synth: &mut PolySynth, channel: u8| {
        synth.apply_midi(ump(Message::Midi2(
            address(channel),
            Voice2::PitchBend(u32::MAX),
        )));
    };
    let mut synth = poly_synth();
    synth.set_mpe_enabled(true);
    bend(&mut synth, 0);
    assert_ne!(synth.bends[5].0, 0.0, "a master bend reaches channel 5");
    synth.set_mpe_enabled(false);
    bend(&mut synth, 15);
    assert_ne!(synth.bends[15].0, 0.0, "and its own channel");
    // Off again, a bend on channel 0 stays on channel 0.
    synth.apply_midi(ump(Message::Midi2(address(0), Voice2::PitchBend(0))));
    assert_ne!(
        synth.bends[5].0, synth.bends[0].0,
        "channel 5 keeps the bend it had"
    );
}
