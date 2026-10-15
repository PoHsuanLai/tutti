//! [`ControlLaneNode`]: one target's [`ControlProgram`], evaluated on the
//! play-clock grid and sent to the target as curve segments (design doc 014
//! §5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tutti_core::ChannelLayout;
use tutti_graph::{
    Cost, Cx, Event, EventKind, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node,
    NodeParts, Offset, ParamRangeReader, ParamSegment, PlayReading, Prepare, SegmentShape, Shape,
    Status, Transport,
};
use tutti_mod::{Clock, ControlProgram, CurveSegment, ProgramTime, MAX_LANE_INPUTS};
use tutti_types::{
    AtomicF32, Beat, Frame, ParamAddr, ParamKey, RangeCell, RtPublish, SamplePosition, Samples,
    Tail, Unit,
};

use super::lane::segment_shape;

/// The control lane's grid step, in frames of the play clock: a lane ends a
/// piece on every frame whose play count is a multiple of this, and on every
/// corner between (doc 014 §5.3). Equal live and offline, because the play
/// clock is.
pub const CONTROL_STEP: u32 = 32;

/// Segments one lane sends per block, at most: a 4096-frame block's grid
/// (128 points) and as many corners. Past it the lane thins its grid and
/// counts it ([`LaneControls::thinned`]); past it with no grid left to thin,
/// it drops the block's last points and counts that
/// ([`LaneControls::truncated`]).
pub const LANE_EVENT_CAPACITY: u32 = 256;

/// How a lane builds a segment for its target: the typed key (or foreign
/// id) bound in, so the lane itself is not generic.
type MakeSegment = dyn Fn(f32, f32, SegmentShape) -> ParamSegment + Send + Sync;

/// Where a lane reads its target's range.
enum RangeSource {
    /// A native port's own cell (`Editor::param_range_cell`): the port
    /// clamps, the lane reads the span.
    Port(ParamRangeReader),
    /// The lane's own cell (a plugin parameter): the span, and the clamp.
    Own(RangeCell),
    /// None: an unranged target takes no modulation.
    None,
}

impl RangeSource {
    fn load(&self) -> (f32, f32) {
        match self {
            Self::Port(r) => {
                let r = r.load();
                (r.min, r.max)
            }
            Self::Own(c) => c.load(),
            Self::None => (f32::NEG_INFINITY, f32::INFINITY),
        }
    }
}

/// What a lane shares with its [`LaneControls`] and its fork source.
struct Shared {
    program: RtPublish<ControlProgram>,
    /// The last program published, for the fork (an `RtRef` must not leave
    /// the block, doc 014 §4.5). Control thread only.
    last: Mutex<Arc<ControlProgram>>,
    /// Bumped after every publish: how the lane tells a republish (a glide)
    /// from a block that reads the same program.
    generation: AtomicU64,
    knob: Option<Arc<AtomicF32>>,
    range: RangeSource,
    thinned: AtomicU64,
    truncated: AtomicU64,
    unranged: AtomicU64,
}

/// The handles a [`ControlLaneNode`]'s owner keeps: the program it
/// evaluates, its knob, its range (a plugin lane's), and its counters.
/// Cheap to clone; every clone reaches the same lane.
#[derive(Clone)]
pub struct LaneControls(Arc<Shared>);

impl LaneControls {
    /// Hands the lane `program`, from its next block's first frame, as a
    /// glide from the value in flight (doc 014 §4.2). Control thread only.
    pub fn publish(&self, program: ControlProgram) {
        self.publish_arc(Arc::new(program));
    }

    /// [`publish`](Self::publish) of a program already behind an `Arc`.
    pub fn publish_arc(&self, program: Arc<ControlProgram>) {
        let mut last = self.0.last.lock().unwrap_or_else(|e| e.into_inner());
        *last = Arc::clone(&program);
        self.0.program.publish(program);
        self.0.generation.fetch_add(1, Ordering::Release);
    }

