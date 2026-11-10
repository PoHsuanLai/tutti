use std::path::{Path, PathBuf};
use std::sync::Arc;

use tutti_types::{Beat, Bpm, SamplePosition};

use crate::LoopSetting;

use super::*;
use crate::testing::{MockTransport, play, prepared, settle};
use crate::{DiskStreamer, DiskVoice};

const RATE: f64 = 48_000.0;

/// A stereo float WAV of `frames` frames, every sample `value`. A dyadic value
/// is read back exactly, so a voice that plays it plays `value` to the bit.
fn write_constant(path: &Path, value: f32, frames: usize) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("a writable temp dir");
    for _ in 0..frames {
        w.write_sample(value).expect("writes");
        w.write_sample(value).expect("writes");
    }
    w.finalize().expect("writes");
}

fn manual() -> DiskStreamer<crate::ByHand> {
    DiskStreamer::manual(SampleRate::new(RATE), Default::default()).expect("builds")
}

/// A file of `value` in a temp dir.
fn constant_file(value: f32) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("constant.wav");
    write_constant(&path, value, 48_000);
    (dir, path)
}

/// One block of the voice playing `reader` from beat 0, planar.
fn first_block(reader: StreamReader) -> Vec<Vec<f32>> {
    let mut voice = prepared(
        DiskVoice::from_reader(reader, Beat(0.0), None),
        SampleRate::new(RATE),
        64,
    );
    let clock = MockTransport::rolling(Beat(0.0), Bpm::of(120.0));
    play(&mut voice, &clock, SampleRate::new(RATE), 64, 64)
}

/// `Placement` keeps the range-checked contract: `new`, `TryFrom` and serde
/// agree (serde as the bare `f64`), and a negative, NaN or infinite position
/// is refused, never clamped.
///
/// Mutation (each run): widening `new` to accept `-1.0` (`>= -1.0`) fails the
/// `-0.5` row; dropping the `is_finite` test fails the infinity row; replacing
/// the serde `try_from` with `transparent` lets `-0.5` deserialize and fails
/// the "deserialize accepted" check.
#[test]
fn placement_keeps_the_range_contract() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: |raw| Placement::new(SamplePosition(raw)),
        get: |placement| placement.get().get(),
        of: None,
        valid: &[0.0, 0.5, 4_000.0, 1e12],
        invalid: &[-0.5, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY],
    });
}

/// A fractional start is floored to the frame it falls in, and the stream's
/// window opens there: the reader plays from frame 4 000 for 4 000.7.
///
/// Mutation (run): `Placement::frame` rounding (`.round()`) -> frame 4 001
/// -> fails.
#[test]
fn a_fractional_start_opens_at_its_frame() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let start = Placement::new(SamplePosition(4_000.7)).expect("a position");
    assert_eq!(start.frame(), 4_000);
    let opening = streamer.open(&path, start);
    let (_stream, reader) = settle(&mut streamer, opening).expect("the file opens");
    assert_eq!(reader.ring.play(), 4_000);
}

/// Before the streamer has run, an open is `Pending` and hands the `Opening`
/// back; after it has, the poll is `Ready`. The reader is the reply: there is
/// nothing to poll for that was not asked for.
///
/// Mutation (run): `poll` answering `Ready` from a stream table in place of
/// the reply (it has none to read before the step) -> no `Pending` -> fails.
#[test]
fn an_open_is_pending_until_the_streamer_answers() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let id;
    let opening = {
        let opening = streamer.open(&path, Placement::HEAD);
        id = opening.id();
        match opening.poll() {
            Open::Pending(again) => again,
            other => panic!("answered before the streamer ran: {other:?}"),
        }
    };
    let _ = streamer.step_once();
    let Open::Ready(stream, reader) = opening.poll() else {
        panic!("the streamer ran and did not answer");
    };
    assert_eq!(reader.id(), id);
    assert_eq!(stream.id(), id, "the handle steers the stream it opened");
}

