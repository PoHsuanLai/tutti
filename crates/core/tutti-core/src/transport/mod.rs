//! Musical time: where the playhead is, how fast it moves, and what moves it.
//!
//! Split by *who decides*. [`TransportSettings`] holds the plain values (tempo,
//! loop region, recording) that anyone may store into; [`MotionFsm`] holds the
//! state machine that may reject or defer a play/stop/locate. [`Transport`] is
//! the two halves as one handle, and [`TransportClock`] is the graph node that
//! turns them into a per-sample beat signal. [`EnvClock`] is the same signal
//! on the native graph, read from each block's `Env`.
//!
//! The traits below are the read side: [`Timeline`] is what a node consults,
//! [`RenderClock`] is what a renderer drives, and [`TransportState`] adds the
//! live-session facts (record, loop) a hosted plugin asks for.

mod beat_window;
mod click;
mod clock;
mod env_clock;
pub(crate) mod fsm;
mod handle;
mod motion;
mod offline;
mod settings;
mod state;
mod timed;

pub use beat_window::{BeatCursor, BeatWindow, BeatWindowSync};
pub use click::{ClickNode, ClickSettings, ClickState, MetronomeMode};
pub use clock::TransportClock;
pub(crate) use clock::{tempo_in_effect, Control};
pub use env_clock::EnvClock;
pub use handle::Transport;
pub use motion::{FadeOut, MotionEvent, MotionFsm, MotionState, QueueFull, Then};
pub use offline::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
pub use settings::TransportSettings;
pub use state::{
    beat_from_ports, beats_per_sample, ClockLinks, Declick, LoopRange, LoopSpan, SeekSlot,
    BEAT_PORTS,
};
pub(crate) use timed::Scheduled;
pub use timed::{ScheduleFull, TransportCommand, SCHEDULE_CAPACITY};

// The Bevy resource wrappers (`TransportRes` / `MetronomeRes`) belong to the
// host adapter, `bevy_tutti::graph`, not to this crate.

/// A musical timeline: the position, the tempo, and whether it is moving.
///
/// Implemented by the live [`Transport`] and by [`OfflineTimeline`], so a
/// beat-driven source can be handed either one and not care which.
///
/// # When to use this instead of a beat edge
///
/// Pure DSP nodes should **not** implement against this trait. A node that is a
/// function of musical time takes the beat as a signal on its input ports (see
/// [`BEAT_PORTS`]) — that is per-sample accurate, works unchanged offline, and
/// makes the timeline→node relationship a visible graph edge.
///
/// What legitimately remains here is what a beat signal cannot express:
///
/// - **Boolean gating** — `is_rolling` drives early returns with state-reset
///   side effects. "Emit nothing" is not the same as "emit a level", and a
///   paused timeline still has a valid beat, so rolling-ness is not
///   recoverable from the beat.
/// - **Nodes with no ports** — the MIDI sources implement `poll_into` and have
///   no `BufferRef` to read a beat from.
///
/// # What is deliberately NOT here
///
/// Recording, looping, and preroll are live-session facts, not timeline facts:
/// an offline render either answers `false`/`None` forever or handles them by
/// direct field access, never through this trait. They live on the
/// [`TransportState`] supertrait (record + loop) and on [`TransportSettings`]
/// (preroll), read only where a genuinely live transport is required.
pub trait Timeline: Send + Sync {
    /// The playhead, as a [`Beat`](crate::Beat) position. May be
    /// negative during a count-in.
    fn beat(&self) -> crate::Beat;
    /// The tempo in force, in [`Bpm`](crate::Bpm).
    fn tempo(&self) -> crate::Bpm;
    /// Whether time is advancing. An offline render is always rolling.
    fn is_rolling(&self) -> bool;
}

