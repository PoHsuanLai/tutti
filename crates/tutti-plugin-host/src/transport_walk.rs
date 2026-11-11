//! The transport changes inside one chunk (doc 020, stage 5b).
//!
//! A chunk is the plugin node's FIFO batch, not the graph's block: it can span
//! several calls, and hold a tempo-map point, a seek, a loop wrap or a start
//! or stop. The chunk's own transport is the one at its first frame
//! (the node's `begin`); this walk adds an entry for each change after it,
//! at the chunk frame the change happens on, so a format with an inter-block
//! transport event (CLAP) can send one at that frame. Each entry's tempo and
//! slope hold over its own **stretch**, up to the next entry or the chunk's
//! end ([`slope`]).
//!
//! # What makes an entry
//!
//! - **A map point inside a playing stretch**, at the frame where the beat
//!   crosses it: the first frame at or after the point's time, by the engine's
//!   one placement rule ([`first_frame_at_or_after`]), measured from the
//!   stretch's own start (so a seek or a wrap before it is already in the
//!   stretch's start).
//! - **A loop wrap**, the same way, at the loop's end.
//! - **An [`Env`](tutti_graph::Env) change** that is not the stretch going on:
//!   a start or stop, a loop edit, a seek. The engine's own tempo changes
//!   (a map step or a ramp's cell edge) are in `Env::changes` too, and are
//!   not entries: a change is one only when the transport at its frame is not
//!   the stretch continued ([`Stretch::continues`], within a frame of beats).
//!
//! # How a chunk spanning calls sees the later calls' changes
//!
//! `begin` runs in the call that holds the chunk's first frame and can see
//! only that call's `Env`. So the walk is **incremental**: [`TransportWalk::scan`]
//! runs from `take`, once per call, for the frames that call puts into the
//! chunk, and reads that call's own `Env` (its changes, shifted to the node's
//! input time by its arrival latency, and the transport at any frame through
//! [`Cx::transport_at`](tutti_graph::Cx::transport_at)). A map point or a wrap
//! is predicted from the stretch the walk is on, so it is found in the call it
//! falls in, whichever call began the chunk. A change that reaches the node's
//! input in a later call than the one that rendered it (arrival latency past
//! the block's end: it is then in the executor's history, not this `Env`) is
//! found by the check at each continuing call's first frame (the transport
//! there is not the stretch continued), and told at that frame: late by up to
//! a call, never lost.
//!
//! # The cap
//!
//! At most [`MAX_TRANSPORT_CHANGES`] are
//! kept. One more is **counted** and its transport waits for the next chunk's
//! first frame, which reads the transport as it stands there. The walk goes
//! on past a dropped change (it re-seats on it without an entry), so each
//! further one is counted too and none is counted twice.

use tutti_graph::{Cx, MAX_TRANSPORT_CHANGES, Offset};
use tutti_tempo::{Cursor, TempoMap};
use tutti_types::{Beat, Samples, Seconds, first_frame_at_or_after};

use crate::transport_source::{ProjectTime, Snapshot, SteadyTime, slope, transport_info};
use tutti_plugin_types::{TransportChange, TransportChanges, TransportInfo};
use tutti_types::MeterMap;

/// What the walk reads of one call, and of the project, to place changes in
/// the call's frames.
///
/// Public for `tutti-plugin`'s node of a served plugin, which drives the same piece.
#[derive(Debug)]
pub struct Call<'a, 'c> {
    /// The call's context: its `Env`, arrival and the transport history.
    pub cx: &'a Cx<'c>,
    /// The call's length in frames.
    pub frames: usize,
    /// The steady-time counter at the call's first frame.
    pub steady: SteadyTime,
    /// The host clock at the call's first frame; `None` offline.
    pub block_ns: Option<i64>,
    /// The project's meter, for each entry's bar.
    pub meter: &'a MeterMap,
    /// The project's tempo map.
    pub tempo: &'a TempoMap,
}

