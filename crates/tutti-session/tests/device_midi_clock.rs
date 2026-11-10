//! A restart re-rates the MIDI clock master, through a session on
//! `ManualStreamDriver`s (the session half of `bevy-tutti`'s
//! `engine::restart` test `a_restart_re_rates_the_midi_clock`, moved here when
//! the adapter was parked).

use tutti_cpal::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_session::Session;
use tutti_transport::MotionEvent;
use tutti_types::{ChannelLayout, SampleRate};

const OLD: f64 = 44_100.0;
const NEW: f64 = 48_000.0;

fn spec_at(rate: f64) -> OutputSpec {
    OutputSpec::new(
        SampleRate(rate),
        ChannelLayout::STEREO,
        tutti_cpal::cpal::SampleFormat::F32,
    )
}

/// Render `blocks` blocks of `frames` on `stream` and drain the hardware out
/// the clock is wired to after each, an update of the session between: every
/// event's status byte and its frame on the graph's clock (which the out node
/// stamps it with).
fn clock(
    s: &mut Session,
    stream: &ManualStream,
    (frames, blocks): (usize, usize),
) -> Vec<(u32, u64)> {
    let mut out = Vec::new();
    let mut buf = [tutti_midi_types::ump::MidiEvent::noop(); 64];
    for _ in 0..blocks {
        stream.render_block(frames).expect("the stream is open");
        let n = s
            .midi_nodes()
            .expect("the engine's MIDI nodes")
            .out_controls
            .poll_into(&mut buf);
        out.extend(
            buf[..n]
                .iter()
                .map(|e| ((e.data[0] >> 16) & 0xFF, u64::from(e.frame_offset))),
        );
        s.update();
    }
    out
}

/// **A restart re-rates the MIDI clock master.** At 120 BPM a 24-PPQN tick is
/// 1/48 s: 918.75 frames at 44.1 kHz, 1 000 at 48 kHz. Through the engine's
/// own clock node, the ticks are 918.75 frames apart before the restart and
/// 1 000 after it, and the restart sends no Song Position (it is not a
/// locate: 512-frame blocks move the beat further at 44.1 kHz than at 48,
/// which a seek check with a fixed epsilon would take for a locate).
///
/// Mutation (run, failed): `Session::rerate` not re-preparing the graph (the
/// `reprepare` call removed): the clock keeps its 44.1 kHz grid. (Removing
/// `ClockNode::prepare`'s `ClockMaster::set_sample_rate` does not fail this
/// test, verified: the ticks are placed at the transport's rate, which the
/// block carries, and the master's own rate is MTC's, which
/// `clock_master`'s unit tests pin.)
#[test]
fn a_restart_re_rates_the_midi_clock() {
    let (driver, old) = ManualStreamDriver::new();
    let (mut s, mut device) = Session::builder()
        .build_device_on(spec_at(OLD), driver)
        .expect("builds with no device");
    s.update();
    s.midi_nodes()
        .expect("the engine's MIDI nodes")
        .clock_master
        .set_enabled(true);
    s.transport()
        .motion
        .try_send(MotionEvent::Play)
        .expect("room");
    let ticks = |events: &[(u32, u64)]| -> f64 {
        let at: Vec<u64> = events
            .iter()
            .filter(|(status, _)| *status == 0xF8)
            .map(|&(_, at)| at)
            .collect();
        assert!(at.len() > 20, "ticks to measure");
        (at[at.len() - 1] - at[0]) as f64 / (at.len() - 1) as f64
    };

    let before = clock(&mut s, &old, (512, 100));
    assert!((ticks(&before) - 918.75).abs() < 0.05, "{}", ticks(&before));

    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec_at(NEW), driver, None, |_| {})
        .expect("restarts");
    let after = clock(&mut s, &new, (512, 100));
    assert!((ticks(&after) - 1_000.0).abs() < 0.05, "{}", ticks(&after));
    assert!(
        !after.iter().any(|(status, _)| *status == 0xF2),
        "the restart is not a locate"
    );
}
