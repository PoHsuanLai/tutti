//! The transport changes inside a chunk (doc 020 stage 5b), through the
//! node's own walk over a chunk: what the plugin is told for a chunk, and what
//! is counted when it does not fit. They were the served node's
//! (`tutti-plugin`'s old `graph_node`), driven through its chunk walk; the generic
//! node walks a chunk the same way (`TransportWalk` `begin`, `scan` and
//! `finish`), which `Rig::chunk` repeats.

use std::sync::Arc;

use tutti_graph::{
    Cx, Env, LoopRange, MAX_TRANSPORT_CHANGES, Offset, PlayClock, Transport, TransportChanges,
    TransportHistory,
};
use tutti_plugin_types::{Features, TransportChanges as WireChanges, TransportInfo};
use tutti_tempo::{Cursor, Shape, TempoMap, TempoPoint};
use tutti_types::{Beat, Bpm, Frame, Latency, SampleRate, Samples};

use crate::{
    Call, DeliveryCounters, ParamDelivery, PluginMeter, Snapshot, TransportSource, TransportWalk,
    transport_info,
};

const RATE: f64 = 48_000.0;

fn point(beat: f64, bpm: f64, shape: Shape) -> TempoPoint {
    TempoPoint {
        beat: Beat(beat),
        tempo: Bpm::of(bpm),
        shape,
    }
}

/// An engine block: `len` frames from `transport`, with the changes a
/// block carries as `(frame, transport)`.
fn block(len: usize, transport: Transport, changes: &[(usize, Transport)]) -> Env {
    let mut list = TransportChanges::NONE;
    for &(at, to) in changes {
        list.push(Offset::new(at, Samples(len)).expect("inside the block"), to)
            .expect("a change");
    }
    Env {
        frame: Frame(0),
        sample_rate: SampleRate(RATE),
        block_len: Samples(len),
        transport,
        changes: list,
        play: PlayClock::ZERO,
    }
}

fn rolling(bpm: f64, beat: f64) -> Transport {
    Transport::new(true, Bpm::of(bpm), Beat(beat), None)
}

/// What the plugin is told for a chunk: its transport at the chunk's first
/// frame, and the changes inside it, on the chunk's frames.
struct Told {
    transport: TransportInfo,
    changes: WireChanges,
}

/// The node's side of a plugin: what `process` keeps between pieces of a
/// chunk, and the chunk's calls.
struct Rig {
    counters: Arc<DeliveryCounters>,
    params: ParamDelivery,
    walk: TransportWalk,
    cursor: Cursor,
    source: TransportSource,
    tempo: TempoMap,
    arrival: Latency,
    info: TransportInfo,
    changes: WireChanges,
}

impl Rig {
    fn new(tempo: TempoMap) -> Self {
        let counters = Arc::new(DeliveryCounters::default());
        let mut source = TransportSource::new(PluginMeter::default().reader());
        source.go_offline();
        Self {
            params: ParamDelivery::new([], Arc::clone(&counters)),
            counters,
            walk: TransportWalk::default(),
            cursor: Cursor::default(),
            source,
            tempo,
            arrival: Latency::ZERO,
            info: TransportInfo::default(),
            changes: WireChanges::default(),
        }
    }

    /// One chunk of `chunk` frames, fed by one call for each env in
    /// `calls` (their lengths add up to the chunk's): what the plugin is
    /// told, walked as the node walks it (`begin` at the chunk's first
    /// frame, `scan` in each call, `finish` in the call that completes it).
    fn chunk(&mut self, chunk: usize, calls: &[Env]) -> Told {
        let mut at = 0;
        let mut told = None;
        for env in calls {
            let frames = env.block_len.get();
            let cx = Cx {
                env,
                arrival: self.arrival,
                history: TransportHistory::none(),
            };
            {
                let mut block = self.source.block(Features::TRANSPORT);
                let (meter, steady, project, block_ns) = block.parts();
                if at == 0 {
                    self.changes.clear();
                    let offset = Offset::new(0, Samples(frames)).expect("inside the block");
                    let mut s = Snapshot::at(&cx, offset, steady.at(0), project, block_ns);
                    let settled = s.settle(&self.tempo, &mut self.cursor);
                    self.walk.begin(&s, settled);
                    self.info = transport_info(&s, meter);
                }
                let call = Call {
                    cx: &cx,
                    frames,
                    steady,
                    block_ns,
                    meter,
                    tempo: &self.tempo,
                };
                let over = self.walk.scan(
                    &call,
                    &mut self.cursor,
                    project,
                    &mut self.info,
                    &mut self.changes,
                    0,
                    frames,
                    at,
                );
                at += frames;
                if at == chunk {
                    self.walk.finish(
                        &self.tempo,
                        &mut self.cursor,
                        env.sample_rate,
                        chunk,
                        &mut self.info,
                        &mut self.changes,
                    );
                    told = Some(Told {
                        transport: self.info,
                        changes: self.changes.clone(),
                    });
                }
                self.params.count_transport_dropped(over);
            }
            self.source.advance(frames);
        }
        told.expect("the calls fill the chunk")
    }

