//! The two effect nodes behind a SoundFont unit's sends: the General MIDI
//! chorus (`ModDelayConfig::GENERAL_MIDI_CHORUS`) and `DesignNode`. Every
//! figure is from the definition of the effect (a wet-only line is a delayed
//! copy, a reverb's tail covers its ring-out), not from another render.

use tutti_graph::Node;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::{FreeverbNode, ModDelayConfig, ModDelayNode};
use tutti_types::{ChannelLayout, Feedback, SampleRate, Tail};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

/// `node` driven over `frames` frames of a unit impulse at frame 0 on every
/// input, one `Vec` per output.
fn impulse_response<N: Node>(node: N, frames: usize) -> Vec<Vec<f32>> {
    let mut node = prepared(node, SR, BLOCK);
    let ins = usize::from(node.shape().audio_in().count());
    let mut out: Vec<Vec<f32>> = Vec::new();
    let mut done = 0;
    while done < frames {
        let n = BLOCK.min(frames - done);
        let chunk: Vec<f32> = (done..done + n)
            .map(|i| if i == 0 { 1.0 } else { 0.0 })
            .collect();
        let inputs: Vec<&[f32]> = (0..ins).map(|_| &chunk[..]).collect();
        let block = drive(&mut node, SR, &inputs, &[]);
        out.resize(block.len(), Vec::with_capacity(frames));
        for (ch, b) in out.iter_mut().zip(block) {
            ch.extend(b);
        }
        done += n;
    }
    out
}

fn chorus() -> ModDelayNode {
    ModDelayNode::new(ChannelLayout::STEREO, ModDelayConfig::GENERAL_MIDI_CHORUS)
}

fn last_nonzero(channel: &[f32]) -> Option<usize> {
    channel.iter().rposition(|&s| s != 0.0)
}

/// The chorus is wet only: a unit impulse at frame 0 leaves nothing at frame 0
/// (a dry path would pass it), and comes out later, inside the delay the
/// config sweeps (2 ms +/- 1.9 ms, so frames 4 to 187 at 48 kHz, with a frame
/// either side for the interpolation).
#[test]
fn the_general_midi_chorus_is_wet_only() {
    let out = impulse_response(chorus(), 1_000);
    for (c, channel) in out.iter().enumerate() {
        assert_eq!(channel[0], 0.0, "channel {c}: no dry impulse");
        let first = channel.iter().position(|&s| s != 0.0).expect("it sounds");
        let last = last_nonzero(channel).unwrap();
        // 0.1 ms is the shortest the sweep goes (2 - 1.9 ms), 3.9 ms the longest.
        assert!((4..=190).contains(&first), "channel {c}: first at {first}");
        assert!(last <= 190, "channel {c}: last at {last}");
    }
}

/// With no feedback, the line is a delay and nothing more: the impulse comes
/// out once (a linear read spreads it over two adjacent frames at most), and
/// raising the feedback through the node's param does not make it ring, since
/// the config's ceiling is none.
#[test]
fn the_general_midi_chorus_cannot_recirculate() {
    let node = chorus();
    node.set_feedback(Feedback(0.9));
    let out = impulse_response(node, 2_000);
    for channel in &out {
        let sounding = channel.iter().filter(|&&s| s != 0.0).count();
        assert!(
            (1..=2).contains(&sounding),
            "an impulse is heard once, over at most two frames: {sounding}"
        );
    }
}

/// The ceiling holds against the raw cell and the param path, which skip
/// `set_feedback`'s clamp: -0.9 (a negative would recirculate with a flipped
/// sign) and 5.0 (past any ceiling) both leave the line a pure delay, so the
/// declared finite tail stays true.
#[test]
fn a_raw_write_cannot_make_the_general_midi_chorus_recirculate() {
    use std::sync::atomic::Ordering;
    for raw in [-0.9f32, 5.0] {
        let node = chorus();
        node.feedback().store(raw, Ordering::Relaxed);
        let out = impulse_response(node, 2_000);
        for channel in &out {
            let sounding = channel.iter().filter(|&&s| s != 0.0).count();
            assert!(
                (1..=2).contains(&sounding),
                "raw {raw}: heard over {sounding} frames"
            );
        }
    }
}