    /// The program last published (or built with).
    pub fn program(&self) -> Arc<ControlProgram> {
        Arc::clone(&self.0.last.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Writes the knob the lane reads: a native lane's is its target's own
    /// `Param` cell (so this is a param write), a plugin lane's its own.
    /// `false` for a lane built without one.
    pub fn set_knob(&self, value: f32) -> bool {
        match &self.0.knob {
            Some(k) => {
                k.store(value, Ordering::Release);
                true
            }
            None => false,
        }
    }

    /// The knob now, if the lane has one.
    pub fn knob(&self) -> Option<f32> {
        self.0.knob.as_ref().map(|k| k.load(Ordering::Acquire))
    }

    /// Sets a plugin lane's range: the span its offsets scale by and the
    /// clamp, from the next block. `false` for a native lane, whose range is
    /// its port's (`Editor::set_param_range`), and for an unranged one.
    pub fn set_range(&self, min: f32, max: f32) -> bool {
        match &self.0.range {
            RangeSource::Own(c) => {
                c.store(min, max);
                true
            }
            _ => false,
        }
    }

    /// The range the lane reads, `(min, max)`; unbounded when it has none.
    pub fn range(&self) -> (f32, f32) {
        self.0.range.load()
    }

    /// Blocks whose grid the lane thinned to stay within
    /// [`LANE_EVENT_CAPACITY`] (doc 014 §5.3): each may differ between live
    /// and export.
    pub fn thinned(&self) -> u64 {
        self.0.thinned.load(Ordering::Relaxed)
    }

    /// Blocks in which the lane dropped points past [`LANE_EVENT_CAPACITY`]
    /// with no grid left to thin (more than that many corners in one block).
    pub fn truncated(&self) -> u64 {
        self.0.truncated.load(Ordering::Relaxed)
    }

    /// Blocks whose program had offsets while the range was not finite: the
    /// lane sent the base alone (doc 014 §4.2).
    pub fn unranged(&self) -> u64 {
        self.0.unranged.load(Ordering::Relaxed)
    }
}

/// The piece the target is on: the segment last sent, the frame it was
/// sent on, and the lane's next point.
#[derive(Clone, Copy)]
struct Flight {
    /// `None`: the lane had no value and sent nothing.
    seg: Option<ParamSegment>,
    /// The segment's raw endpoints (0 with no segment).
    from: f32,
    to: f32,
    sent: Frame,
    /// The next point; `None` while the value is steady (no point is due
    /// until something changes).
    next: Option<Frame>,
}

/// An event input's current segment.
#[derive(Clone, Copy)]
struct Input {
    source: u32,
    seg: ParamSegment,
    recv: Frame,
}

/// The state a block's walk moves on: copied before a walk, so a walk that
/// overflows is retried from the same place.
#[derive(Clone, Copy)]
struct Walk {
    flight: Option<Flight>,
    inputs: [Option<Input>; MAX_LANE_INPUTS],
}

/// What the lane saw at the end of its last block.
#[derive(Clone, Copy)]
struct Seen {
    generation: u64,
    knob: Option<u32>,
    range: (u32, u32),
    /// The frame after the last block, the transport and play clock at its
    /// last frame: how a seek between blocks is told from a roll.
    end: Option<(Frame, Transport, PlayReading)>,
}

/// One target's control lane: it reads a [`ControlProgram`] (published
/// through [`LaneControls`]), its target's knob and range, the transport
/// and play clock at its input's time, and its event input's signal
/// sources, and sends the target piecewise segments (doc 014 §5).
///
/// - **Points**, where a piece starts: every frame whose play count is a
///   multiple of [`CONTROL_STEP`]; every corner of the program (a
///   breakpoint, an edge, a peak, a zero crossing of a curved bipolar
///   layer, a rate anchor); every transport change, seek and loop wrap;
///   every input event; frame 0 when nothing is in flight or the program,
///   knob or range changed. At each point the lane sends a linear piece
///   from the value there to the value just before the next point.
/// - **A piece in flight continues** across a block edge untouched: the
///   lane does not re-send it, so what the target renders does not depend
///   on where blocks fall.
/// - **Exact mode.** A native target whose program is automation alone
///   receives the curve's own shaped pieces (as `AutomationLaneNode` sent
///   them), and a piece with no [`SegmentShape`] on the grid. A plugin lane
///   never uses it: a plugin receives points.
/// - **Steady.** When nothing moves (stopped with no seconds-clock layer,
///   or no layers at all, and every input arrived) the lane sends one held
///   value and no grid.
/// - **The clamp** is the native port's, per frame. A plugin lane clamps
///   its own pieces, split where they cross a bound.
///
/// No allocation on the audio thread: per-input state is a fixed array of
/// [`MAX_LANE_INPUTS`], and the block's events are built in a buffer
/// reserved at `prepare`.
pub struct ControlLaneNode {
    shared: Arc<Shared>,
    make: Arc<MakeSegment>,
    /// A native port (exact mode allowed; the port clamps).
    native: bool,
    walk: Walk,
    seen: Seen,
    scratch: Vec<Event>,
}

fn fresh_walk() -> Walk {
    Walk {
        flight: None,
        inputs: [None; MAX_LANE_INPUTS],
    }
}

fn fresh_seen() -> Seen {
    Seen {
        generation: u64::MAX,
        knob: None,
        range: (0, 0),
        end: None,
    }
}

impl ControlLaneNode {
    fn build(
        program: Arc<ControlProgram>,
        knob: Option<Arc<AtomicF32>>,
        range: RangeSource,
        make: Arc<MakeSegment>,
        native: bool,
    ) -> Self {
        let shared = Shared {
            program: RtPublish::from_arc(Arc::clone(&program)),
            last: Mutex::new(program),
            generation: AtomicU64::new(0),
            knob,
            range,
            thinned: AtomicU64::new(0),
            truncated: AtomicU64::new(0),
            unranged: AtomicU64::new(0),
        };
        Self {
            shared: Arc::new(shared),
            make,
            native,
            walk: fresh_walk(),
            seen: fresh_seen(),
            scratch: Vec::with_capacity(LANE_EVENT_CAPACITY as usize),
        }
    }

    /// A lane for built-in parameter `key`, wired to its param port with
    /// `ParamFrom::Events`: `program`'s values are `key`'s raw values.
    ///
    /// `knob` is the target node's own `Param` cell (`ParamSet::cell`), the
    /// base where the program has no automation value; `None` sends nothing
    /// there. `range` is the port's range cell
    /// (`Editor::param_range_cell`), read once per block for the span the
    /// program's offsets scale by; `None` is unranged, which sends the base
    /// alone.
    pub fn for_param<U: Unit<Raw = f32> + 'static>(
        key: ParamKey<U>,
        program: ControlProgram,
        knob: Option<Arc<AtomicF32>>,
        range: Option<ParamRangeReader>,
    ) -> Self {
        Self::build(
            Arc::new(program),
            knob,
            range.map_or(RangeSource::None, RangeSource::Port),
            Arc::new(move |from, to, shape| {
                ParamSegment::new(key, U::from_raw(from), U::from_raw(to), Samples(0), shape)
            }),
            true,
        )
    }

    /// A lane for a foreign unit's parameter `id` (a hosted plugin's), in
    /// its own normalisation: a knob of its own starting at `knob` (`None`:
    /// no knob), and a range of its own, `range` (`(min, max)`), which the
    /// lane clamps to. Never exact mode.
    pub fn for_foreign(
        id: u32,
        program: ControlProgram,
        knob: Option<f32>,
        range: (f32, f32),
    ) -> Self {
        Self::build(
            Arc::new(program),
            knob.map(|k| Arc::new(AtomicF32::new(k))),
            RangeSource::Own(RangeCell::new(range.0, range.1)),
            Arc::new(move |from, to, shape| ParamSegment::foreign(id, from, to, Samples(0), shape)),
            false,
        )
    }

    /// The lane's controls (also handed out by an insert).
    pub fn controls(&self) -> LaneControls {
        LaneControls(Arc::clone(&self.shared))
    }

    /// A fresh lane for a fork: the last published program, the knob and
    /// range as they are now in cells of its own, the same address, and no
    /// piece in flight (doc 014 §5.6). Control thread.
    pub fn fork_fresh(&self) -> Self {
        let s = &*self.shared;
        let program = Arc::clone(&s.last.lock().unwrap_or_else(|e| e.into_inner()));
        let knob = s
            .knob
            .as_ref()
            .map(|k| Arc::new(AtomicF32::new(k.load(Ordering::Acquire))));
        let range = match &s.range {
            RangeSource::None => RangeSource::None,
            r => {
                let (lo, hi) = r.load();
                RangeSource::Own(RangeCell::new(lo, hi))
            }
        };
        Self::build(program, knob, range, Arc::clone(&self.make), self.native)
    }
}

/// Frames per beat at `tempo` and `rate`, when both are usable.
fn frames_per_beat(rate: f64, tempo: f64) -> Option<f64> {
    let usable = tempo.is_finite() && tempo > 0.0 && rate.is_finite() && rate > 0.0;
    usable.then(|| rate * 60.0 / tempo)
}

/// The transport one frame on from `t`, as the executor's own history rolls
/// it (`FrameClock` advanced, loop wrapped).
fn roll_one(t: Transport, rate: tutti_types::SampleRate) -> Transport {
    if !t.playing || frames_per_beat(rate.get(), t.tempo.get()).is_none() {
        return t;
    }
    let mut clock = t.clock(rate);
    let region = t
        .looping
        .and_then(|l| tutti_types::LoopRange::new(l.start, l.end));
    clock.advance(Samples(1), region);
    Transport::counted(t.playing, t.tempo, clock.origin(), t.looping)
}

/// Whether `got` is `expected` (a roll of the last block's last frame):
/// equal play state, tempo and loop, and the beat within a nanobeat.
fn continues(expected: &Transport, got: &Transport) -> bool {
    let (a, b) = (expected.beat().get(), got.beat().get());
    expected.playing == got.playing
        && expected.tempo.get().to_bits() == got.tempo.get().to_bits()
        && expected.looping == got.looping
        && (a - b).abs() <= 1e-9 * a.abs().max(1.0)
}

/// A segment's value `j` frames (fractional) after its event, `to` once it
/// has arrived: the port's arithmetic in `f64`, for a time between frames.
fn value_along(seg: &ParamSegment, from: f32, to: f32, j: f64) -> f32 {
    let (p, l) = (seg.position().get(), seg.length().get());
    if !(l > 0.0 && l.is_finite()) {
        return to;
    }
    let t = ((p + j) / l) as f32;
    if t < 1.0 {
        from + (to - from) * seg.shape().eval(t)
    } else {
        to
    }
}

impl Input {
    /// The input's raw value at `at` frames after the block start of `base`
    /// (fractional): the port's own arithmetic on a frame, `f64` between.
    fn value(&self, at: f64, base: Frame) -> f32 {
        let since = base.get() as f64 + at - self.recv.get() as f64;
        let id = self.source;
        let (from, to) = (
            self.seg.foreign_start(id).unwrap_or(0.0),
            self.seg.foreign_target(id).unwrap_or(0.0),
        );
        if since.fract() == 0.0 && since >= 0.0 {
            self.seg.raw_value_after(Samples(since as usize))
        } else {
            value_along(&self.seg, from, to, since.max(0.0))
        }
    }