/// Where the transport stands, from a chunk frame on, as far as the walk
/// follows it: the truth the next change is predicted from. Moves on every
/// change, kept or dropped.
#[derive(Clone, Copy, Debug)]
struct Stretch {
    /// The chunk frame it starts at.
    frame: usize,
    /// The beat there, as the map's tempo was read at it
    /// ([`Snapshot::settle`]).
    beat: Beat,
    playing: bool,
    recording: bool,
    looping: Option<(Beat, Beat)>,
    /// The map points at or before this beat were told already: a stretch a
    /// point started is at that point even when the engine's beat there is
    /// an ulp short of it (its cells lag a ramp), and must not predict it
    /// again.
    told: Beat,
}

/// The entry whose slope is not written yet: the last one kept. Its stretch
/// runs to the next kept entry (or the chunk's end), through any change that
/// was dropped.
#[derive(Clone, Copy, Debug)]
struct Open {
    /// `0` is the chunk's own transport; `k` is `changes[k - 1]`.
    index: usize,
    start: Stretch,
    /// A change was dropped inside it: a seek, a wrap, a start or stop (no
    /// slope describes a stretch the beat jumped in) or a map point (past a
    /// ramp's end its slope would extrapolate).
    tainted: bool,
}

/// What a predicted change is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The map point at this index of `TempoMap::points`.
    Point(usize),
    Wrap,
}

impl Stretch {
    fn of(s: &Snapshot, settled: Beat, frame: usize) -> Self {
        Self {
            frame,
            beat: settled,
            playing: s.playing,
            recording: s.recording,
            looping: s.looping,
            told: Beat(f64::NEG_INFINITY),
        }
    }

    /// The first change this stretch predicts, at a chunk frame after its
    /// start: the next map point past its beat, or its loop's end, whichever
    /// the playing beat reaches first (the wrap, on a tie). `None` when it is
    /// stopped or has none ahead.
    fn predict(&self, map: &TempoMap, cursor: &mut Cursor, rate: f64) -> Option<(usize, Kind)> {
        if !self.playing || !rate.is_finite() || rate <= 0.0 || !self.beat.get().is_finite() {
            return None;
        }
        let t0 = map.seconds_at_with(self.beat, cursor).get();
        let frames_to = |beat: Beat, cursor: &mut Cursor| {
            let ahead = (map.seconds_at_with(beat, cursor).get() - t0) * rate;
            // A change is on a frame after the stretch's own: one that rounds
            // onto the start was taken into it ([`Snapshot::settle`]).
            usize::try_from(first_frame_at_or_after(ahead).max(1)).ok()
        };
        let points = map.points();
        let past = self.beat.get().max(self.told.get());
        let next = points.partition_point(|p| p.beat.get() <= past);
        let point = points
            .get(next)
            .and_then(|p| frames_to(p.beat, cursor))
            .map(|f| (self.frame + f, Kind::Point(next)));
        let wrap = self
            .looping
            .filter(|(_, end)| end.get() > self.beat.get())
            .and_then(|(_, end)| frames_to(end, cursor))
            .map(|f| (self.frame + f, Kind::Wrap));
        match (point, wrap) {
            (Some(p), Some(w)) => Some(if w.0 <= p.0 { w } else { p }),
            (p, w) => p.or(w),
        }
    }

