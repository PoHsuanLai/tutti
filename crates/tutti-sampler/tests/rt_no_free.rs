//! Regression gate: the pool's command drain must not free on the audio
//! thread, and a disk stream's ring is freed where the doc says (doc 025 §10
//! Q4).
//!
//! `tests/rt_no_alloc.rs` guards allocation, and says why it cannot see a
//! free: `assert_no_alloc` counts allocations only. This file counts
//! deallocations, with a global allocator that tallies every `dealloc` made
//! while a thread-local flag is up, so the drain's frees are counted and
//! nothing else's (the test harness's own threads included).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use tutti_codec::Wave;
use tutti_graph::contract::Direct;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::testing::settle;
use tutti_sampler::{
    ByHand, DiskStreamer, DiskVoice, LoopSetting, MemorySource, Placement, Playback, SlotId,
    Stream, Voice, VoiceCommand, VoicePool, VoiceSource,
};
use tutti_types::{Beat, Bpm, SamplePosition, SampleRate, Samples, StretchFactor};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static FREES: Cell<usize> = const { Cell::new(0) };
    /// The largest single free made while watching, in bytes.
    static BIGGEST: Cell<usize> = const { Cell::new(0) };
}

struct CountFrees;

unsafe impl GlobalAlloc for CountFrees {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // `try_with`: a thread tearing down its locals still frees.
        let _ = WATCHING.try_with(|w| {
            if w.get() {
                let _ = FREES.try_with(|f| f.set(f.get() + 1));
                let _ = BIGGEST.try_with(|b| b.set(b.get().max(layout.size())));
            }
        });
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountFrees = CountFrees;

/// Frees made on this thread while `f` runs.
fn frees_during(f: impl FnOnce()) -> usize {
    watch(f).0
}

/// How many frees, and the largest one in bytes, made on this thread while `f`
/// runs.
fn watch(f: impl FnOnce()) -> (usize, usize) {
    FREES.with(|c| c.set(0));
    BIGGEST.with(|c| c.set(0));
    WATCHING.with(|w| w.set(true));
    f();
    WATCHING.with(|w| w.set(false));
    (FREES.with(|c| c.get()), BIGGEST.with(|c| c.get()))
}

/// Adding voices, and replacing one, frees nothing in the drain. Three
/// `AddVoice`s — two stretched (their filters built by the sender), then a
/// second add at the first one's id, which replaces that slot — are drained
/// by one `process`, counting frees on this thread. The replaced slot comes
/// back through the retirement channel, where the control thread frees it.
///
/// Mutation (run): the drain unboxing the voice (`insert_voice_inner(id,
/// Box::new(*voice), …)`, the pre-fix `*voice`) → the command's box is freed
/// in the drain → fails. Mutation (run): a replaced slot dropped in place
/// (`self.voices.retain(|s| s.id != id)`, the pre-fix line) → its filter and
/// box are freed in the drain → fails.
#[test]
fn the_add_voice_drain_frees_nothing() {
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let wave = Arc::new(Wave::from_samples(
        SampleRate::new(48_000.0),
        &vec![0.5f32; 48_000],
    ));
    let (pool, handle) = VoicePool::new().with_handle();
    let mut rig = Direct::new(pool, SampleRate(48_000.0), 64);
    let env = t.env(64, SampleRate(48_000.0));
    let voice = || {
        Box::new(Voice {
            source: VoiceSource::Memory(
                MemorySource::placed(Arc::clone(&wave), Beat::new(0.0), None)
                    .expect("a placeable wave"),
            ),
            play: Playback {
                stretch: StretchFactor::new(2.0),
                ..Playback::default()
            },
        })
    };
    for id in [1, 2, 1] {
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(id),
                voice: voice(),
                stretch: None,
            })
            .expect("the command queue has room");
    }
    let frees = frees_during(|| {
        rig.block_in(&env);
    });
    assert_eq!(
        rig.node.voice_count(),
        2,
        "the third add replaced the first"
    );
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread; a box or a \
         replaced slot must travel to the control thread instead"
    );
    assert_eq!(
        handle.collect_retired(),
        1,
        "the replaced slot is retired to the control thread"
    );
}

// ---------------------------------------------------------------------------
// A disk stream's ring: who frees it (doc 025 §10 Q4).
// ---------------------------------------------------------------------------