/// A clock an offline render drives, one block at a time.
///
/// [`Timeline`] is read-only — `beat`/`tempo`/`is_rolling` are what a *node*
/// consults. Something has to move that position forward, and in a live session
/// it is the audio callback. Offline there is no callback, so the renderer does
/// it: after each block it reports how many frames it produced, and the clock
/// advances by exactly that much.
///
/// A renderer driving a `Net` needs nothing else: its clock nodes are inside
/// the net, so it only ever advances this. A renderer driving the native graph
/// (`tutti_graph::Executor`) must also *hand* each block a transport, since
/// the graph's clock is the executor's `Env`, not a node — that is
/// [`graph_block`](Self::graph_block) — and *seat* this clock on each
/// 64-frame chunk the graph's `Legacy` units run, since they poll it as a
/// `Timeline` per call and the graph block is longer — that is
/// [`seat`](Self::seat). [`render_graph`](Self::render_graph) is the one
/// order they are called in. It is still not a supertrait of
/// `Timeline`: a clock that does not move (`FrozenClock`) has a transport to
/// report — stopped, at beat zero — without being a timeline anything reads.
///
/// **Advance AFTER processing, never before.** `TransportClock` (the in-net
/// clock feeding beat-driven nodes) is emit-then-advance: sample 0 of a block
/// carries the block's start beat, and only then does the beat increment. A
/// clock advanced ahead of the net sits one `beats_per_sample` off the net's own
/// clock for the entire render — a desync that reads as "the samplers are
/// slightly late" and nothing else.
pub trait RenderClock: Send + Sync {
    /// Advance the playhead by `frames` — a **frame** count, not a sample
    /// count, so it is independent of the render's channel width.
    ///
    /// Call after the block has been processed, never before.
    fn advance(&self, frames: tutti_types::Samples);

    /// The block about to be rendered as the native graph takes it: the
    /// transport at the block's first frame, and the changes inside it. Read
    /// **before** the block is processed, as [`render_graph`](Self::render_graph)
    /// does.
    ///
    /// Required, not defaulted. The obvious default — stopped, at beat zero —
    /// is right for a clock that does not move and wrong for every clock that
    /// does: a moving clock that fell back on it would advance its clip readers
    /// while the graph's `EnvClock` and every `Env` reader held beat zero, a
    /// desync that renders without an error. A clock that moves must say where
    /// it is.
    fn graph_block(&self) -> (tutti_graph::Transport, tutti_graph::TransportChanges);

    /// Stand `at` frames into the block `origin` describes (what
    /// [`graph_block`](Self::graph_block) reported for it): where
    /// [`advance`](Self::advance) from there, called per `LEGACY_CHUNK`
    /// (64) frames and then for the rest, would leave this clock — the
    /// steps [`render_graph`](Self::render_graph) and a `Net` render both
    /// advance it in, so a seat is the position the clock will really pass
    /// through, to the bit. A view of the block in progress, not a move: `at`
    /// 0 puts the clock back on the block's first frame.
    ///
    /// [`render_graph`](Self::render_graph) calls it before each of a
    /// `Legacy` unit's chunks (a sampler voice polls this clock as a
    /// [`Timeline`] on every call, doc 013 §6), then with 0 before it
    /// advances the clock over the block. It answers from `origin` every
    /// time, never from the last seat: each `Legacy` node walks the block
    /// from frame 0 again.
    ///
    /// Required, for [`graph_block`](Self::graph_block)'s reason: a clock
    /// that moves and did not seat would hand every chunk of a block the
    /// block's first beat, and a voice would replay its first 64 frames
    /// through the block. A clock that does not move (`FrozenClock`) seats
    /// nothing.
    fn seat(&self, origin: &tutti_graph::Transport, at: tutti_types::Samples);