    fn dropped(&self) -> u64 {
        self.counters.dropped_transport_changes()
    }
}

/// The `(frame, tempo, slope)` of every entry of a payload, first included.
fn entries(p: &Told) -> Vec<(u32, f64, f64)> {
    std::iter::once((0, p.transport.timing.tempo, p.transport.timing.tempo_inc))
        .chain(p.changes.iter().map(|c| {
            (
                c.offset,
                c.transport.timing.tempo,
                c.transport.timing.tempo_inc,
            )
        }))
        .collect()
}

/// 120 BPM, then 90 from beat 4 (a step).
fn step_map() -> TempoMap {
    TempoMap::new([
        point(0.0, 120.0, Shape::Step),
        point(4.0, 90.0, Shape::Step),
    ])
    .expect("a map")
}

/// 120 -> 180 over four beats (tempo linear in time: 1.6 s, 76 800
/// frames, 60 / 76 800 BPM a frame), then 180 -> 60 over four more (2 s,
/// 96 000 frames, -120 / 96 000 a frame), then a step at 60.
fn ramps_map() -> TempoMap {
    TempoMap::new([
        point(0.0, 120.0, Shape::RampTime),
        point(4.0, 180.0, Shape::RampTime),
        point(8.0, 60.0, Shape::Step),
    ])
    .expect("a map")
}

/// **A step map point inside a chunk is an entry at the step's frame**,
/// carrying the step's tempo and no slope, while the stretch before it
/// keeps the tempo before. Beat 3.99 to the step at beat 4 is 0.01 beat at
/// 120 BPM, 0.005 s: frame 240, which is what the map says its
/// `frame_at_or_after` is relative to the chunk's first frame (96 000 less
/// 95 760). The engine's block carries the step as a tempo change at that
/// frame (to 90 BPM at beat 4), which is no entry of its own: one entry,
/// not two.
///
/// Mutation: in `Stretch::predict` ignore the map's points (the `point`
/// candidate `None`) -> the tempo-only `Env` change is no entry and none
/// is made -> no entry -> fails. Mutation: drop the filter that skips an
/// `Env` change that is the stretch going on -> the step is an entry
/// twice -> fails.
#[test]
fn a_step_inside_a_chunk_is_an_entry_at_its_frame() {
    let map = step_map();
    assert_eq!(
        map.frame_at_or_after(Beat(4.0), SampleRate(RATE)).get()
            - map.frame_at_or_after(Beat(3.99), SampleRate(RATE)).get(),
        240
    );
    let mut rig = Rig::new(map);
    let env = block(512, rolling(120.0, 3.99), &[(240, rolling(90.0, 4.0))]);
    let p = rig.chunk(512, &[env]);
    assert_eq!(entries(&p), [(0, 120.0, 0.0), (240, 90.0, 0.0)]);
    assert_eq!(p.changes[0].transport.position.quarters, 4.0);
    assert_eq!(rig.dropped(), 0);
}