/// A ring is at least 4 096 frames of the file's width in 4-byte slots, so a
/// free of this many bytes or more, in a block or a step, is a ring (the
/// writer's decoder and the voice's buffers are all smaller than this for a
/// stereo file under the default config).
const RING_BYTES: usize = 4_096 * 2 * 4;

/// A stereo WAV two seconds long.
fn write_wav(path: &std::path::Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..96_000 {
        let s = (i as f32 * 0.01).sin() * 0.25;
        w.write_sample(s).expect("writes");
        w.write_sample(s).expect("writes");
    }
    w.finalize().expect("writes");
}

/// A hand-stepped streamer streaming a fresh file, and the stream's handle and
/// reader.
fn stream() -> (
    tempfile::TempDir,
    DiskStreamer<ByHand>,
    Stream,
    tutti_sampler::StreamReader,
) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("two_seconds.wav");
    write_wav(&path);
    let mut streamer =
        DiskStreamer::manual(SampleRate(48_000.0), Default::default()).expect("builds");
    let opening = streamer.open(&path, Placement::HEAD);
    let (stream, reader) = settle(&mut streamer, opening).expect("the file opens");
    (dir, streamer, stream, reader)
}

/// **Dropping a `Stream` sends; the streamer frees.** With the voice's reader
/// already gone (so the streamer's writer is the ring's last holder), dropping
/// the handle on the control thread frees no ring, and the ring is freed in the
/// streamer's step that handles the close: the thread that steps, never the one
/// that drops the handle.
///
/// Mutation (run): `Drop for Stream` without its `Close` send -> the step
/// frees no ring and the second assertion fails. Mutation (run): `Drop for
/// Stream` freeing in place (sending nothing and holding the ring itself, via a
/// clone of it) -> the first assertion fails.
#[test]
fn dropping_a_stream_frees_the_ring_in_the_streamers_step() {
    let (_dir, mut streamer, stream, reader) = stream();
    drop(reader);

    let (_, biggest) = watch(|| drop(stream));
    assert!(
        biggest < RING_BYTES,
        "dropping the handle freed a {biggest}-byte block: the ring, on the dropping thread"
    );
    let (_, biggest) = watch(|| {
        let _ = streamer.step_once();
    });
    assert!(
        biggest >= RING_BYTES,
        "the close's step freed at most {biggest} bytes: the ring was not freed there"
    );
}

/// **A `Block` frees nothing when it ends** (doc 025 §10 Q4, the reader's
/// half). A block lends the stream's published control and holds it to its
/// end; the streamer publishes a new one meanwhile (a loop edit), which
/// retires the old one *behind* the block's loan. Dropping the block, on the
/// audio thread, returns the loan and frees nothing; the old control is freed
/// by the streamer's next sweep (`reclaim_control`), on its own thread. So a
/// `StreamReader` and its blocks are free-detecting-clean: the audio thread
/// never owns published state.
///
/// Mutation (run): `Block` holding an owning `Arc` of the control
/// (`ArcLend::current`) instead of the lent `Loan` -> the streamer's sweep
/// frees the cell's own reference, the block is the retired control's last
/// holder and frees it in its drop -> the first assertion fails. Mutation
/// (run): the cycle's `reclaim_control` loop removed -> the second assertion
/// fails (nothing frees the retired control).
#[test]
fn a_block_that_ends_after_a_publish_frees_nothing() {
    let (_dir, mut streamer, mut stream, mut reader) = stream();
    let none = || Samples::new(0)..Samples::new(0);
    let block = reader.block(Samples::new(0), [none(), none()]);
    stream
        .set_loop(LoopSetting::On {
            start: SamplePosition::new(1_000.0),
            end: SamplePosition::new(3_000.0),
            crossfade_frames: 0,
        })
        .expect("the streamer lives");
    let _ = streamer.step_once();

    let frees = frees_during(|| drop(block));
    assert_eq!(
        frees, 0,
        "ending the block freed {frees} time(s) on the audio thread"
    );
    let frees = frees_during(|| {
        let _ = streamer.step_once();
    });
    assert!(
        frees > 0,
        "the streamer's sweep freed nothing: the retired control leaked"
    );
}