    /// Frames from `at` (after `base`) until the input arrives; `None` once
    /// it has.
    fn arrives_in(&self, at: f64, base: Frame) -> Option<f64> {
        let since = base.get() as f64 + at - self.recv.get() as f64;
        let arrive = self.seg.arrives_after()?.get() as f64;
        (arrive > since).then_some(arrive - since)
    }
}

/// Why a walk stopped short.
enum Overflow {
    /// The capacity filled with the grid at `stride`.
    Full,
}

/// One block's walk, borrowed together.
struct Block<'b> {
    cx: &'b Cx<'b>,
    program: &'b ControlProgram,
    knob: Option<f32>,
    range: (f32, f32),
    span: f64,
    native: bool,
    make: &'b MakeSegment,
    stride: u64,
    /// Thinning failed already: drop past the capacity instead.
    truncate: bool,
    seconds: bool,
}

impl Block<'_> {
    fn len(&self) -> usize {
        self.cx.env.block_len.get()
    }

    fn base(&self) -> Frame {
        self.cx.env.frame
    }

    /// The inputs' raw values at `at` frames into the block, in the
    /// program's input order.
    fn inputs_at(&self, walk: &Walk, at: f64) -> [Option<f32>; MAX_LANE_INPUTS] {
        let mut out = [None; MAX_LANE_INPUTS];
        for (k, l) in self.program.inputs().iter().enumerate() {
            out[k] = walk
                .inputs
                .iter()
                .flatten()
                .find(|i| i.source == l.source.0)
                .map(|i| i.value(at, self.base()));
        }
        out
    }

    /// The earliest arrival of an input still moving at `at`, in frames.
    fn inputs_moving(&self, walk: &Walk, at: f64) -> Option<f64> {
        self.program
            .inputs()
            .iter()
            .filter_map(|l| {
                walk.inputs
                    .iter()
                    .flatten()
                    .find(|i| i.source == l.source.0)
                    .and_then(|i| i.arrives_in(at, self.base()))
            })
            .min_by(f64::total_cmp)
    }

    /// Sends `seg` at frame `p`.
    fn send(&self, out: &mut Vec<Event>, p: usize, seg: ParamSegment) -> Result<(), Overflow> {
        if out.len() >= LANE_EVENT_CAPACITY as usize {
            return Err(Overflow::Full);
        }
        if let Some(at) = Offset::new(p, self.cx.env.block_len) {
            out.push(Event::segment(at, seg));
        }
        Ok(())
    }

    /// The point at frame `p`: one piece from the value there (or, gliding,
    /// from the value in flight) to the value just before the next point.
    fn point(
        &self,
        walk: &mut Walk,
        out: &mut Vec<Event>,
        p: usize,
        glide: bool,
    ) -> Result<(), Overflow> {
        let env = self.cx.env;
        let off = Offset::new(p, env.block_len).unwrap_or(Offset::ZERO);
        let t = self.cx.transport_at(off);
        let play = self.cx.play_at(off);
        let rate = env.sample_rate;
        let fpb = frames_per_beat(rate.get(), t.tempo.get()).filter(|_| t.playing);
        let abs = self.base() + Samples(p);
        let beat = t.beat().get();
        let time = ProgramTime {
            beat: t.beat(),
            play: SamplePosition(play.play.get() as f64),
            play_epoch: play.epoch,
            rate,
        };
        // The loop's wrap, a point of its own, in frames from here.
        let wrap = match (fpb, t.looping) {
            (Some(f), Some(l)) if l.end.get() > beat => Some((l.end.get() - beat) * f),
            _ => None,
        };
        let pf = p as f64;
        let moving = self.inputs_moving(walk, pf);
        let make = self.make;
        let next_at = |d: Option<f64>| d.map(|d| abs + Samples(d.ceil().max(1.0) as usize));
        let flight = |seg: Option<(ParamSegment, f32, f32)>, next: Option<f64>| Flight {
            seg: seg.map(|s| s.0),
            from: seg.map_or(0.0, |s| s.1),
            to: seg.map_or(0.0, |s| s.2),
            sent: abs,
            next: next_at(next),
        };
        // A glide that would re-send the piece in flight unchanged (same
        // end, same next point) leaves it alone: a republish that moves
        // nothing changes no frame.
        let old = walk.flight.filter(|f| glide && f.seg.is_some());
        let unchanged = |from: Option<f32>, to: f32, next: Option<f64>, seg: &ParamSegment| {
            old.is_some_and(|o| {
                o.to.to_bits() == to.to_bits()
                    && from.is_none_or(|f| {
                        f.to_bits() == o.from.to_bits()
                            && o.seg.is_some_and(|s| {
                                s.shape() == seg.shape() && s.length() == seg.length()
                            })
                    })
                    && o.next == next_at(next)
            })
        };

        // Exact mode: a native target's pure automation, rolling.
        if self.native && self.program.is_pure_automation() && moving.is_none() {
            let curve = self.program.automation().map(|c| &**c);
            if let (Some(curve), Some(f)) = (curve, fpb) {
                let exact = match curve.segment_at(t.beat()) {
                    Some(CurveSegment::Move {
                        start,
                        end,
                        from,
                        to,
                        shape,
                    }) if from.is_finite() && to.is_finite() && end.get() > beat => {
                        segment_shape(shape).map(|shape| {
                            let (s, e) = (start.get(), end.get());
                            let seg = make(from, to, shape).resumed(
                                SamplePosition((beat - s) * f),
                                SamplePosition((e - s) * f),
                            );
                            (seg, from, to, Some((e - beat) * f))
                        })
                    }
                    Some(CurveSegment::Hold { value, until }) if value.is_finite() => Some((
                        make(value, value, SegmentShape::Linear),
                        value,
                        value,
                        until.map(|u| (u.get() - beat) * f).filter(|d| *d > 0.0),
                    )),
                    None if curve.value_at(t.beat()).is_none() => {
                        // No value anywhere: the knob, held.
                        self.knob
                            .map(|k| (make(k, k, SegmentShape::Linear), k, k, None))
                    }
                    _ => None,
                };
                if let Some((seg, from, to, d)) = exact {
                    let d = match (d, wrap) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    if unchanged(Some(from), to, d, &seg) {
                        return Ok(());
                    }
                    self.send(out, p, seg)?;
                    walk.flight = Some(flight(Some((seg, from, to)), d));
                    return Ok(());
                }
            }
        }

        let ins = self.inputs_at(walk, pf);
        let n_in = self.program.inputs().len();
        let in_flight = walk.flight.and_then(|f| f.seg.map(|s| (s, f.sent)));
        let v0 = match (glide, in_flight) {
            (true, Some((seg, sent))) => {
                Some(seg.raw_value_after(abs.since(sent).unwrap_or(Samples(0))))
            }
            _ => self
                .program
                .value_at(self.knob, &time, self.span, &ins[..n_in]),
        };
        let layers = !self.program.layers().is_empty() || self.program.automation().is_some();
        let steady = moving.is_none() && (!layers || (fpb.is_none() && !self.seconds));
        let grid = (self.stride - play.play.get() % self.stride) as f64;
        let Some(v0) = v0 else {
            // No value: nothing sent, the target keeps its own; look again
            // at the next grid point.
            walk.flight = Some(flight(None, (!steady).then_some(grid)));
            return Ok(());
        };
        if steady {
            let v = if glide {
                self.program
                    .value_at(self.knob, &time, self.span, &ins[..n_in])
                    .map(|v| self.clamp(v))
            } else {
                Some(self.clamp(v0))
            };
            let seg = v.map(|v| make(v, v, SegmentShape::Linear));
            if let (Some(v), Some(seg)) = (v, seg) {
                if unchanged(None, v, None, &seg) {
                    return Ok(());
                }
                if !glide {
                    self.send(out, p, seg)?;
                    walk.flight = Some(flight(Some((seg, v, v)), None));
                    return Ok(());
                }
            }
        }

        // The next point: the grid, or a corner before it.
        let mut d = if steady { f64::INFINITY } else { grid };
        let corners = self.program.next_corner(&time);
        let mut consider = |x: Option<f64>| {
            if let Some(x) = x.filter(|x| *x > 0.0 && x.is_finite()) {
                d = d.min(x);
            }
        };
        if let Some(f) = fpb {
            consider(corners.beat.map(|b| (b.get() - beat) * f));
            consider(wrap);
        }
        consider(corners.play.map(|c| c.get() - time.play.get()));
        consider(moving);
        if !d.is_finite() {
            // Gliding into a steady value: one grid step.
            d = f64::from(CONTROL_STEP);
        }
        let end = ProgramTime {
            beat: Beat(fpb.map_or(beat, |f| beat + d / f)),
            play: SamplePosition(time.play.get() + d),
            ..time
        };
        let ins_end = self.inputs_at(walk, pf + d);
        let mut v1 = self
            .program
            .value_before(self.knob, &end, self.span, &ins_end[..n_in])
            .unwrap_or(v0);
        // A plugin lane clamps, split where the piece crosses a bound.
        if !self.native {
            let (lo, hi) = self.range;
            let (a, b) = (f64::from(v0), f64::from(v1));
            let cross = [lo, hi]
                .into_iter()
                .filter(|x| x.is_finite())
                .map(f64::from)
                .filter(|&x| (a - x) * (b - x) < 0.0)
                .map(|x| ((x - a) / (b - a), x))
                .min_by(|p, q| p.0.total_cmp(&q.0));
            if let Some((x, bound)) = cross {
                // The first frame at or past the crossing. `a` and `b` are
                // `f32`s, so a crossing on a frame can compute a hair past
                // it: a frame whose line value is the bound to `f32`
                // precision is the crossing's own.
                let line = |k: f64| a + (b - a) * k / d;
                let mut k = (x * d).ceil();
                let tol = bound.abs().max(1.0) * f64::from(f32::EPSILON);
                if k > 1.0 && (line(k - 1.0) - bound).abs() <= tol {
                    k -= 1.0;
                }
                if k >= 1.0 && k < d {
                    v1 = (a + (b - a) * k / d) as f32;
                    d = k;
                }
            }
        }
        let (v0, v1) = (self.clamp(v0), self.clamp(v1));
        let seg =
            make(v0, v1, SegmentShape::Linear).resumed(SamplePosition(0.0), SamplePosition(d));
        if unchanged(None, v1, Some(d), &seg) {
            return Ok(());
        }
        self.send(out, p, seg)?;
        walk.flight = Some(flight(Some((seg, v0, v1)), Some(d)));
        Ok(())
    }

    /// A plugin lane's clamp; a native port clamps for itself.
    fn clamp(&self, v: f32) -> f32 {
        let (lo, hi) = self.range;
        if self.native || !(lo <= hi) {
            v
        } else {
            v.clamp(lo, hi)
        }
    }

    /// The block's walk from `walk`: points in frame order into `out`.
    fn run(
        &self,
        walk: &mut Walk,
        out: &mut Vec<Event>,
        events: &[Event],
        at_zero: Option<bool>,
    ) -> Result<(), Overflow> {
        let env = self.cx.env;
        let len = self.len();
        let changes = env.changes.as_slice();
        let mut ev = 0usize;
        let mut from = 0usize;
        let mut cur = at_zero.map(|g| (0usize, g));
        loop {
            if let Some((p, glide)) = cur {
                // Input events on this frame land before its point.
                while let Some(e) = events.get(ev).filter(|e| e.offset.index() <= p) {
                    if let EventKind::Segment(seg) = e.kind {
                        if let ParamAddr::Id(id) = seg.addr() {
                            self.take_input(walk, id, seg, self.base() + Samples(e.offset.index()));
                        }
                    }
                    ev += 1;
                }
                if let Err(o) = self.point(walk, out, p, glide) {
                    if !self.truncate {
                        return Err(o);
                    }
                    // Full with no grid left to thin: the rest of the block
                    // sends nothing, and the next block starts with a point.
                    if let Some(f) = walk.flight.as_mut() {
                        f.next = Some(self.base() + Samples(p));
                    }
                    for e in &events[ev..] {
                        if let EventKind::Segment(seg) = e.kind {
                            if let ParamAddr::Id(id) = seg.addr() {
                                self.take_input(
                                    walk,
                                    id,
                                    seg,
                                    self.base() + Samples(e.offset.index()),
                                );
                            }
                        }
                    }
                    return Err(o);
                }
                from = p + 1;
            }
            let sched = walk
                .flight
                .and_then(|f| f.next)
                .and_then(|n| n.since(self.base()))
                .map(|s| s.get())
                .filter(|&i| i >= from && i < len);
            let change = changes
                .iter()
                .map(|c| c.at.index())
                .find(|&i| i >= from && i < len);
            let input = events
                .get(ev)
                .map(|e| e.offset.index())
                .filter(|&i| i < len);
            let next = [sched, change, input].into_iter().flatten().min();
            match next {
                Some(p) => cur = Some((p, false)),
                None => return Ok(()),
            }
        }
    }

    /// Records input `id`'s segment, received on frame `recv`, if the
    /// program reads that source (other events are ignored).
    fn take_input(&self, walk: &mut Walk, id: u32, seg: ParamSegment, recv: Frame) {
        if !self.program.inputs().iter().any(|l| l.source.0 == id) {
            return;
        }
        let slot = walk
            .inputs
            .iter()
            .position(|i| i.is_some_and(|i| i.source == id))
            .or_else(|| walk.inputs.iter().position(Option::is_none));
        if let Some(k) = slot {
            walk.inputs[k] = Some(Input {
                source: id,
                seg,
                recv,
            });
        }
    }
}