/// **A ramp ending inside a chunk splits the slope at that frame**:
/// before it the ramp's analytic slope (60 / 76 800), after it the next
/// segment's (-120 / 96 000). Beat 3.99 is 1.5967 s into the first ramp
/// (`0.3125 s^2 + 2 s = 3.99`), 0.00334 s short of its end at 1.6 s:
/// frame 161 (160.2 rounds up).
///
/// Mutation: in `slope`, read the tempo one stretch ahead at the chunk's
/// end rather than held to the segment (`tempo_slope_with` replaced by the
/// 5a difference `tempo_at_seconds(t1) - tempo` over the whole chunk) ->
/// the first stretch reads the second ramp's falling tempo -> fails.
/// Mutation: in `close`, read the slope at the first beat of the chunk
/// (`Beat(0.0)` for `open.start.beat`) -> the second entry reads the
/// first ramp's slope -> fails.
#[test]
fn a_ramp_ending_inside_a_chunk_splits_the_slope() {
    let s0 = (-2.0 + (4.0f64 + 1.25 * 3.99).sqrt()) / 0.625;
    let frame = ((1.6 - s0) * RATE - 1e-6).ceil() as usize;
    assert_eq!(frame, 161);
    let mut rig = Rig::new(ramps_map());
    let env = block(512, rolling(150.0, 3.99), &[(frame, rolling(180.0, 4.0))]);
    let p = rig.chunk(512, &[env]);
    let got = entries(&p);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1].0, frame as u32);
    assert!((got[0].2 - 60.0 / 76_800.0).abs() < 1e-9, "{got:?}");
    assert!((got[1].1 - 180.0).abs() < 1e-9, "{got:?}");
    assert!((got[1].2 - (-120.0 / 96_000.0)).abs() < 1e-9, "{got:?}");
}

/// **A stretch with no point inside is told its slope** (5a zeroed it
/// whenever the chunk held a point; a chunk in the middle of a ramp has
/// the ramp's), and the engine's own cell edges (a tempo change every 128
/// frames of a ramp, in `Env::changes`) are no entries.
///
/// Mutation: `slope` returning `0.0` -> fails. Mutation: treat every
/// `Env` change as an entry (drop the `continues` test) -> the cell edges
/// are entries -> fails.
#[test]
fn a_ramp_chunk_has_its_slope_and_its_cell_edges_are_no_entries() {
    let mut rig = Rig::new(ramps_map());
    // Beat 1.8 is 0.8 s in: tempo 150, rolling at the cell's chord. The
    // edges at 128 and 256 continue the beat at nearly that tempo.
    let t = |frames: f64, tempo: f64| {
        let b = rig
            .tempo
            .beat_at_seconds(tutti_types::Seconds(0.8 + frames / RATE));
        rolling(tempo, b.get())
    };
    let env = block(
        384,
        rolling(150.0, 1.8),
        &[(128, t(128.0, 150.1)), (256, t(256.0, 150.2))],
    );
    let p = rig.chunk(384, &[env]);
    let got = entries(&p);
    assert_eq!(got.len(), 1, "no entry for a cell edge: {got:?}");
    assert!((got[0].1 - 150.0).abs() < 1e-6);
    assert!((got[0].2 - 60.0 / 76_800.0).abs() < 1e-9, "{got:?}");
}

/// **A start, a stop, a seek and a loop wrap inside a chunk are entries**,
/// each with the transport at its frame (play state, beat, loop), in
/// order.
///
/// Mutation: skip an `Env` change whose play state differs (the
/// `t.playing != self.playing` clause of `continues` removed) -> the stop
/// and the start are no entries -> fails. Mutation: drop the wrap
/// prediction (`wrap` candidate `None`) -> the wrap at frame 360 is no
/// entry -> fails.
#[test]
fn a_stop_a_start_a_seek_and_a_wrap_are_entries() {
    let looped = LoopRange {
        start: Beat(1.0),
        end: Beat(2.0),
    };
    let mut rig = Rig::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
    // 120 BPM: a frame is 1/24 000 beat. Playing from beat 1.5 in the loop
    // [1, 2]; stop at 100 (held at 1.5004), start again at 200, seek to
    // 1.9 at 300 (0.1 beat is 2 400 frames from the end: wrap at 2 700).
    let at = |f: usize| 1.5 + f as f64 / 24_000.0;
    let t0 = Transport::new(true, Bpm::of(120.0), Beat(1.5), Some(looped));
    let stopped = Transport::new(false, Bpm::of(120.0), Beat(at(100)), Some(looped));
    let started = Transport::new(true, Bpm::of(120.0), Beat(at(100)), Some(looped));
    let sought = Transport::new(true, Bpm::of(120.0), Beat(1.9), Some(looped));
    let env = block(3_000, t0, &[(100, stopped), (200, started), (300, sought)]);
    let p = rig.chunk(3_000, &[env]);
    let flags: Vec<_> = p
        .changes
        .iter()
        .map(|c| {
            (
                c.offset,
                c.transport.state.playing,
                c.transport.position.quarters,
            )
        })
        .collect();
    assert_eq!(flags.len(), 4, "{flags:?}");
    assert_eq!(flags[0], (100, false, at(100)));
    assert_eq!(flags[1], (200, true, at(100)));
    assert_eq!(flags[2], (300, true, 1.9));
    // 0.1 beat from 1.9 to the loop's end is 2 400 frames after the seek.
    assert_eq!(flags[3].0, 2_700);
    assert!(flags[3].1);
    assert!(
        (flags[3].2 - 1.0).abs() < 1e-6,
        "wrapped to the start: {flags:?}"
    );
    assert!(p.changes.iter().all(|c| c.transport.state.cycle_active));
}

