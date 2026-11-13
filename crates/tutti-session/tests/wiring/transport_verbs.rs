//! The session's transport verbs: `set_tempo`, `locate`, `play`, `stop` and
//! the one timed form, `schedule`.

use tutti_session::{ScheduleError, Session};
use tutti_transport::{FadeOut, MotionEvent, Timeline};
use tutti_types::{
    At, Beat, Bpm, ChannelLayout, Frame, InterleavedMut, LoopRange, MaxBlock, SampleRate, Samples,
};

fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate::SR_48K,
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
    s.set_tempo(Bpm::of(60.0)).expect("not following");
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
    s.set_tempo(Bpm::of(60.0)).expect("not following");
    s.schedule(Frame::new(24_000), MotionEvent::Play).unwrap();
    render(&s, 48_000);
    let beat = s.transport().beat().get();
    assert!((beat - 0.5).abs() < 1e-3, "{beat}");
}

/// **A beat schedules as it stands**: a stop scheduled at beat 1 of a rolling
/// 60 BPM transport halts the playhead there.
#[test]
fn a_stop_scheduled_at_a_beat_halts_there() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0)).expect("not following");
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
    s.set_tempo(Bpm::of(60.0)).expect("not following");
    s.locate(Beat(2.0)).unwrap();
    s.play().unwrap();
    render(&s, 2_000);
    let p = s.playhead();
    assert!(p.playing());
    assert_eq!(p.frame(), Frame::new(1_000));
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
    assert_eq!(p.frame(), Frame::new(2_000));
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
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let p = s.playhead();
    assert!(!p.playing());
    assert_eq!(
        (p.beat(), p.frame(), p.at()),
        (Beat(0.0), Frame::new(0), None)
    );
    assert_eq!(s.beat_now(), Beat(0.0));
}

/// **`set_loop` wraps a rolling render at the loop's end.** At 60 BPM a beat is
/// one second (48 000 frames): looping beats 0 to 1 and rolling for 1.5 s ends
/// at beat 0.5 (frame 72 000 is half a pass past the wrap at frame 48 000),
/// where an unlooped transport would read beat 1.5.
#[test]
fn set_loop_wraps_a_rolling_render_at_the_loops_end() {
    let looped = offline();
    looped.set_tempo(Bpm::of(60.0)).expect("not following");
    looped
        .set_loop(LoopRange::try_from(Beat(0.0)..Beat(1.0)).unwrap())
        .unwrap();
    looped.play().unwrap();
    render(&looped, 72_000);
    let beat = looped.transport().beat().get();
    assert!((beat - 0.5).abs() < 1e-3, "looped: {beat}");

    let free = offline();
    free.set_tempo(Bpm::of(60.0)).expect("not following");
    free.play().unwrap();
    render(&free, 72_000);
    let beat = free.transport().beat().get();
    assert!((beat - 1.5).abs() < 1e-3, "unlooped: {beat}");
}

/// **`clear_loop` stops the wrapping and keeps the bounds.** Looped over beats
/// 0 to 1, half a second in (beat 0.5), the loop is cleared: one more second
/// rolls on to beat 1.5 instead of wrapping to 0.5; the bounds stay as set.
#[test]
fn clear_loop_stops_wrapping_and_keeps_the_bounds() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0)).expect("not following");
    let range = LoopRange::try_from(Beat(0.0)..Beat(1.0)).unwrap();
    s.set_loop(range).unwrap();
    s.play().unwrap();
    render(&s, 24_000);
    s.clear_loop().unwrap();
    render(&s, 48_000);
    let beat = s.transport().beat().get();
    assert!((beat - 1.5).abs() < 1e-3, "{beat}");
    let region = s.transport().motion().loop_region();
    assert_eq!(region.bounds(), range, "the bounds are kept");
    assert!(!region.is_armed());
}

/// **A play timed at a beat while the transport is stopped is refused where it
/// is made.** A beat has a frame only while the transport rolls, so such a play
/// waits for a playback only it can start: sent straight to the transport it
/// is still pending, and the playhead has not moved, after three seconds
/// rendered. `Session::schedule` names that, for a play and for a locate that
/// rolls, sends nothing, and still takes a command that does not start playback
/// (a stop) and a play timed at a frame.
#[test]
fn a_play_at_a_beat_on_a_stopped_transport_is_refused() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0)).expect("not following");

    // What the refusal is for: the transport itself accepts it, and it never fires.
    s.transport()
        .motion()
        .schedule(At::Beat(Beat(1.0)), MotionEvent::Play)
        .unwrap();
    render(&s, 144_000);
    assert_eq!(s.transport().beat().get(), 0.0, "it never started");
    assert_eq!(s.transport().motion().scheduled_outstanding(), 1);
    s.transport().motion().cancel_scheduled();
    render(&s, 1_000);
    assert_eq!(s.transport().motion().scheduled_outstanding(), 0);

    assert_eq!(
        s.schedule(Beat(1.0), MotionEvent::Play),
        Err(ScheduleError::BeatWhileStopped {
            beat: Beat(1.0),
            event: MotionEvent::Play
        })
    );
    let roll = MotionEvent::play_from(Beat(0.0));
    assert_eq!(
        s.schedule(Beat(2.0), roll),
        Err(ScheduleError::BeatWhileStopped {
            beat: Beat(2.0),
            event: roll
        })
    );
    assert_eq!(
        s.transport().motion().scheduled_outstanding(),
        0,
        "nothing sent"
    );

    assert!(s.schedule(Beat(4.0), MotionEvent::stop_unfaded()).is_ok());
    assert!(s.schedule(Frame::new(48_000), MotionEvent::Play).is_ok());
}

/// **A play at a beat straight after `play()` is taken**, before any block has
/// rendered: the session knows it started the transport. It fires at its beat
/// (at 60 BPM, beat 1 is frame 48 000): pending after half a second, applied
/// after 1.25 s. A `stop()` after it makes a later beat play refused again.
#[test]
fn a_play_at_a_beat_right_after_play_is_taken_and_fires() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0)).expect("not following");
    s.play().unwrap();
    s.schedule(Beat(1.0), MotionEvent::Play).unwrap();
    // The play and the timed play, neither applied yet.
    assert_eq!(s.transport().motion().scheduled_outstanding(), 2);
    render(&s, 24_000);
    assert_eq!(s.transport().motion().scheduled_outstanding(), 1, "not yet");
    render(&s, 36_000);
    assert_eq!(s.transport().motion().scheduled_outstanding(), 0, "fired");

    s.stop(FadeOut::Immediate).unwrap();
    render(&s, 2_000);
    assert!(s.schedule(Beat(9.0), MotionEvent::Play).is_err());
}