/// Whether any of `program`'s layers runs on the play clock.
fn has_seconds(program: &ControlProgram) -> bool {
    let secs = |c: &Clock| matches!(c, Clock::Seconds { .. });
    program
        .layers()
        .iter()
        .any(|l| secs(&l.clock) || l.depth_by.as_ref().is_some_and(|(c, _)| secs(c)))
}

impl Node for ControlLaneNode {
    /// No audio; one event input (signal sources), one event output
    /// (segments to the target).
    ///
    /// `Moderate`, from a measurement (doc 014 §5.3): 200 lanes of one
    /// moving layer each at 512-frame blocks spend about 3.7 µs per lane per
    /// block in `process` (a point's transport roll and the program's
    /// evaluation, sixteen grid points a block), about 7 ns a frame, where
    /// `Moderate`'s model is 200 + 8 a frame and `Light`'s 100 + 1.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(1, 1)
            .with_event_capacity(LANE_EVENT_CAPACITY)
            .with_cost(Cost::Moderate)
            .with_tail(Tail::Unbounded)
    }

    fn prepare(&mut self, _: &Prepare) {
        self.scratch.reserve(LANE_EVENT_CAPACITY as usize);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let env = cx.env;
        let len = env.block_len.get();
        if len == 0 {
            return Status::Modified;
        }
        let s = &*self.shared;
        let generation = s.generation.load(Ordering::Acquire);
        let program = s.program.read();
        let knob = s
            .knob
            .as_ref()
            .map(|k| k.load(Ordering::Acquire))
            .filter(|k| k.is_finite());
        let range = s.range.load();
        let span = f64::from(range.1) - f64::from(range.0);
        let span = if span.is_finite() && span >= 0.0 {
            span
        } else {
            f64::INFINITY
        };
        let offsets = !program.layers().is_empty() || !program.inputs().is_empty();
        if offsets && !span.is_finite() {
            s.unranged.fetch_add(1, Ordering::Relaxed);
        }
        let range_bits = (range.0.to_bits(), range.1.to_bits());
        let changed = generation != self.seen.generation
            || knob.map(f32::to_bits) != self.seen.knob
            || range_bits != self.seen.range;
        let t0 = cx.transport_at(Offset::ZERO);
        let p0 = cx.play_at(Offset::ZERO);
        let moved = match self.seen.end {
            Some((frame, t, play)) => {
                frame != env.frame
                    || !continues(&roll_one(t, env.sample_rate), &t0)
                    || play.after(Samples(1)) != p0
            }
            None => true,
        };
        let due = self
            .walk
            .flight
            .is_none_or(|f| f.next.is_some_and(|n| n <= env.frame));
        let at_zero = (due || changed || moved).then_some(changed && !moved);

        let events = io.events(0).as_slice();
        let mut block = Block {
            cx,
            program: &program,
            knob,
            range,
            span,
            native: self.native,
            make: &*self.make,
            stride: u64::from(CONTROL_STEP),
            truncate: false,
            seconds: has_seconds(&program),
        };
        let mut thinned = false;
        loop {
            self.scratch.clear();
            let mut walk = self.walk;
            match block.run(&mut walk, &mut self.scratch, events, at_zero) {
                Ok(()) => {
                    self.walk = walk;
                    break;
                }
                Err(Overflow::Full) if !block.truncate => {
                    if block.stride < len as u64 {
                        block.stride *= 2;
                        thinned = true;
                    } else {
                        block.truncate = true;
                    }
                }
                Err(Overflow::Full) => {
                    self.walk = walk;
                    s.truncated.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        if thinned {
            s.thinned.fetch_add(1, Ordering::Relaxed);
        }
        drop(block);
        drop(program);
        let out = io.event_out(0);
        for e in &self.scratch {
            // Within the declared capacity: never refused.
            let _ = out.push(*e);
        }
        let last = Offset::new(len - 1, env.block_len).unwrap_or(Offset::ZERO);
        self.seen = Seen {
            generation,
            knob: knob.map(f32::to_bits),
            range: range_bits,
            end: Some((
                env.frame + Samples(len),
                cx.transport_at(last),
                cx.play_at(last),
            )),
        };
        Status::Modified
    }

    fn reset(&mut self) {
        self.walk = fresh_walk();
        self.seen = fresh_seen();
    }
}

/// A lane's fork source: [`ControlLaneNode::fork_fresh`] at the gather.
struct LaneFork(ControlLaneNode);

impl ForkSource for LaneFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(self.0.fork_fresh())))
    }
}

/// Inserted with its [`LaneControls`], and a fork that reads the last
/// published program and the knob and range values at the gather.
impl IntoNode for ControlLaneNode {
    type Controls = LaneControls;

    fn into_parts(self) -> NodeParts<LaneControls> {
        let controls = self.controls();
        let fork = LaneFork(Self {
            shared: Arc::clone(&self.shared),
            make: Arc::clone(&self.make),
            native: self.native,
            walk: fresh_walk(),
            seen: fresh_seen(),
            scratch: Vec::new(),
        });
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}