/// **A chunk spanning two calls sees the second call's `Env` changes**: a
/// seek in the second call is an entry at the chunk frame it falls on
/// (the first call's length and its frame in the second), though `begin`
/// ran in the first call and saw only that call's `Env`.
///
/// Mutation: `take` not scanning (the `walk.scan` call removed) -> no
/// entry -> fails. Mutation: the entry's frame taken as the call's frame
/// (`p`) rather than the chunk's (`at + (p - from)`) -> 10, not 74 ->
/// fails.
#[test]
fn a_chunk_spanning_two_calls_sees_the_second_calls_seek() {
    let mut rig = Rig::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
    let first = block(64, rolling(120.0, 1.0), &[]);
    // The second call starts where the first ended (1 + 64 / 24 000) and
    // seeks to beat 8 at its frame 10.
    let second = block(
        64,
        rolling(120.0, 1.0 + 64.0 / 24_000.0),
        &[(10, rolling(120.0, 8.0))],
    );
    let p = rig.chunk(128, &[first, second]);
    assert_eq!(p.changes.len(), 1);
    assert_eq!(p.changes[0].offset, 74);
    assert_eq!(p.changes[0].transport.position.quarters, 8.0);
}

/// **A seek the second call's `Env` does not list, because the node reads
/// its input behind a latent path** (the change is in the executor's
/// history), is told at the second call's first frame: the transport there
/// is not the stretch continued.
///
/// Mutation: the `check_start` check removed -> no entry -> fails.
#[test]
fn a_seek_the_second_call_does_not_list_is_told_at_its_first_frame() {
    let mut rig = Rig::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
    let first = block(64, rolling(120.0, 1.0), &[]);
    let second = block(64, rolling(120.0, 8.0), &[]);
    let p = rig.chunk(128, &[first, second]);
    assert_eq!(p.changes.len(), 1);
    assert_eq!(p.changes[0].offset, 64);
    assert_eq!(p.changes[0].transport.position.quarters, 8.0);
    // And a second call that does continue is no entry.
    let first = block(64, rolling(120.0, 1.0 + 128.0 / 24_000.0), &[]);
    let second = block(64, rolling(120.0, 1.0 + 192.0 / 24_000.0), &[]);
    let p = rig.chunk(128, &[first, second]);
    assert!(p.changes.is_empty(), "{:?}", entries(&p));
}

/// **A change dropped past the cap taints the stretch it lands in**: the
/// eighth entry's slope is zero when a seek after it was dropped (no slope
/// describes a stretch the beat jumped in), while the entries before it
/// keep the ramp's.
///
/// Mutation: never set `Open::tainted` -> the eighth entry keeps the
/// ramp's slope -> fails.
#[test]
fn a_dropped_seek_leaves_its_stretch_without_a_slope() {
    let mut rig = Rig::new(ramps_map());
    let seeks: Vec<(usize, Transport)> = (0..8)
        .map(|i| (4 + 8 * i, rolling(150.0, 1.0 + 0.1 * i as f64)))
        .collect();
    let first = block(64, rolling(150.0, 0.5), &seeks);
    let second = block(
        64,
        rolling(150.0, 1.7 + 4.0 / 24_000.0),
        &[(10, rolling(150.0, 3.0))],
    );
    let p = rig.chunk(128, &[first, second]);
    let slopes: Vec<f64> = entries(&p).iter().map(|e| e.2).collect();
    assert_eq!(slopes.len(), 9);
    assert!((slopes[7] - 60.0 / 76_800.0).abs() < 1e-9, "{slopes:?}");
    assert_eq!(slopes[8], 0.0, "{slopes:?}");
    assert_eq!(rig.dropped(), 1);
}

