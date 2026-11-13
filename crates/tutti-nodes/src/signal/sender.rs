//! [`SourceNode`]: the one node that sends a [`ControlSource`]'s legs to
//! control lanes (doc 024 §4.5a, amendment B3).

use tutti_graph::{
    Cost, Cx, Event, EventKind, Io, Node, Offset, ParamSegment, Prepare, Shape, Status,
};
use tutti_modulation::{ControlSource, Leg, SourceId};
use tutti_types::{ChannelLayout, Frame, SamplePosition, Samples};

use super::PlayWalk;

/// Events a source node sends per block, at most. Past it the node first
/// thins its grid (doubling the stride: a 4096-frame block's grid is 128
/// points), never a corner; past it with no grid left to thin it sends what
/// fits and re-sends where it stands at the next block's first frame.
pub const SOURCE_EVENT_CAPACITY: u32 = 256;

/// How a control source's node is wired: its cost, whether it has a MIDI
/// input, and how it reads its audio input.
pub trait SourcePorts: ControlSource {
    /// Its cost class.
    const COST: Cost;
    /// It has one MIDI event input (event input 0), read through
    /// [`ControlSource::heard`].
    const MIDI_IN: bool;

    /// Frames `from..to` of its audio input, in order, before the stops on
    /// `to`. A source with no audio input reads nothing.
    fn hear(&mut self, _io: &Io<'_>, _from: usize, _to: usize) {}
}

/// The leg last sent (or meant to be sent, when it was refused), and the
/// stream frame it was sent on.
#[derive(Clone, Copy, Debug)]
struct Flight {
    segment: ParamSegment,
    at: u64,
}

impl Flight {
    fn since(&self, now: u64) -> Samples {
        Samples(usize::try_from(now.saturating_sub(self.at)).unwrap_or(usize::MAX))
    }

    /// Its value at stream frame `now`.
    fn value(&self, now: u64) -> f32 {
        self.segment.raw_value_after(self.since(now))
    }

    /// It has arrived by `now`: it holds `to` from here on.
    fn arrived(&self, now: u64) -> bool {
        self.segment
            .arrives_after()
            .is_some_and(|a| self.since(now) >= a)
    }
}

/// A **control source's node**: walks the graph's play clock (`Cx::play_at`,
/// the node's input time) and sends what its [`ControlSource`] produces as
/// segments addressed `ParamAddr::Id(source.0)`, for every control lane that
/// reads `source`.
///
/// - **Its stops**, in this order on one frame: a play start (the source's
///   `start`), a corner the source named, a point of its [grid], and each
///   MIDI message or [`EventKind::Resync`] on its input. A fresh node is idle
///   already, so a play start on its first frame is none.
/// - **One leg per frame**: a later one on the same frame replaces it.
/// - **Silence is not sent twice**: a leg from 0 to 0, where what it sent
///   has arrived at 0, is not sent (nor before it has sent anything: a lane
///   reads an input that has sent nothing as 0).
/// - **Overflow** (more than [`SOURCE_EVENT_CAPACITY`] in a block): first its
///   grid's stride doubles until the block fits (a block past 8192 frames for
///   the control grid), never a corner; then what still does not fit is
///   dropped, and it re-sends where it stands, resumed, at the next block's
///   first frame.
/// - **A resync** ([`EventKind::Resync`] on its input) re-sends where it
///   stands, resumed, on the event's frame: for a lane that newly reads it.
///
/// [grid]: ControlSource::GRID
#[derive(Clone, Debug)]
pub struct SourceNode<S: ControlSource> {
    source: S,
    id: u32,
    /// Its audio input's width ([`ChannelLayout::EMPTY`]: none).
    input: ChannelLayout,
    flight: Option<Flight>,
    /// Re-send where it stands at the next block's first frame.
    owed: bool,
    /// The play clock's epoch at the last frame read; `None` while fresh.
    epoch: Option<u32>,
    /// Frames since its last reset: the clock its flight is timed on.
    position: u64,
}

impl<S: ControlSource> SourceNode<S> {
    /// A node sending `source`'s legs as `id`, reading `input`-wide audio.
    pub(crate) fn with_source(id: SourceId, source: S, input: ChannelLayout) -> Self {
        Self {
            source,
            id: id.0,
            input,
            flight: None,
            owed: false,
            epoch: None,
            position: 0,
        }
    }

