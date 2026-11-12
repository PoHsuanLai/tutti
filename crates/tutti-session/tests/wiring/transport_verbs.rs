//! The session's transport verbs: `set_tempo`, `locate`, `play`, `stop` and
//! the one timed form, `schedule`.

use tutti_session::Session;
use tutti_transport::{FadeOut, MotionEvent, Timeline};
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
    let beat = s.transport().beat().get();
    assert!((beat - 3.0).abs() < 1e-3, "{beat}");

    s.stop(FadeOut::Immediate).unwrap();
    render(&s, 2_000);
    let held = s.transport().beat().get();
    render(&s, 10_000);
    assert_eq!(s.transport().beat().get(), held, "stopped");
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
    let beat = s.transport().beat().get();
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
    let beat = s.transport().beat().get();
    assert!((beat - 1.0).abs() < 0.01, "{beat}");
}

/// **The session's playhead is one read of the last block.** At 60 BPM a beat
/// is a second: a locate to beat 2 and a play, then two 1 000-frame blocks,
/// reads rolling at the second block's first frame, beat 2 + 1 000 / 48 000,
/// frame 1 000 and seconds equal to the beat. A stop reads stopped with its
/// beat, frame and seconds from that same block.
#[test]
fn the_playhead_is_one_read_of_the_last_block() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0));
    s.locate(Beat(2.0)).unwrap();
    s.play().unwrap();
    render(&s, 2_000);
    let p = s.playhead();
    assert!(p.playing());
    assert_eq!(p.frame(), Frame(1_000));
    assert!((p.beat().get() - (2.0 + 1_000.0 / 48_000.0)).abs() < 1e-9);
    assert!((p.seconds().get() - p.beat().get()).abs() < 1e-9);
    assert!(p.at().is_some());

    s.stop(FadeOut::Immediate).unwrap();
    render(&s, 1_000);
    let p = s.playhead();
    assert!(
        !p.playing(),
        "the stop and the beat it left are one snapshot"
    );
    assert_eq!(p.frame(), Frame(2_000));
    assert!((p.beat().get() - (2.0 + 2_000.0 / 48_000.0)).abs() < 1e-9);
    assert_eq!(
        p.beat_now(p.at().unwrap() + 500_000),
        p.beat(),
        "stopped holds"
    );
}

/// **A session that renders nothing has a resting playhead**, a value and not
/// a panic: stopped at the origin with no clock reading, and `beat_now` holds
/// the origin.
#[test]
fn a_detached_session_reads_a_resting_playhead() {
    let s = Session::builder()
        .build_detached(SampleRate(48_000.0))
        .expect("builds");
    let p = s.playhead();
    assert!(!p.playing());
    assert_eq!((p.beat(), p.frame(), p.at()), (Beat(0.0), Frame(0), None));
    assert_eq!(s.beat_now(), Beat(0.0));
}
