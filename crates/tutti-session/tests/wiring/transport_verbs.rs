//! The session's transport verbs: `set_tempo`, `locate`, `play`, `stop` and
//! the one timed form, `schedule`.

use tutti_session::Session;
use tutti_transport::{FadeOut, MotionEvent};
use tutti_types::{Beat, Bpm, ChannelLayout, Frame, InterleavedMut, MaxBlock, SampleRate, Samples};

fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_000)),
        )
        .expect("builds")
}

fn render(s: &Session, frames: usize) {
    let mut block = vec![0.0f32; 1_000 * 2];
    for _ in 0..frames / 1_000 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    }
}

/// **The verbs drive the playhead on the project's tempo.** At 60 BPM a beat
/// is one second: a locate to beat 2 then a play, and one second rendered, is
/// the playhead at beat 3; a stop holds it there.
#[test]
fn locate_play_and_stop_move_the_playhead_on_the_set_tempo() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0));
    s.locate(Beat(2.0)).unwrap();
    s.play().unwrap();
    render(&s, 48_000);
    let beat = s.transport().settings().beat().get();
    assert!((beat - 3.0).abs() < 1e-3, "{beat}");

    s.stop(FadeOut::Immediate).unwrap();
    render(&s, 2_000);
    let held = s.transport().settings().beat().get();
    render(&s, 10_000);
    assert_eq!(s.transport().settings().beat().get(), held, "stopped");
}

/// **A scheduled play starts on its frame, not at the next block**: a play
/// scheduled at frame 24 000 of a 60 BPM transport leaves the playhead at
/// half a beat after one second.
#[test]
fn a_scheduled_play_starts_on_its_frame() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0));
    s.schedule(Frame(24_000), MotionEvent::Play).unwrap();
    render(&s, 48_000);
    let beat = s.transport().settings().beat().get();
    assert!((beat - 0.5).abs() < 1e-3, "{beat}");
}

/// **A beat schedules as it stands**: a stop scheduled at beat 1 of a rolling
/// 60 BPM transport halts the playhead there.
#[test]
fn a_stop_scheduled_at_a_beat_halts_there() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0));
    s.schedule(Beat(1.0), MotionEvent::stop_unfaded()).unwrap();
    s.play().unwrap();
    render(&s, 3_000);
    render(&s, 45_000);
    let beat = s.transport().settings().beat().get();
    assert!((beat - 1.0).abs() < 0.01, "{beat}");
}