/// **A map point dropped past the cap taints its stretch too**: the eighth
/// entry's stretch then crosses the ramp's end at beat 4, where
/// `tempo_slope_with` would extrapolate the ramp, so its slope is zero.
/// Eight seeks fill the list (beats 3.928 to 3.998, each a jump of 0.01
/// beat in 8 frames, where the ramp moves about 0.0005), and the point
/// some 32 frames after the eighth is the ninth change.
///
/// Mutation: taint only for a dropped seek, wrap, start or stop (skip
/// `Source::Predicted(Kind::Point(_))`) -> the eighth entry keeps the
/// ramp's slope -> fails.
#[test]
fn a_dropped_point_leaves_its_stretch_without_a_slope() {
    let mut rig = Rig::new(ramps_map());
    let seeks: Vec<(usize, Transport)> = (0..8)
        .map(|i| (4 + 8 * i, rolling(180.0, 3.928 + 0.01 * i as f64)))
        .collect();
    let only = block(128, rolling(180.0, 3.5), &seeks);
    let p = rig.chunk(128, &[only]);
    let slopes: Vec<f64> = entries(&p).iter().map(|e| e.2).collect();
    assert_eq!(slopes.len(), 9);
    assert!(slopes[7] > 0.0, "{slopes:?}");
    assert_eq!(slopes[8], 0.0, "{slopes:?}");
    assert_eq!(rig.dropped(), 1);
}

/// **A point is told once though the engine's beat lags it**: the
/// engine's cells hold a ramp's tempo a little under the curve, so at
/// the frame the map puts beat 4 the block's beat is an ulp short of it.
/// The block here runs at 170 BPM from beat 3.998 while the ramp nears
/// 180, so it is short by more: the point is one entry, not two.
///
/// Mutation: never set `Stretch::told` -> the stretch the point started
/// is before beat 4 and predicts it again a frame on -> two entries ->
/// fails.
#[test]
fn a_point_the_engine_reaches_late_is_one_entry() {
    let mut rig = Rig::new(ramps_map());
    let only = block(128, rolling(170.0, 3.998), &[]);
    let p = rig.chunk(128, &[only]);
    let e = entries(&p);
    assert_eq!(p.changes.len(), 1, "{e:?}");
    assert!(e[1].0 > 0 && e[1].0 < 128, "{e:?}");
    assert_eq!(rig.dropped(), 0);
}

/// **Nine changes in one chunk: eight are sent, one is counted, and the
/// ninth's transport is the next chunk's first.** Eight seeks in the
/// first call and a ninth in the second, a chunk of 128 frames.
///
/// Mutation: the list's `is_full` guard removed (push regardless) -> the
/// ninth panics the inline list (`ArrayVec::push`) -> fails. Mutation:
/// `count_transport_dropped` not called -> 0 counted -> fails. Mutation:
/// the ninth added at the expense of the first (keep the latest) -> the
/// list's first entry is the ninth's -> the order assertion fails.
#[test]
fn nine_changes_send_eight_count_one_and_the_ninth_waits_for_the_next_chunk() {
    assert_eq!(MAX_TRANSPORT_CHANGES, 8);
    let mut rig = Rig::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
    let seeks: Vec<(usize, Transport)> = (0..8)
        .map(|i| (4 + 8 * i, rolling(120.0, 10.0 + i as f64)))
        .collect();
    let first = block(64, rolling(120.0, 1.0), &seeks);
    let second = block(
        64,
        // After the eighth seek (frame 60 to beat 17), the second call
        // starts four frames on: beat 17 + 4 / 24 000.
        rolling(120.0, 17.0 + 4.0 / 24_000.0),
        &[(10, rolling(120.0, 50.0))],
    );
    let p = rig.chunk(128, &[first, second]);
    assert_eq!(p.changes.len(), MAX_TRANSPORT_CHANGES);
    let beats: Vec<_> = p
        .changes
        .iter()
        .map(|c| (c.offset, c.transport.position.quarters))
        .collect();
    let want: Vec<_> = (0..8)
        .map(|i| (4 + 8 * i as u32, 10.0 + i as f64))
        .collect();
    assert_eq!(beats, want);
    assert_eq!(rig.dropped(), 1);
    // The next chunk begins on the ninth's transport.
    let third = block(64, rolling(120.0, 50.0 + 54.0 / 24_000.0), &[]);
    let fourth = block(64, rolling(120.0, 50.0 + 118.0 / 24_000.0), &[]);
    let next = rig.chunk(128, &[third, fourth]);
    assert!((next.transport.position.quarters - (50.0 + 54.0 / 24_000.0)).abs() < 1e-9);
    assert!(next.changes.is_empty());
    assert_eq!(rig.dropped(), 1, "counted once");
}