/// The chorus declares its delay as its tail, and the output never outlasts
/// it. The tail is the config's `max_delay`, 5 ms: 240 frames at 48 kHz.
#[test]
fn the_general_midi_chorus_declares_its_delay_as_its_tail() {
    let node = prepared(chorus(), SR, BLOCK);
    let Tail::Finite(tail) = node.shape().tail else {
        panic!("a finite tail, not {:?}", node.shape().tail)
    };
    assert_eq!(tail.get(), 240);
    let out = impulse_response(chorus(), 1_000);
    for channel in &out {
        assert!(last_nonzero(channel).unwrap() < tail.get());
    }
}

/// The ceiling is the config's: the presets that recirculate keep their tail
/// `Unknown`, as they were.
#[test]
fn a_recirculating_chorus_keeps_an_unknown_tail() {
    for node in [
        ModDelayNode::chorus(ChannelLayout::STEREO),
        ModDelayNode::flanger(ChannelLayout::STEREO),
    ] {
        assert_eq!(prepared(node, SR, BLOCK).shape().tail, Tail::Unknown);
    }
}

/// The reverb is one channel in and two out, and silent until its first comb
/// has gone round (a wet-only effect passes nothing straight through).
#[test]
fn the_reverb_is_mono_in_stereo_out_and_wet_only() {
    let node = FreeverbNode::general_midi();
    let shape = node.shape();
    assert_eq!(shape.audio_in(), ChannelLayout::MONO);
    assert_eq!(shape.audio_out(), ChannelLayout::STEREO);
    let out = impulse_response(FreeverbNode::general_midi(), 2_000);
    assert_eq!(out.len(), 2);
    // 1116 frames at 44.1 kHz is 1215 at 48 kHz (Freeverb's tuning, scaled).
    assert!(out[0][..1215].iter().all(|&s| s == 0.0));
    assert!(out[0][1215].abs() > 0.01);
}

/// The reverb's declared tail covers its ring-out: the last frame of the wet
/// signal at or above a thousandth (-60 dB) of its own peak is within it. The
/// node is prepared at 48 kHz, so the tail is figured at that rate.
#[test]
fn the_reverbs_tail_covers_its_ring_out() {
    let node = prepared(FreeverbNode::general_midi(), SR, BLOCK);
    let Tail::Finite(tail) = node.shape().tail else {
        panic!("a finite tail, not {:?}", node.shape().tail)
    };
    let out = impulse_response(FreeverbNode::general_midi(), tail.get() + 20_000);
    let peak = out.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
    let rang = out
        .iter()
        .filter_map(|c| c.iter().rposition(|s| s.abs() >= 1e-3 * peak))
        .max()
        .expect("it rings")
        + 1;
    println!(
        "RINGOUT reverb node rang {rang} frames, tail {}",
        tail.get()
    );
    assert!(rang <= tail.get(), "rang {rang}, tail {}", tail.get());
    assert!(
        rang > tail.get() / 4,
        "not absurdly loose: {rang} of {}",
        tail.get()
    );
}

/// `reset` empties the room: after it the node answers an impulse as a fresh
/// one does.
#[test]
fn reset_empties_the_room() {
    let mut node = prepared(FreeverbNode::general_midi(), SR, BLOCK);
    let burst = vec![1.0f32; BLOCK];
    for _ in 0..40 {
        drive(&mut node, SR, &[&burst], &[]);
    }
    node.reset();
    let silence = vec![0.0f32; BLOCK];
    let out = drive(&mut node, SR, &[&silence], &[]);
    assert!(out.iter().flatten().all(|&s| s == 0.0));
}