/// A pool with the disk voice of a stream whose handle has been dropped and
/// closed, so the voice's reader is the ring's **last holder**: its drop
/// frees the ring wherever it happens.
fn pool_holding_the_last_holder(
    streamer: &mut DiskStreamer<ByHand>,
    stream: Stream,
    reader: tutti_sampler::StreamReader,
) -> (VoicePool, tutti_sampler::VoicePoolHandle) {
    let (pool, handle) = VoicePool::new().with_handle();
    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Disk(DiskVoice::from_reader(reader, Beat::new(0.0), None)),
                play: Playback::default(),
            }),
            stretch: None,
        })
        .expect("room");
    drop(stream);
    let _ = streamer.step_once();
    (pool, handle)
}

/// **The remove path: a voice's last holder of a ring is never dropped on the
/// audio thread.** The voice is removed through the handle; the drain frees
/// nothing (the slot goes over the retirement channel) and the control thread's
/// `collect_retired` frees the ring. Answers doc 025 §10 Q4 for the normal path:
/// no, a disk voice's drop is on the control thread.
///
/// Mutation (run): `retire` dropping the value in the drain instead of sending
/// it -> the drain frees the ring -> fails.
#[test]
fn a_removed_disk_voice_frees_its_ring_on_the_control_thread() {
    let (_dir, mut streamer, stream, reader) = stream();
    let (pool, handle) = pool_holding_the_last_holder(&mut streamer, stream, reader);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate(48_000.0), 64);
    let env = t.env(64, SampleRate(48_000.0));
    rig.block_in(&env);
    assert_eq!(rig.node.voice_count(), 1, "setup: the voice is in");

    handle.send(VoiceCommand::Remove(SlotId(1))).expect("room");
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 0);
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread"
    );
    assert_eq!(handle.drops().freed_in_callback, 0);

    let (_, biggest) = watch(|| {
        assert_eq!(handle.collect_retired(), 1);
    });
    assert!(
        biggest >= RING_BYTES,
        "the control thread's collection freed at most {biggest} bytes: no ring"
    );
}

/// **The degraded path: it does.** When the retirement channel is full (the
/// control thread has not collected), a removed voice is dropped in the
/// callback, counted as `freed_in_callback`, and a voice that is the ring's
/// last holder frees the ring there. This pins the doc's answer to §10 Q4: the
/// audio thread's "never frees" holds except on that counted path, for a
/// stream's ring as for anything else the voice holds. A `Stream` is not
/// involved: its handle is the control side's and was dropped first.
///
/// The channel is filled by replacing a slot 64 times without collecting: each
/// replaced slot is retired.
///
/// Mutation (run): the 64 replacing adds removed -> the channel has room, the
/// voice is retired, the drain frees nothing and `freed_in_callback` stays 0
/// -> fails.
#[test]
fn a_removed_disk_voice_on_a_full_retirement_channel_frees_its_ring_in_the_callback() {
    let (_dir, mut streamer, stream, reader) = stream();
    let (pool, handle) = pool_holding_the_last_holder(&mut streamer, stream, reader);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate(48_000.0), 64);
    let env = t.env(64, SampleRate(48_000.0));
    rig.block_in(&env);

    let wave = Arc::new(Wave::from_samples(
        SampleRate::new(48_000.0),
        &vec![0.5f32; 480],
    ));
    let memory = || {
        Box::new(Voice {
            source: VoiceSource::Memory(
                MemorySource::placed(Arc::clone(&wave), Beat::new(0.0), None)
                    .expect("a placeable wave"),
            ),
            play: Playback::default(),
        })
    };
    // 65 adds at one id: the first makes the slot, the next 64 each retire the
    // one before. Two drains: a drain takes at most 64 commands.
    for batch in [33, 32] {
        for _ in 0..batch {
            handle
                .send(VoiceCommand::AddVoice {
                    id: SlotId(2),
                    voice: memory(),
                    stretch: None,
                })
                .expect("room");
        }
        rig.block_in(&env);
    }
    assert_eq!(
        handle.drops().freed_in_callback,
        0,
        "setup: the channel took every retirement, and is now full"
    );

    handle.send(VoiceCommand::Remove(SlotId(1))).expect("room");
    let (frees, biggest) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(handle.drops().freed_in_callback, 1, "counted");
    assert!(frees > 0);
    assert!(
        biggest >= RING_BYTES,
        "the callback freed at most {biggest} bytes: the ring was not freed there"
    );
}