/// A `Ready` reader has registered its map reader already, on the control
/// side, and the voice built from it registers nothing more: the ring's map
/// has one reader after the open, and still one with the voice built and
/// playing, so nothing on the audio thread registers (which allocates).
///
/// Mutation (run): `DiskVoice::from_reader` registering a second reader
/// (`ring.map_reader()`) -> two readers -> fails.
#[test]
fn a_ready_reader_has_registered_its_map_before_it_is_returned() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let opening = streamer.open(&path, Placement::HEAD);
    let (_stream, reader) = settle(&mut streamer, opening).expect("the file opens");
    assert_eq!(reader.ring.map_readers(), 1, "registered by the open");
    let ring = Arc::clone(&reader.ring);
    let mut voice = prepared(
        DiskVoice::from_reader(reader, Beat(0.0), None),
        SampleRate::new(RATE),
        64,
    );
    let clock = MockTransport::rolling(Beat(0.0), Bpm::of(120.0));
    let _ = play(&mut voice, &clock, SampleRate::new(RATE), 64, 64);
    assert_eq!(ring.map_readers(), 1, "the voice registers none");
}

/// Variant `Io`: a path with no file fails the open with the kind the
/// filesystem reported, where the stream used to stay unopened for ever with no error.
///
/// Mutation (run): `unreadable` answering `Format` for a file it cannot open
/// -> fails.
#[test]
fn a_missing_file_fails_with_io() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut streamer = manual();
    let opening = streamer.open(&dir.path().join("nothing.wav"), Placement::HEAD);
    assert_eq!(
        settle(&mut streamer, opening).err(),
        Some(OpenError::Io(std::io::ErrorKind::NotFound))
    );
}

/// Variant `Format`: a file that opens but is not audio fails the open as
/// such.
///
/// Mutation (run): `unreadable` answering `Io` for a file it can open ->
/// fails.
#[test]
fn a_file_that_is_not_audio_fails_with_format() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("notes.wav");
    std::fs::write(&path, b"this is not a RIFF file, whatever its name says").expect("writes");
    let mut streamer = manual();
    let opening = streamer.open(&path, Placement::HEAD);
    assert_eq!(
        settle(&mut streamer, opening).err(),
        Some(OpenError::Format)
    );
}

/// Variant `Placement`: a wave with a channel no speaker names (bit 25 of the
/// channel mask is a second LFE) fails the open with the placement's refusal,
/// at the file's own width and at stereo. (`tests/disk_placement.rs` has the
/// rest of the placements, through the same open.)
///
/// Mutation (run): `open_stream_region` not building the `Place` (the open
/// answering `Ok` for the file) -> fails.
#[test]
fn a_wave_with_an_unnamed_channel_fails_with_placement() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("second_lfe.wav");
    write_extensible_wav(&path, 2, 0x0200_0001);
    let refused = OpenError::Placement(PlaceError::Fold(tutti_types::FoldError::Unnamed {
        end: tutti_types::FoldEnd::Source,
        channel: 1,
    }));
    let mut streamer = manual();
    let opening = streamer.open(&path, Placement::HEAD);
    assert_eq!(settle(&mut streamer, opening).err(), Some(refused.clone()));
    let opening = streamer.open_into(&path, Placement::HEAD, tutti_types::ChannelLayout::STEREO);
    assert_eq!(settle(&mut streamer, opening).err(), Some(refused));
}