    /// This node, sending where it stands on its first block's first frame
    /// (0, while it has sent nothing): for one that replaces another of the
    /// same [`SourceId`] (a declaration edit rebuilds it), since a lane holds
    /// the old node's last value until this one sends. A fork (the node
    /// reset) does not keep it.
    #[must_use]
    pub fn replacing(mut self) -> Self {
        self.owed = true;
        self
    }

    /// The source id its segments are addressed to.
    pub fn source(&self) -> SourceId {
        SourceId(self.id)
    }

    fn segment(&self, leg: Leg) -> ParamSegment {
        ParamSegment::foreign(
            self.id,
            leg.piece.from,
            leg.piece.to,
            Samples(0),
            leg.piece.law,
        )
        .resumed(SamplePosition(0.0), SamplePosition(leg.frames.get() as f64))
    }

    /// The value it has sent, at stream frame `now` (0 before any).
    fn level(&self, now: u64) -> f32 {
        self.flight.map_or(0.0, |f| f.value(now))
    }

    /// Where it stands at stream frame `now`, as the leg in flight resumed
    /// there (the same trajectory, so a lane reading it hears no change), or
    /// 0 before any.
    fn standing(&self, now: u64) -> ParamSegment {
        match self.flight {
            Some(f) => {
                let along = SamplePosition(f.since(now).get() as f64);
                f.segment
                    .resumed(f.segment.position() + along, f.segment.length())
            }
            None => self.segment(Leg::hold(0.0)),
        }
    }

    /// Whether `leg` is a 0 where 0 is held at `now`: silence is not sent
    /// twice. (Any other constant is sent: a lane may be new, or have
    /// thinned, and a held level is what a reader resynchronizes on.)
    fn silent(&self, leg: &Leg, now: u64) -> bool {
        let zero = 0.0f32.to_bits();
        if leg.piece.from.to_bits() != zero || leg.piece.to.to_bits() != zero {
            return false;
        }
        self.flight
            .is_none_or(|f| f.arrived(now) && f.value(now).to_bits() == zero)
    }
}

/// A block's output: the segment waiting on its frame, so a later one on the
/// same frame replaces it, written once the walk moves past it.
struct Out {
    pending: Option<(usize, ParamSegment)>,
    len: Samples,
    refused: bool,
}

impl Out {
    fn put(&mut self, io: &mut Io<'_>, i: usize, seg: ParamSegment) {
        if self.pending.is_some_and(|(p, _)| p != i) {
            self.flush(io);
        }
        self.pending = Some((i, seg));
    }

    fn flush(&mut self, io: &mut Io<'_>) {
        if let Some((i, seg)) = self.pending.take() {
            let pushed = Offset::new(i, self.len)
                .is_some_and(|at| io.event_out(0).push(Event::segment(at, seg)).is_ok());
            self.refused |= !pushed;
        }
    }
}

impl<S: SourcePorts> SourceNode<S> {
    fn send(&mut self, out: &mut Out, io: &mut Io<'_>, i: usize, seg: ParamSegment) {
        self.flight = Some(Flight {
            segment: seg,
            at: self.position + i as u64,
        });
        out.put(io, i, seg);
    }

    fn send_leg(&mut self, out: &mut Out, io: &mut Io<'_>, i: usize, leg: Option<Leg>) {
        if let Some(leg) = leg
            && !self.silent(&leg, self.position + i as u64)
        {
            let seg = self.segment(leg);
            self.send(out, io, i, seg);
        }
    }

