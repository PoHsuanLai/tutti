//! The step table: what each command does to the table, driven with no thread,
//! no streamer, no file and no queue.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use tutti_types::{ChannelLayout, SampleRate};

use super::*;
use crate::streamer::cache::LruCache;
use crate::streamer::control::StreamOrigin;
use crate::streamer::prefetch::RegionBuffer;
use crate::streamer::prefetch::{Ring, SharedReader};

/// A record on a fresh ring of `capacity` frames of `channels`, with the ring
/// and the reader's half, and the fork's view of it: what a stream's open
/// builds, minus the file.
pub(in crate::streamer) fn record(
    id: StreamId,
    path: PathBuf,
    capacity: usize,
    channels: impl Into<ChannelLayout>,
) -> (Record, SharedReader, tutti_rt::PosReader) {
    let (out, ring, pos) = RegionBuffer::with_capacity(id, path.clone(), capacity, channels);
    let cache = Arc::new(LruCache::new(4, 1 << 20));
    let pin = cache.pin(&path);
    let shared = Arc::new(StreamRecord::new(
        path,
        SampleRate(48_000.0),
        Arc::downgrade(&cache),
    ));
    let record = Record::new(
        out,
        Arc::new(RtState::new()),
        1_000,
        shared,
        pin,
        Arc::new(StatusCell::new()),
    );
    (record, ring, pos)
}

fn open(streams: &mut Streams, id: u64) -> (Weak<Ring>, StreamOrigin) {
    let (rec, ring, _pos) = record(StreamId(id), PathBuf::from("t.wav"), 1024, 1u16);
    let origin = StreamOrigin::new(Arc::clone(&rec.shared));
    let weak = Arc::downgrade(&ring);
    let _ = step(streams, Cmd::Open(Box::new(rec)));
    (weak, origin)
}

fn live(streams: &mut Streams, id: u64) -> (Weak<Ring>, StreamOrigin) {
    let opened = open(streams, id);
    let _ = step(streams, Cmd::Opened(StreamId(id)));
    opened
}

fn ask(start: u64, end: u64) -> Option<LoopAsk> {
    Some(LoopAsk {
        range: (start, end),
        crossfade_frames: 0,
    })
}

/// An open is a record in `Opening`, which a refill leaves alone and a loop
/// does not reach; its reader's delivery makes it `Live`, once.
///
/// Mutation (run): `Opened` not setting the phase → the stream stays
/// `Opening` and the loop below asks for nothing → fails. Mutation (run):
/// `Cmd::Loop` not checking the phase → the `Opening` stream is asked to loop
/// → fails.
#[test]
fn an_open_is_opening_until_its_reader_is_delivered() {
    let mut streams = Streams::new();
    let (_ring, _origin) = open(&mut streams, 1);
    let id = StreamId(1);
    assert_eq!(streams.len(), 1);
    assert_eq!(streams.get(id).unwrap().phase(), Phase::Opening);
    assert!(!streams.get(id).unwrap().is_live());
    assert!(
        step(&mut streams, Cmd::Loop(id, ask(0, 10))).is_empty(),
        "a stream whose reader was not delivered takes no loop"
    );

    assert_eq!(step(&mut streams, Cmd::Opened(id)), &[Effect::Live(id)]);
    assert_eq!(streams.get(id).unwrap().phase(), Phase::Live);
    assert!(
        step(&mut streams, Cmd::Opened(id)).is_empty(),
        "delivered once"
    );
    assert_eq!(
        step(&mut streams, Cmd::Loop(id, ask(0, 10))),
        &[Effect::Loop(id, ask(0, 10))]
    );
}