    /// Whether the transport `t`, read at chunk frame `frame`, is this stretch
    /// going on: the same play state, recording flag and loop, at the beat
    /// the map reaches from the stretch's start, to within two frames of beats
    /// (the executor's own rule for a continuation, so a tempo change, a map
    /// step or a ramp's cell edge, which is not a seek, is one).
    fn continues(
        &self,
        t: &tutti_graph::Transport,
        frame: usize,
        map: &TempoMap,
        cursor: &mut Cursor,
        rate: f64,
    ) -> bool {
        let looping = t.looping.map(|l| (l.start, l.end));
        if t.playing != self.playing || t.recording() != self.recording || looping != self.looping {
            return false;
        }
        let there = t.beat().get();
        if !there.is_finite() {
            return false;
        }
        let (predicted, slack) = if self.playing && rate.is_finite() && rate > 0.0 {
            let t0 = map.seconds_at_with(self.beat, cursor).get();
            let at = Seconds(t0 + (frame - self.frame) as f64 / rate);
            let beat = map.beat_at_seconds_with(at, cursor);
            let per_frame = map.tempo_at_with(beat, cursor).get().max(t.tempo.get()) / 60.0 / rate;
            (beat.get(), 2.0 * per_frame)
        } else {
            // Stopped: it does not move.
            (self.beat.get(), 1e-9)
        };
        (there - predicted).abs() <= slack
    }
}

/// The walk over the chunk being filled. One per node, reset by every
/// [`begin`](Self::begin); nothing here allocates.
///
/// Public for `tutti-plugin`'s node of a served plugin, which drives the same piece.
#[derive(Debug, Default)]
pub struct TransportWalk {
    stretch: Option<Stretch>,
    open: Option<Open>,
}

impl TransportWalk {
    /// A chunk begins with `first` as its transport (already settled, at
    /// `settled`): the walk is at its first frame. Not called for a plugin
    /// told no transport, which then has no walk ([`scan`](Self::scan) and
    /// [`finish`](Self::finish) do nothing).
    pub fn begin(&mut self, first: &Snapshot, settled: Beat) {
        let start = Stretch::of(first, settled, 0);
        self.stretch = Some(start);
        self.open = Some(Open {
            index: 0,
            start,
            tainted: false,
        });
    }

    /// No transport for this chunk.
    pub fn clear(&mut self) {
        self.stretch = None;
        self.open = None;
    }

