//! What reaches the speakers, and who decides.
//!
//! `Net::pipe_output` is not a mix. It walks *every* global output channel and
//! overwrites that channel's edge, so two callers do not layer — the second
//! silently disconnects the first. Two places in this crate call it as though it
//! layered: `engine::build` for the metronome (its comment says "mixed into
//! master output") and `synth::soundfont` for every soundfont that finishes
//! loading.
//!
//! These tests pin that behaviour *before* it is fixed. They assert what the
//! code does today, not what it should do, and the commit that introduces
//! declarative wiring inverts them. A test written after the fix would prove
//! nothing about whether the bug was ever real.

#![cfg(feature = "synth")]

use tutti_core::dsp::{sine_hz, Net, Source};

/// Two `pipe_output` calls do not sum. The second wins outright.
///
/// This is the whole defect in four lines: nothing warns, nothing logs, and the
/// first node is simply gone from the output.
#[test]
fn a_second_pipe_output_silently_replaces_the_first() {
    let mut net = Net::new(0, 2);
    let first = net.push(Box::new(sine_hz::<f32>(440.0)));
    let second = net.push(Box::new(sine_hz::<f32>(880.0)));

    net.pipe_output(first);
    assert_eq!(
        net.output_source(0),
        Source::Local(first, 0),
        "the first claim lands"
    );

    net.pipe_output(second);
    assert_eq!(
        net.output_source(0),
        Source::Local(second, 0),
        "and the second overwrites it — this is the bug, not a mix"
    );
    assert_eq!(
        net.output_source(1),
        // `sine_hz` has one output, so `channel % node_outputs` wraps both
        // global channels onto port 0.
        Source::Local(second, 0),
        "on every channel, not just channel 0"
    );
}

/// The clobber reaches *all* global channels regardless of the source's width,
/// which is why a mono node claiming the bus silences a stereo one on both
/// sides rather than just the left.
#[test]
fn pipe_output_claims_every_channel_even_from_a_mono_source() {
    let mut net = Net::new(0, 2);
    let stereo = net.push(Box::new(sine_hz::<f32>(440.0) | sine_hz::<f32>(440.0)));
    let mono = net.push(Box::new(sine_hz::<f32>(880.0)));

    net.pipe_output(stereo);
    net.pipe_output(mono);

    // `pipe_output` wraps with `channel % node_outputs`, so a 1-output node
    // feeds both channels from its single port.
    assert_eq!(net.output_source(0), Source::Local(mono, 0));
    assert_eq!(net.output_source(1), Source::Local(mono, 0));
}

/// The engine's own build hands the master to the metronome the same way, so
/// anything that later calls `pipe_output` takes the whole bus from it.
///
/// Stated as a unit-level fact rather than driven through `build_into`, which
/// needs a real audio device. The two call sites are `engine/build.rs` (the
/// click) and `synth/soundfont.rs` (each promoted soundfont); this reproduces
/// exactly the sequence those two produce in a running app.
#[test]
fn a_later_node_takes_the_master_from_the_metronome() {
    let mut net = Net::new(0, 2);

    // Stand-in for the click node `build_into` pipes to output.
    let click = net.push(Box::new(sine_hz::<f32>(1000.0)));
    net.pipe_output(click);

    // A soundfont finishes loading a few frames later.
    let soundfont = net.push(Box::new(sine_hz::<f32>(261.0)));
    net.pipe_output(soundfont);

    assert_ne!(
        net.output_source(0),
        Source::Local(click, 0),
        "the metronome is disconnected — silently, with nothing in the log"
    );
    assert_eq!(net.output_source(0), Source::Local(soundfont, 0));
}