    /// The grid's stride for this block: the source's, doubled while the
    /// block's grid points and `reserve` other stops would not fit.
    fn stride(&self, cx: &Cx<'_>, walk: &PlayWalk, len: usize, reserve: usize) -> Option<u64> {
        let mut stride = u64::from(S::GRID?.stride());
        let room = (SOURCE_EVENT_CAPACITY as usize).saturating_sub(reserve);
        while grid_points(cx, walk, len, self.epoch, stride) > room && stride < len as u64 {
            stride *= 2;
        }
        Some(stride)
    }
}

/// Grid points of `stride` in a block: frames whose play count is a multiple.
fn grid_points(cx: &Cx<'_>, walk: &PlayWalk, len: usize, prev: Option<u32>, stride: u64) -> usize {
    let mut n = 0;
    let mut starts = walk.starts(cx, len, prev).peekable();
    let mut s = 0;
    while s < len {
        if starts.peek() == Some(&s) {
            starts.next();
        }
        let e = starts.peek().copied().unwrap_or(len);
        let p = walk.at(cx, s).play.get();
        let first = p.div_ceil(stride) * stride - p;
        let span = (e - s) as u64;
        if first < span {
            n += ((span - first - 1) / stride + 1) as usize;
        }
        s = e;
    }
    n
}

impl<S: SourcePorts> Node for SourceNode<S> {
    /// Its audio input (if any) in, no audio out; a MIDI input if its source
    /// hears one, and one event output (the segments).
    fn shape(&self) -> Shape {
        let shape = if self.input == ChannelLayout::EMPTY {
            Shape::EMPTY
        } else {
            Shape::sink_unassigned(self.input)
        };
        shape
            .with_events(u16::from(S::MIDI_IN), 1)
            .with_event_capacity(SOURCE_EVENT_CAPACITY)
            .with_cost(S::COST)
            .with_no_note_events()
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.source.prepare(prepare.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let len = io.frames();
        if len == 0 {
            return Status::Modified;
        }
        let walk = PlayWalk::new(cx, len);
        let events = if S::MIDI_IN {
            io.events(0).as_slice()
        } else {
            &[]
        };
        let n_starts = walk.starts(cx, len, self.epoch).count();
        // Every other stop a block can hold: the owed re-send, the starts,
        // and per event its own leg and the corner it may set.
        let reserve = 2 + n_starts + 2 * events.len();
        let stride = self.stride(cx, &walk, len, reserve);
        let mut out = Out {
            pending: None,
            len: cx.env.block_len,
            refused: false,
        };
        if std::mem::take(&mut self.owed) {
            // Refused last block, or replacing a node: where it stands now.
            let seg = self.standing(self.position);
            self.send(&mut out, &mut io, 0, seg);
        }
        let mut starts = walk.starts(cx, len, self.epoch).peekable();
        let (mut ev, mut heard, mut s) = (0usize, 0usize, 0usize);
        while s < len {
            let start = starts.peek() == Some(&s);
            if start {
                starts.next();
            }
            let e = starts.peek().copied().unwrap_or(len);
            let reading = walk.at(cx, s);
            let base = reading.play.get();
            // The play frame of offset `i` in this epoch's span.
            let play = |i: usize| Frame::new(base + (i - s) as u64);
            if start {
                let leg = self.source.start();
                self.send_leg(&mut out, &mut io, s, leg);
            }
            // The first frame not yet searched for a stop.
            let mut q = s;
            loop {
                let corner = self
                    .source
                    .next_corner()
                    .map(|c| (s + c.get().saturating_sub(base) as usize).max(q))
                    .filter(|&i| i < e);
                let grid = stride
                    .map(|st| {
                        let p = base + (q - s) as u64;
                        q + (p.div_ceil(st) * st - p) as usize
                    })
                    .filter(|&i| i < e);
                let note = events.get(ev).map(|x| x.offset.index()).filter(|&i| i < e);
                let Some(p) = [corner, grid, note].into_iter().flatten().min() else {
                    break;
                };
                self.source.hear(&io, heard, p);
                heard = p;
                if corner == Some(p) {
                    let leg = self.source.corner(play(p));
                    self.send_leg(&mut out, &mut io, p, leg);
                }
                if let (true, Some(st)) = (grid == Some(p), stride) {
                    let leg = self.source.grid(play(p), Samples(st as usize));
                    self.send_leg(&mut out, &mut io, p, leg);
                }
                while let Some(x) = events.get(ev).filter(|x| x.offset.index() == p) {
                    match x.kind {
                        EventKind::Midi(ump) => {
                            let level = self.level(self.position + p as u64);
                            let leg = self.source.heard(play(p), level, ump);
                            self.send_leg(&mut out, &mut io, p, leg);
                        }
                        // A lane newly reads it: where it stands, resumed.
                        EventKind::Resync => {
                            let seg = self.standing(self.position + p as u64);
                            self.send(&mut out, &mut io, p, seg);
                        }
                        _ => {}
                    }
                    ev += 1;
                }
                q = p + 1;
            }
            self.source.hear(&io, heard, e);
            heard = e;
            s = e;
        }
        out.flush(&mut io);
        self.owed |= out.refused;
        self.epoch = Some(walk.at(cx, len - 1).epoch);
        self.position += len as u64;
        Status::Modified
    }

    fn reset(&mut self) {
        let _ = self.source.start();
        self.flight = None;
        self.owed = false;
        self.epoch = None;
        self.position = 0;
    }
}