    /// The call's frames `from..from + n` go into the chunk at its frame
    /// `at`: add an entry to `changes` for each change in them, in order.
    /// Returns how many changes did not fit (counted by the caller).
    #[allow(
        clippy::too_many_arguments,
        reason = "the walk's inputs are the call, the project and the chunk"
    )]
    pub fn scan(
        &mut self,
        call: &Call<'_, '_>,
        cursor: &mut Cursor,
        project: &mut ProjectTime,
        chunk_transport: &mut TransportInfo,
        changes: &mut TransportChanges,
        from: usize,
        n: usize,
        at: usize,
    ) -> u64 {
        let Some(mut stretch) = self.stretch else {
            return 0;
        };
        let rate = call.cx.env.sample_rate;
        let end = from + n;
        let arrival = call.cx.arrival.samples().get();
        let env = call.cx.env.changes.as_slice();
        // The node reads its input `arrival` frames after the graph rendered
        // it: an `Env` change at frame `e` reaches the input at `e + arrival`.
        let mut next_env = env
            .iter()
            .position(|c| c.at.index() + arrival >= from)
            .unwrap_or(env.len());
        // A continuing chunk's first frame in this call is checked for a
        // change it was not told of (see the module docs).
        let mut check_start = at > 0;
        let mut dropped = 0;
        loop {
            let env_at = env
                .get(next_env)
                .map(|c| c.at.index() + arrival)
                .filter(|&p| p < end);
            let start_at = check_start.then_some(from);
            let predicted = stretch.predict(call.tempo, cursor, rate.get());
            // Call frames; a prediction is a chunk frame, in this call once it
            // is before the call's end.
            let predicted_at = predicted
                .map(|(c, kind)| (from + c.saturating_sub(at), kind))
                .filter(|&(p, _)| p < end && p >= from);
            let first = [
                start_at.map(|p| (p, Source::Start)),
                env_at.map(|p| (p, Source::Env)),
                predicted_at.map(|(p, kind)| (p, Source::Predicted(kind))),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|&(p, source)| (p, source.rank()));
            let Some((p, source)) = first else {
                break;
            };
            match source {
                Source::Start => check_start = false,
                Source::Env => next_env += 1,
                Source::Predicted(_) => {}
            }
            let frame = at + (p - from);
            let Some(offset) = Offset::new(p, Samples(call.frames)) else {
                continue;
            };
            let t = call.cx.transport_at(offset);
            let is_change = match source {
                Source::Start | Source::Env => {
                    frame > stretch.frame
                        && !stretch.continues(&t, frame, call.tempo, cursor, rate.get())
                }
                // A prediction is a change when the engine's transport is
                // there: a point always, a wrap only once the beat has jumped
                // back (a wrap an ulp early is the stretch going on).
                Source::Predicted(Kind::Point(_)) => true,
                Source::Predicted(Kind::Wrap) => t.beat().get() < stretch.beat.get(),
            };
            if !is_change {
                if let Source::Predicted(Kind::Wrap) = source {
                    // Re-seat past it so the next prediction is after this
                    // frame, without an entry.
                    stretch.beat = t.beat();
                    stretch.frame = frame;
                    // `told` stays: the beat here is past any point told.
                }
                continue;
            }
            // The change's own transport, with the tempo the map has there.
            let mut s = Snapshot::at(call.cx, offset, call.steady.at(p), project, call.block_ns);
            let settled = s.settle(call.tempo, cursor);
            let mut next = Stretch::of(&s, settled, frame);
            if let Source::Predicted(Kind::Point(i)) = source
                && let Some(point) = call.tempo.points().get(i)
            {
                next.told = point.beat;
            }
            if changes.is_full() {
                dropped += 1;
                // No slope describes a stretch the beat jumped in, nor one
                // that crosses a map point (the ramp's would extrapolate).
                if let Some(open) = self.open.as_mut() {
                    open.tainted = true;
                }
                stretch = next;
                continue;
            }
            self.close(call.tempo, cursor, rate, frame, chunk_transport, changes);
            let info = transport_info(&s, call.meter);
            changes.push(TransportChange {
                offset: u32::try_from(frame).unwrap_or(u32::MAX),
                transport: info,
            });
            self.open = Some(Open {
                index: changes.len(),
                start: next,
                tainted: false,
            });
            stretch = next;
        }
        self.stretch = Some(stretch);
        dropped
    }

    /// The chunk of `chunk` frames is complete: write the last entry's slope.
    pub fn finish(
        &mut self,
        map: &TempoMap,
        cursor: &mut Cursor,
        rate: tutti_types::SampleRate,
        chunk: usize,
        chunk_transport: &mut TransportInfo,
        changes: &mut TransportChanges,
    ) {
        self.close(map, cursor, rate, chunk, chunk_transport, changes);
        self.clear();
    }

    /// The open entry's stretch ends at chunk frame `end`: its slope.
    fn close(
        &mut self,
        map: &TempoMap,
        cursor: &mut Cursor,
        rate: tutti_types::SampleRate,
        end: usize,
        chunk_transport: &mut TransportInfo,
        changes: &mut TransportChanges,
    ) {
        let Some(open) = self.open.take() else {
            return;
        };
        let inc = if open.tainted {
            0.0
        } else {
            slope(
                map,
                cursor,
                open.start.beat,
                open.start.playing,
                open.start.looping.map(|(_, end)| end),
                rate,
                end.saturating_sub(open.start.frame),
            )
        };
        let info = match open.index {
            0 => chunk_transport,
            k => &mut changes[k - 1].transport,
        };
        info.timing.tempo_inc = inc;
    }
}

/// Where a candidate frame came from, and the order they take on one frame:
/// the call's first-frame check, then an `Env` change, then a prediction.
#[derive(Clone, Copy, Debug)]
enum Source {
    Start,
    Env,
    Predicted(Kind),
}

impl Source {
    fn rank(self) -> u8 {
        match self {
            Self::Start => 0,
            Self::Env => 1,
            Self::Predicted(_) => 2,
        }
    }
}

const _: () = assert!(MAX_TRANSPORT_CHANGES > 0);