    /// Render one block of `frames` through `exec` under this clock, then
    /// advance the clock by it: [`graph_block`](Self::graph_block), then
    /// `Executor::process_with_clock` [seating](Self::seat) the clock on
    /// each `Legacy` chunk's first frame, then the clock back on the block's
    /// first frame and [`advance`](Self::advance)d by `frames`, 64 at a
    /// time as the `Net` path advances it — the one order that keeps every
    /// reader of this clock on the frame the graph renders (emit-then-advance,
    /// as above).
    ///
    /// # Panics
    ///
    /// As `Executor::process_with_changes`: if `frames` is zero or past the
    /// executor's prepared maximum block.
    fn render_graph(
        &self,
        exec: &mut tutti_graph::Executor,
        frames: usize,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        let (transport, changes) = self.graph_block();
        let seats = Seats {
            clock: self,
            origin: transport,
        };
        exec.process_with_clock(frames, &transport, &changes, &seats, inputs, outputs);
        // The last seat is wherever the last `Legacy` chunk began: back to
        // the block's start, then forward as a `Net` render moves the
        // clock, a chunk at a time, which is where the seats said it was.
        self.seat(&transport, tutti_types::Samples(0));
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(tutti_graph::LEGACY_CHUNK);
            self.advance(tutti_types::Samples(n));
            done += n;
        }
    }
}

/// A [`RenderClock`] as the executor's `LegacyClock` for one block: each seat
/// is [`RenderClock::seat`] from the block's origin.
struct Seats<'a, C: RenderClock + ?Sized> {
    clock: &'a C,
    origin: tutti_graph::Transport,
}

impl<C: RenderClock + ?Sized> tutti_graph::LegacyClock for Seats<'_, C> {
    fn seat(&self, at: tutti_graph::Offset) {
        self.clock
            .seat(&self.origin, tutti_types::Samples(at.index()));
    }
}

/// A clock that does not move.
///
/// For rendering a net with no time-dependent nodes — a synth patch, a test
/// tone, an impulse response. It exists so that "this graph has no transport"
/// is something a caller *states* rather than something they omit: a renderer
/// takes a clock, so forgetting one is a compile error instead of a silently
/// silent render.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrozenClock;

impl RenderClock for FrozenClock {
    fn advance(&self, _frames: tutti_types::Samples) {}

    /// Stopped at beat zero, every block: "this graph has no transport".
    fn graph_block(&self) -> (tutti_graph::Transport, tutti_graph::TransportChanges) {
        (
            tutti_graph::Transport::default(),
            tutti_graph::TransportChanges::NONE,
        )
    }

    /// Nothing moves, so there is nothing to seat.
    fn seat(&self, _origin: &tutti_graph::Transport, _at: tutti_types::Samples) {}
}

/// A live transport: a [`Timeline`] that also carries the record/loop state a
/// plugin host asks for.
///
/// This is the "transport state" bundle plugins request by name — VST3's opt-in
/// flag is literally `kNeedTransportState`, VST2 prefixes the flags
/// `TRANSPORT_RECORDING`/`TRANSPORT_CYCLE_ACTIVE`, CLAP groups them under
/// `clap_transport_flags`. Splitting it off keeps [`Timeline`] minimal so an
/// [`OfflineTimeline`] stays a first-class `Timeline`: an offline render has no
/// record state and folds looping into its own `advance()`, so it implements
/// `Timeline` only and keeps `loop_range` as an inherent method.
///
/// The live [`Transport`] implements this; a consumer that needs record or loop
/// state (the plugin `TransportSource`, `ParamAutomationSource`) depends on
/// `dyn TransportState`, never the concrete backend.
pub trait TransportState: Timeline {
    /// Whether the transport is armed and recording. Always `false` offline.
    fn is_recording(&self) -> bool;
    /// The active loop region, or `None` when not looping. Always a valid,
    /// non-empty region — see [`LoopRange`].
    fn loop_range(&self) -> Option<LoopRange>;
    /// Samples elapsed since the stream started — free-running, never reset by
    /// a loop, seek, or stop.
    ///
    /// Belongs here rather than on [`Timeline`] for the same reason as the two
    /// above: it is a live-session fact.
    ///
    /// Required, not defaulted. A default of `0` would be defensible only for an
    /// offline render — but an offline render implements [`Timeline`] alone and
    /// never reaches this trait, so the default would exist for a case that
    /// cannot occur while silently letting a real implementor report "no
    /// continuous clock" forever. A host with no sample counter should return `0`
    /// deliberately, which is what the plugin ABIs read as exactly that.
    fn steady_time(&self) -> i64;
}