/// A close frees the stream's region in the step that handles it: the ring
/// (its writer, decoder and resident wave with it) is dead when the step
/// returns, the fork's record says the stream ended, and the table no longer
/// holds it. B1 restated: the region goes with the record, in one place.
///
/// Mutation (run): `Cmd::Close` finding the record without removing it → the
/// ring is alive after the step → fails. Mutation (run): `Drop for Record`
/// without `end()` → the fork still describes the stream → fails.
#[test]
fn a_close_frees_the_region_in_that_step() {
    let mut streams = Streams::new();
    let (ring, origin) = live(&mut streams, 1);
    let id = StreamId(1);
    assert!(ring.upgrade().is_some(), "setup: the writer holds the ring");
    assert!(origin.describe().is_some(), "setup: the stream runs");

    assert_eq!(step(&mut streams, Cmd::Close(id)), &[Effect::Freed(id)]);
    assert!(
        ring.upgrade().is_none(),
        "the closed stream's ring outlived its step"
    );
    assert!(streams.is_empty());
    assert!(origin.describe().is_none(), "a fork of it plays nothing");

    // A stream still `Opening` closes the same way (an abandoned open).
    let (ring, _origin) = open(&mut streams, 2);
    let _ = step(&mut streams, Cmd::Close(StreamId(2)));
    assert!(ring.upgrade().is_none());
}

/// A command for an id the table does not hold changes nothing and asks for
/// nothing, and never reaches another stream: B4. Ids are minted once, so the
/// stream that took a closed one's place has a new id, and what was sent to
/// the old one finds nobody.
///
/// Mutation (run): `Cmd::Loop` asking for the first live stream whatever id
/// it names → the stale loop reaches stream 2 → fails.
#[test]
fn a_command_for_an_unknown_stream_reaches_no_other() {
    let mut streams = Streams::new();
    let (ring_a, _) = live(&mut streams, 1);
    let _ = step(&mut streams, Cmd::Close(StreamId(1)));
    assert!(ring_a.upgrade().is_none());
    let (ring_b, _) = live(&mut streams, 2);

    assert!(
        step(&mut streams, Cmd::Loop(StreamId(1), ask(0, 10))).is_empty(),
        "a loop for the closed stream is dropped"
    );
    assert!(
        step(&mut streams, Cmd::Loop(StreamId(1), None)).is_empty(),
        "and so is a clear"
    );
    assert!(
        step(&mut streams, Cmd::Close(StreamId(1))).is_empty(),
        "closing twice frees nothing more"
    );
    assert!(step(&mut streams, Cmd::Opened(StreamId(1))).is_empty());
    assert_eq!(streams.len(), 1);
    assert!(ring_b.upgrade().is_some(), "the new stream is untouched");
    assert_eq!(
        step(&mut streams, Cmd::Loop(StreamId(2), ask(5, 9))),
        &[Effect::Loop(StreamId(2), ask(5, 9))],
        "its own id reaches it"
    );
}

/// Streams are independent: a loop for one asks for that one only, and
/// closing one leaves the other held, found by its own id after the
/// `swap_remove` has moved it.
///
/// Mutation (run): `Cmd::Close` removing the record at `(at + 1) % len` →
/// another stream is freed → fails.
#[test]
fn streams_are_independent() {
    let mut streams = Streams::new();
    let rings: Vec<_> = (1..=3).map(|i| live(&mut streams, i).0).collect();
    assert_eq!(
        step(&mut streams, Cmd::Loop(StreamId(2), ask(1, 2))),
        &[Effect::Loop(StreamId(2), ask(1, 2))]
    );
    let _ = step(&mut streams, Cmd::Close(StreamId(1)));
    assert!(rings[0].upgrade().is_none());
    assert!(rings[1].upgrade().is_some() && rings[2].upgrade().is_some());
    assert!(streams.get(StreamId(3)).is_some() && streams.get(StreamId(2)).is_some());
    assert_eq!(
        step(&mut streams, Cmd::Loop(StreamId(3), None)),
        &[Effect::Loop(StreamId(3), None)]
    );
}

/// An open whose id the table already holds is dropped, and freed, rather than
/// shadowing the stream held. Ids are minted once, so this cannot happen
/// through the control side; the step still refuses it.
///
/// Mutation (run): `Cmd::Open` pushing without the position check → two
/// records answer to one id → fails.
#[test]
fn a_duplicate_open_does_not_shadow() {
    let mut streams = Streams::new();
    let (first, _) = open(&mut streams, 1);
    let (second, _) = open(&mut streams, 1);
    assert_eq!(streams.len(), 1);
    assert!(first.upgrade().is_some());
    assert!(second.upgrade().is_none(), "the duplicate is freed");
}