/// A float32 `WAVE_FORMAT_EXTENSIBLE` WAV, `channels` wide, declaring `mask`.
fn write_extensible_wav(path: &Path, channels: u16, mask: u32) {
    let mut fmt = Vec::new();
    fmt.extend(0xFFFEu16.to_le_bytes());
    fmt.extend(channels.to_le_bytes());
    fmt.extend((RATE as u32).to_le_bytes());
    fmt.extend((RATE as u32 * 4 * u32::from(channels)).to_le_bytes());
    fmt.extend((4 * channels).to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(22u16.to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    fmt.extend([
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ]);
    let data: Vec<u8> = (0..8_192 * usize::from(channels))
        .flat_map(|_| 0.25f32.to_le_bytes())
        .collect();
    let mut out = b"RIFF".to_vec();
    out.extend((4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend((fmt.len() as u32).to_le_bytes());
    out.extend(&fmt);
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(&data);
    std::fs::write(path, out).expect("writes the file");
}

/// Variant `StreamerGone`: an open on a streamer whose thread is gone is never
/// answered, and says so at the first poll rather than staying `Pending` for
/// ever.
///
/// Mutation (run): `poll` answering `Pending` on a disconnected reply -> the
/// loop below never ends -> fails (it is bounded).
#[test]
fn an_open_on_a_gone_streamer_fails_with_streamer_gone() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    streamer.driver.gone();
    let mut opening = streamer.open(&path, Placement::HEAD);
    for _ in 0..1_000 {
        match opening.poll() {
            Open::Failed(error) => {
                assert_eq!(error, OpenError::StreamerGone);
                return;
            }
            Open::Pending(again) => opening = again,
            Open::Ready(..) => panic!("a gone streamer answered an open"),
        }
    }
    panic!("a gone streamer left an open pending");
}

/// A caller that drops its `Opening` abandons the open: the streamer that
/// answers into nobody closes the stream it opened, and keeps no record or
/// region for it.
///
/// Mutation (run): `handle_open` not closing the stream when the reply cannot
/// be sent -> the record is still held -> fails.
#[test]
fn an_abandoned_open_is_closed() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    drop(streamer.open(&path, Placement::HEAD));
    let _ = streamer.step_once();
    assert!(
        streamer.driver.open_streams() == 0,
        "the streamer kept a stream nobody can read"
    );
}

/// **B3, in its new form (doc 025 §1.6).** Two opens, the second made before
/// the streamer has run for the first: each `Opening` yields the reader of its
/// own open, the first is not the second, each plays its own file, and the
/// first ring is not freed (it is fed) while its stream is open and is freed
/// when its stream closes.
///
/// Before this cut, a voice taken by channel after a second open of that
/// channel was sent and before it was applied succeeded on the first stream's link: the
/// voice was bound to a ring the streamer then stopped feeding. There is no
/// such lookup now: a reader is the reply of the open it was asked from.
///
/// How it passes: each `Opening` owns its own reply cell, written once by the
/// streamer for that open's id, so `A.poll` can only return A's reader.
///
/// Mutation (run): `Opening::poll` reading one reply shared by the streamer's
/// opens, the newest overwriting (the channel's current link, as
/// the voice lookup read it) -> `first.poll` takes the second stream's
/// answer, so the second open is never answered (and had the polls been the
/// other way round, the readers' ids would be swapped) -> fails at "the second
/// open was not answered". (Recorded as a patch: `DiskStreamer` keeps the
/// newest open's `Receiver` in a field and `Opening::poll` receives from that.)
#[test]
fn two_opens_each_yield_the_reader_of_their_own_stream() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (path_a, path_b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    // Dyadic constants: voice A plays 0.25 and voice B 0.5, exactly.
    write_constant(&path_a, 0.25, 48_000);
    write_constant(&path_b, 0.5, 48_000);

    let mut streamer = manual();
    let first = streamer.open(&path_a, Placement::HEAD);
    let second = streamer.open(&path_b, Placement::HEAD);
    let (id_a, id_b) = (first.id(), second.id());
    assert_ne!(id_a, id_b);
    // The streamer has not run: neither is answered.
    let (Open::Pending(first), Open::Pending(second)) = (first.poll(), second.poll()) else {
        panic!("answered before the streamer ran");
    };

    let _ = streamer.step_until_settled(1_000);
    let Open::Ready(stream_a, a) = first.poll() else {
        panic!("the first open was not answered");
    };
    let Open::Ready(_stream_b, b) = second.poll() else {
        panic!("the second open was not answered");
    };
    assert_eq!(
        a.id(),
        id_a,
        "the first open's reader is the first stream's"
    );
    assert_eq!(
        b.id(),
        id_b,
        "the second open's reader is the second stream's"
    );
    assert!(!Arc::ptr_eq(&a.ring, &b.ring), "two streams, two rings");

    let ring_a = Arc::downgrade(&a.ring);
    let (block_a, block_b) = (first_block(a), first_block(b));
    assert!(
        block_a.iter().flatten().all(|&s| s == 0.25),
        "the first reader plays the first file: {:?}",
        &block_a[0][..4]
    );
    assert!(
        block_b.iter().flatten().all(|&s| s == 0.5),
        "the second reader plays the second file: {:?}",
        &block_b[0][..4]
    );

    // The first ring is still its stream's: the second open did not replace
    // it, and the streamer holds it until the stream closes.
    assert!(ring_a.upgrade().is_some(), "the first ring was freed early");
    drop(stream_a);
    let _ = streamer.step_once();
    assert!(
        ring_a.upgrade().is_none(),
        "the first ring outlived its stream"
    );
}

/// **B4 (doc 025 §1.6).** A command meant for one stream cannot reach another.
/// Two streams of one file: a loop set through A's handle is A's alone (B's
/// reader, and a fork of it, still see no loop); and then through B's, B's
/// alone.
///
/// Mutation (run): `handle_loop` applying the effect to the first record
/// (`records_mut().first_mut()`) instead of the one the effect names -> B's
/// loop is set instead of A's -> fails.
#[test]
fn a_loop_set_through_one_stream_leaves_the_other_unchanged() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let opening = streamer.open(&path, Placement::HEAD);
    let (mut a, reader_a) = settle(&mut streamer, opening).expect("the file opens");
    let opening = streamer.open(&path, Placement::HEAD);
    let (mut b, reader_b) = settle(&mut streamer, opening).expect("the file opens");
    let loop_of = |reader: &StreamReader| reader.origin.describe().expect("the stream runs").loop_;
    let looped = |start, end| LoopSetting::On {
        start: SamplePosition(start),
        end: SamplePosition(end),
        crossfade_frames: 0,
    };

    // B is opened second, so a command that went to "the newest" or to "the
    // first" would be A's or B's by accident; both directions are checked.
    a.set_loop(looped(1_000.0, 3_000.0)).expect("alive");
    let _ = streamer.step_until_settled(1_000);
    assert_eq!(loop_of(&reader_a), looped(1_000.0, 3_000.0));
    assert_eq!(loop_of(&reader_b), LoopSetting::Off, "B was not asked");

    b.set_loop(looped(500.0, 900.0)).expect("alive");
    let _ = streamer.step_until_settled(1_000);
    assert_eq!(loop_of(&reader_b), looped(500.0, 900.0));
    assert_eq!(
        loop_of(&reader_a),
        looped(1_000.0, 3_000.0),
        "A is as it was"
    );
}

/// A command for a stream the streamer no longer holds is ignored, and
/// reaches nobody: the handle of a stream that is gone commands nothing. A
/// stream closed by its handle's drop, then a stale handle (one a late
/// command could still be sent through) setting a loop beside a stream that
/// is open: the open one keeps the loop it had, and the streamer neither
/// panics nor keeps anything of the closed one.
///
/// Mutation (run): `handle_loop` applying the loop to the newest record
/// whatever the step answered -> B takes the stale loop -> fails.
#[test]
fn a_stopped_streams_handle_commands_nothing() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let opening = streamer.open(&path, Placement::HEAD);
    let (stopped, reader_a) = settle(&mut streamer, opening).expect("the file opens");
    let id_a = stopped.id();
    let opening = streamer.open(&path, Placement::HEAD);
    let (_b, reader_b) = settle(&mut streamer, opening).expect("the file opens");

    drop(stopped);
    drop(reader_a);
    let _ = streamer.step_once();
    assert_eq!(streamer.driver.open_streams(), 1, "A is closed, B is open");

    // The handle of the stream that is gone: what a late command still
    // reaching the queue looks like.
    let mut ghost = Stream::new(id_a, streamer.port());
    ghost
        .set_loop(LoopSetting::On {
            start: SamplePosition(1_000.0),
            end: SamplePosition(3_000.0),
            crossfade_frames: 0,
        })
        .expect("the queue is alive: the command is sent, and ignored");
    let _ = streamer.step_until_settled(1_000);
    assert_eq!(
        reader_b.origin.describe().expect("B runs").loop_,
        LoopSetting::Off,
        "a command for a closed stream reached another"
    );
    assert_eq!(streamer.driver.open_streams(), 1);
    drop(ghost);
    let _ = streamer.step_once();
    assert_eq!(
        streamer.driver.open_streams(),
        1,
        "the ghost's close found nothing to close"
    );
}
