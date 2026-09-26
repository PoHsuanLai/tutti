//! The per-buffer graph render, called from the audio callback.
//!
//! [`Engine`] ticks the native graph's
//! [`Executor`](tutti_graph::Executor) and the transport, and renders one
//! output buffer per block (doc 013; fundsp's `Net` rendered here too until
//! Phase 3 PR 15). It folds the graph's outputs to the device width, keeps
//! the declick, and applies timestamped transport commands on their frame.
//!
//! # Timestamped transport commands
//!
//! [`MotionFsm::schedule`] queues a play, stop, seek, tempo or loop change at
//! an [`At`]. Each block, the engine walks the commands due in it in time
//! order and **cuts the block's transport** at each one's frame: the pieces
//! before and after run under different transports. The executor never
//! splits a block (doc 013 §6): the engine advances its own clock piece by
//! piece, records each cut as a
//! [`TransportChange`](tutti_graph::TransportChange) in the block's `Env`,
//! and renders the whole block once. A node reads the transport at a frame
//! with `Env::transport_at`, and the graph's own `At::Beat` commands resolve
//! against the piece that reaches their beat.
//!
//! **Chunk-major while the plan holds a `Legacy` unit**
//! (`Plan::has_legacy`, doc 013's `Legacy` compatibility mode). A `Legacy`
//! unit reads no `Env`: one that follows the transport (a sampler voice, a
//! MIDI clip source) polls the live `Transport` on every 64-frame call. So
//! the engine then renders graph blocks of at most [`LEGACY_CHUNK`] frames,
//! across every node, each with its own walk, and publishes the playhead
//! after each: through a chunk it reads the chunk's first frame, and it only
//! moves forward. A graph with no `Legacy` unit renders whole blocks.
//!
//! A block holds at most
//! [`MAX_TRANSPORT_CHANGES`](tutti_graph::MAX_TRANSPORT_CHANGES) cuts. A command due past
//! that lands at the start of the next block and is counted late, like any
//! command already past due ([`MotionFsm::late_commands`]). (`At::NextBlock`
//! commands, late ones and crossed beats all land at the block's first frame
//! and need no cut, so they are never deferred.) Commands due on one frame
//! apply in send order.
//!
//! **Untimed state is read once per block.** The tempo, loop and play state
//! are read at the start of the walk; a store from the control thread during
//! the block lands at the next one, and only an applied command changes them
//! at a cut.
//!
//! **Beats** are resolved with the graph's rule (`tutti_graph::Env::due`):
//! the first frame at or after the beat that playback reaches, the piece's
//! own transport deciding. A beat continuous playback already crossed is late;
//! one a seek or loop jumped over waits (`tutti_graph::Playhead`).
//!
//! **The declick is audio only, and its fade-out ends on the command's
//! frame.** A declick stop or seek moves the transport on its command's
//! frame, as an immediate one does. The fade is the engine's, as a gain on
//! the output, continuous at every frame:
//!
//! - **A timed command** (`At::Frame`, `At::Beat`) is seen ahead: the walk
//!   looks one fade (480 frames) past each block for the next declicked
//!   command playback reaches, and the gain falls linearly on the *old*
//!   position's audio to reach zero exactly on its frame. Seen with less
//!   than a fade to go (sent late, or reached sooner after a tempo change),
//!   it falls over whatever frames are left: steeper, still continuous.
//! - **On the frame** the transport jumps or stops, sample-accurately, and
//!   the gain rises from zero over one fade: the new position's audio after
//!   a seek, and after a stop whatever still sounds while stopped (live
//!   input, a reverb tail; transport-gated sources are already silent), so
//!   the output never comes back with a step.
//! - **With no lead time** (`At::NextBlock`, an untimed `try_send`, a late
//!   command) the jump is at the block's first frame and nothing could fade
//!   the audio before it: the gain is zero on that frame (the old audio's
//!   abrupt end, the one step, accepted) and the new audio fades in.
//!
//! A command the motion machine refuses after its fade-out still leaves the
//! gain at zero on its frame, and it fades back in from there. The motion
//! machine's own fade ramp is not used; the engine settles it the moment the
//! command lands.
//!
//! # Limits
//!
//! [`Engine::new`] bounds the graph's editor ([`Editor::set_limits`]) to
//! what its fold scratch holds: at most [`MAX_ROOT_CHANNELS`] global
//! outputs, and a `MaxBlock` no larger than its block capacity (the larger
//! of the prepared maximum and [`DEFAULT_BLOCK_CAPACITY`], or the
//! capacity given to [`Engine::with_capacity`]). A commit or a re-prepare
//! past them is refused on the control thread
//! (`CommitError::TooManyOutputs`, `CommitError::BlockTooLong`), so the
//! callback never meets a graph it cannot render.

use tutti_graph::{
    CommitError, Due, Editor, Env, Executor, Limits, Offset, Playhead, TransportChanges,
    LEGACY_CHUNK,
};

use crate::transport::fsm::DEFAULT_DECLICK_FRAMES;
use crate::transport::Control;
use crate::transport::{
    FadeOut, MotionEvent, MotionFsm, MotionState, TransportClock, TransportCommand,
};
use crate::transport::{Schedule, Scheduled, SCHEDULE_CAPACITY};
use crate::{AudioThreadCell, AudioUnit, InterleavedMut, SampleRate, Samples};
use tutti_types::{At, Frame};

/// The widest graph root an engine renders — the most global outputs it
/// folds to the device. The executor must be handed a buffer for **every**
/// global output, and the engine's fold scratch holds this many: mono
/// through 7.1. So an engine refuses more: [`Engine::new`] bounds the editor
/// to it, and a commit with more global outputs is an error on the control
/// thread. (A device wider than this is fine: `fold_frame` writes every
/// device channel, and the ones past what the root folds to are silent.)
pub const MAX_ROOT_CHANNELS: usize = 8;

/// The block capacity an engine sizes its fold scratch for unless given
/// another ([`Engine::with_capacity`]): tutti-cpal's largest callback
/// (`MAX_FRAMES`). The engine bounds its editor's re-prepares to it.
pub const DEFAULT_BLOCK_CAPACITY: Samples = Samples(8192);

/// Why [`Engine::new`] refused a graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEngineError {
    /// The executor is not the editor's: they were not built together.
    NotAPair,
    /// What the editor already sent is past what the engine can run: more
    /// than [`MAX_ROOT_CHANNELS`] global outputs, or a `MaxBlock` above the
    /// block capacity.
    Limits(CommitError),
    /// Another clock already writes `transport`'s playhead: an engine built
    /// over it (or over a clone of it) is still alive. Two engines on one
    /// transport would both consume every seek and both write the playhead
    /// ([`Transport::clock_links`](crate::Transport::clock_links)).
    PlayheadClaimed(crate::transport::PlayheadClaimed),
}

impl core::fmt::Display for GraphEngineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAPair => f.write_str("the executor is not the editor's"),
            Self::Limits(e) => write!(f, "the graph is past the engine's limits: {e}"),
            Self::PlayheadClaimed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GraphEngineError {}

/// The transport as a native graph block sees it.
type GraphTransport = tutti_graph::Transport;

/// The audio engine: ticks the native graph + transport and renders one
/// output buffer per block from the audio callback.
///
/// # What it owns, and what it deliberately does not
///
/// `Engine` is the *audio-thread half* of the runtime and holds only what a
/// render needs: the transport's [`MotionFsm`], the graph's [`Executor`]
/// with the clock that feeds its `Env`, and the declick gain it puts on the
/// output. It owns no graph topology, no parameter storage and no device
/// configuration — the control thread keeps the graph's editing half (the
/// `tutti_graph::Editor`) and hands changes over by committing, so nothing
/// here allocates, locks, or edits a graph.
///
/// That split is the reason this is a distinct type rather than a method on the
/// transport or the graph. Both of those are edited from the control thread;
/// this is touched only from the callback. Fusing it into either would put a
/// control-thread API and an RT-only API on one object, where the compiler can
/// no longer say which methods are safe to call from where — and the failure is
/// silent, because a lock or an allocation on the audio thread produces a
/// dropout rather than an error.
///
/// [`process`](Self::process) is the per-block entry point the callback calls:
/// it drains pending motion, applies timestamped transport commands on their
/// frames (see the module docs), renders, then applies any declick fade.
/// [`process_segment`](Self::process_segment) is public separately because it
/// is the pure render — useful to drive directly in a test or an offline pass,
/// where transport motion and declicking are not in play.
pub struct Engine {
    motion: MotionFsm,
    graph: AudioThreadCell<GraphRender>,
    /// Cached from the transport so the fade path avoids a double deref.
    fader: AudioThreadCell<Fader>,
    /// The block capacity (the editor's `MaxBlock` limit).
    capacity: Samples,
}

/// The native graph's executor, the clock that feeds its `Env`, and the
/// planar scratch its outputs land in before the fold.
struct GraphRender {
    exec: Executor,
    /// The engine's playhead. The graph reads the transport from `Env`, so
    /// the engine drives the clock itself ([`TransportClock::begin`] /
    /// [`TransportClock::advance`]); an `EnvClock` in the graph continues it
    /// with the same arithmetic.
    clock: TransportClock,
    /// `MAX_ROOT_CHANNELS` planar channels of `stride` frames each.
    scratch: Vec<f32>,
    stride: usize,
    playhead: Playhead,
}

impl Engine {
    /// Build an engine that renders a native graph: `executor`, the audio
    /// half of `editor`'s pair, whose `Prepare` comes from the device
    /// configuration (its rate, and the largest block the device hands
    /// over). The block capacity is the larger of that maximum and
    /// [`DEFAULT_BLOCK_CAPACITY`]; see
    /// [`with_capacity`](Self::with_capacity).
    pub fn new(
        transport: &crate::Transport,
        editor: &mut Editor,
        executor: Executor,
    ) -> Result<Self, GraphEngineError> {
        Self::with_capacity(transport, editor, executor, DEFAULT_BLOCK_CAPACITY)
    }

    /// As [`new`](Self::new), with the fold scratch sized for blocks of up
    /// to `capacity` frames (or the prepared maximum, if larger).
    ///
    /// The engine renders whole device blocks through the executor — no
    /// 64-frame chunking unless a `Legacy` unit is present (module docs); a
    /// device block longer than the prepared maximum is rendered as
    /// consecutive graph blocks of at most that — and builds each block's
    /// `Env` from `transport`: the frame is the executor's clock, which
    /// tracks device time, and the transport snapshot comes from a
    /// [`TransportClock`] the engine drives over `transport`'s
    /// [`clock_links`](crate::Transport::clock_links), so it publishes the
    /// playhead, steady time and tempo in force.
    ///
    /// **Bounds the editor** ([`Editor::set_limits`]) to at most
    /// [`MAX_ROOT_CHANNELS`] global outputs and a `MaxBlock` of at most the
    /// capacity, so every later commit or re-prepare past them is refused
    /// there, with a `CommitError`. Refused here, building nothing, when
    /// `executor` is not `editor`'s, or when what the editor already sent is
    /// past them. The editor is borrowed mutably for the check, so no commit
    /// can slip in beside it.
    ///
    /// **One engine per transport.** The engine's clock is `transport`'s one
    /// playhead writer ([`Transport::clock_links`](crate::Transport::clock_links)):
    /// while this engine is alive, another built over the same transport, or
    /// a clone of it, is refused with [`GraphEngineError::PlayheadClaimed`] —
    /// two would both consume every seek and both write the playhead. The
    /// claim is given back when this engine is dropped.
    ///
    /// Nor can the graph hold a second writer: a `TransportClock` can only
    /// write a playhead through links from that one call. A node that
    /// takes the beat as a signal (`ClickNode`, a beat-driven LFO or
    /// automation lane) is fed by an [`EnvClock`](crate::EnvClock) instead,
    /// which emits the same samples from the block's `Env` and shares
    /// nothing.
    ///
    /// Control thread. Allocates the fold scratch
    /// (`MAX_ROOT_CHANNELS × capacity` samples).
    pub fn with_capacity(
        transport: &crate::Transport,
        editor: &mut Editor,
        executor: Executor,
        capacity: Samples,
    ) -> Result<Self, GraphEngineError> {
        if !editor.is_paired_with(&executor) {
            return Err(GraphEngineError::NotAPair);
        }
        // Before the limits, which change the editor: a refused engine
        // leaves it as it was.
        let links = transport
            .clock_links()
            .map_err(GraphEngineError::PlayheadClaimed)?;
        // Mid re-prepare, the editor's `Prepare` is the one the executor will
        // adopt, and may be the larger.
        let stride = capacity
            .get()
            .max(executor.prepare().max_block().get())
            .max(editor.prepare().max_block().get());
        editor
            .set_limits(Limits {
                max_global_outputs: MAX_ROOT_CHANNELS,
                max_block: stride,
            })
            .map_err(GraphEngineError::Limits)?;
        let rate = executor.prepare().sample_rate();
        let motion = transport.motion.clone();
        Ok(Self {
            motion,
            graph: AudioThreadCell::new(GraphRender {
                exec: executor,
                clock: TransportClock::new(links, rate),
                scratch: vec![0.0; MAX_ROOT_CHANNELS * stride],
                stride,
                playhead: Playhead::new(),
            }),
            fader: AudioThreadCell::new(Fader::new()),
            capacity: Samples(stride),
        })
    }

    /// The block capacity: the largest `MaxBlock` the editor may re-prepare
    /// to.
    pub fn block_capacity(&self) -> Samples {
        self.capacity
    }

    /// Render the whole of `output` — an interleaved device buffer that carries
    /// its own width — with no transport motion and no declick: the pure
    /// render.
    ///
    /// One executor block per device block (up to its prepared maximum, or
    /// [`LEGACY_CHUNK`] while a `Legacy` unit is present), under the
    /// transport as it stands. The graph root has no inputs. The root is
    /// rendered at its **own** output width (at most [`MAX_ROOT_CHANNELS`],
    /// which the editor's limits enforce) into scratch, then each frame is
    /// folded to the *output's* width — the device / target width — via the
    /// ITU/Dolby matrices ([`tutti_types::fold_frame`]): a surround root
    /// plays folded to a stereo device, or straight through to a
    /// matching-width surround device; a mono root duplicates into every
    /// target channel of a wider output.
    ///
    /// # Two widths are live here; only one is the buffer's
    ///
    /// The output width (`out_ch`) comes from `output`'s own layout; the
    /// root's from the plan's global outputs. They are different numbers
    /// from different sources, and confusing them writes past the end of one
    /// buffer or reads garbage from the other. The output width arrives
    /// welded to the buffer it strides, which is what makes a third
    /// confusion — a width disagreeing with its slice — unrepresentable
    /// rather than merely unlikely.
    #[inline]
    pub fn process_segment(&self, output: &mut InterleavedMut<'_>) {
        let out_ch = output.stride();
        let frames = output.len();
        let output = output.samples_mut();
        let g = &mut *self.graph.borrow_mut();
        let mut done = 0;
        while done < frames {
            let (bound, rate) = g.settle(self.motion.timed());
            let len = (frames - done).min(bound);
            let control = Control::read(self.motion.settings());
            let t = g.clock.begin(&control, true);
            // Kept current, so a later `process` tells a late beat from one
            // jumped over across this block too.
            g.playhead.observe(&Env {
                frame: g.exec.frame(),
                sample_rate: rate,
                block_len: Samples(len),
                transport: t,
                changes: TransportChanges::NONE,
            });
            g.clock.advance(len, &t);
            g.render(output, out_ch, done, len, &t, &TransportChanges::NONE);
            done += len;
        }
    }

    /// Render one block into `output`, an interleaved device buffer.
    ///
    /// How many frames is `output`'s own business — it is `output.len()`,
    /// which cannot disagree with the slice the way a separate `frames`
    /// argument could. The graph root is folded to `output`'s width (the device
    /// / target width) via the ITU/Dolby matrices — see
    /// [`process_segment`](Self::process_segment). Untimed motion
    /// ([`MotionFsm::try_send`]) applies at the block's first frame; scheduled
    /// commands ([`MotionFsm::schedule`]) on their own frames (module docs).
    /// Called once per block from the audio callback. RT-safe: no
    /// allocation, no locks, no I/O.
    #[inline]
    pub fn process(&self, output: &mut InterleavedMut<'_>) {
        self.motion.drain();
        let out_ch = output.stride();
        let frames = output.len();
        let output = output.samples_mut();
        let g = &mut *self.graph.borrow_mut();
        let mut done = 0;
        while done < frames {
            let (bound, rate) = g.settle(self.motion.timed());
            let len = (frames - done).min(bound);
            let frame0 = g.exec.frame();
            let mut pieces = GraphPieces {
                clock: &mut g.clock,
                settings: self.motion.settings(),
                control: Control::read(self.motion.settings()),
                changes: TransportChanges::NONE,
            };
            let aim = self.fader.borrow().aim;
            let walk = self.walk(frame0, len, rate, aim, &mut g.playhead, &mut pieces);
            let changes = pieces.changes;
            g.render(output, out_ch, done, len, &walk.start, &changes);
            self.fader
                .borrow_mut()
                .apply(&walk.gain, frame0, output, out_ch, done, len);
            done += len;
        }
    }

    /// Walk one block's scheduled transport commands in time order, running
    /// each piece between them through `pieces` and planning the declick
    /// gain over the block. See the module docs for the rules.
    ///
    /// `aim` is the fader's aim going in: an aim is pushed only where it
    /// changes, so a block with no command in sight plans nothing and the
    /// fader skips it.
    fn walk(
        &self,
        frame0: Frame,
        frames: usize,
        rate: SampleRate,
        mut aim: Option<Frame>,
        playhead: &mut Playhead,
        pieces: &mut impl Pieces,
    ) -> Walk {
        let schedule = self.motion.timed();
        let mut walk = Walk {
            start: pieces.begin(None),
            gain: GainPlan::new(),
        };
        // An untimed declicked command drained at the block's start has no
        // lead time: it lands at the first frame.
        if self.settle_declick(&mut walk.gain, 0) {
            aim = None;
        }
        let mut t = walk.start;
        let mut cursor = 0;
        // The playhead as of the start of the current piece. Each piece is
        // observed with its real length once it is cut; before that, a copy
        // observes it with the rest of the block, to answer `crossed`.
        let mut base = *playhead;
        schedule.with_pending(|pending| loop {
            let env = Env {
                frame: frame0 + Samples(cursor),
                sample_rate: rate,
                block_len: Samples(frames - cursor),
                transport: t,
                changes: TransportChanges::NONE,
            };
            // The next declicked command ahead, within one fade of the end of
            // this block: the fade-out that ends on its frame may start here.
            let next = lead_target(pending, &env);
            if next != aim {
                walk.gain.push(cursor, Gain::Aim(next));
                aim = next;
            }
            let mut ph = base;
            ph.observe(&env);
            // The earliest command due from the cursor on; send order breaks
            // ties.
            let mut best: Option<(usize, usize, bool)> = None;
            for (i, cmd) in pending.iter().enumerate() {
                let (k, late) = match (cmd.at, env.due(cmd.at)) {
                    (_, Due::In(k)) => (k.index(), false),
                    (At::Frame(_), Due::Late) => (0, true),
                    (At::Beat(b), _) if ph.crossed(b.get()) => (0, true),
                    _ => continue,
                };
                let earlier = best
                    .is_none_or(|(bk, bi, _)| k < bk || (k == bk && cmd.seq() < pending[bi].seq()));
                if earlier {
                    best = Some((k, i, late));
                }
            }
            let Some((k, i, late)) = best else { break };
            let at = cursor + k;
            if at > cursor {
                if !pieces.room() {
                    // No more cuts fit this block: the rest wait for the
                    // next one, where they land late.
                    break;
                }
                base.observe(&Env {
                    block_len: Samples(at - cursor),
                    ..env
                });
                pieces.run(cursor, at, &t);
                cursor = at;
            }
            let cmd = pending.swap_remove(i);
            schedule.release(1);
            if late {
                schedule.count_late();
            }
            self.motion.apply(cmd.command);
            if self.settle_declick(&mut walk.gain, cursor) {
                // A jump clears the fader's aim.
                aim = None;
            }
            t = pieces.begin(Some(&cmd.command));
            match Offset::new(cursor, Samples(frames)) {
                Some(o) if cursor > 0 => pieces.change(o, t),
                _ => walk.start = t,
            }
        });
        base.observe(&Env {
            frame: frame0 + Samples(cursor),
            sample_rate: rate,
            block_len: Samples(frames - cursor),
            transport: t,
            changes: TransportChanges::NONE,
        });
        *playhead = base;
        pieces.run(cursor, frames, &t);
        walk
    }

    /// The motion machine chose a declick for the command just applied at
    /// `at`: the transport has already moved (`MotionFsm` applies a fade's
    /// outcome at once), so the fade is the engine's, as gain. Mark the jump
    /// (the fade-out, if it had lead time, ends here; the fade-in starts
    /// here) and settle the machine, whose own ramp the engine does not use.
    /// Returns whether it marked one.
    fn settle_declick(&self, gain: &mut GainPlan, at: usize) -> bool {
        if is_declicking(self.motion.motion()) {
            gain.push(at, Gain::Jump);
            self.motion.complete_declick();
            return true;
        }
        false
    }

    /// Install every commit the engine's editor has sent, and follow a
    /// re-prepare's rate change with the engine's clock and the transport's
    /// frame-timed commands — what the first block after it would do, done
    /// now. Returns whether the graph is running a plan with no re-prepare
    /// between its halves.
    ///
    /// A device restart calls it between the two halves of
    /// `Editor::reprepare` so the re-prepare finishes before the first block
    /// at the new rate, which then renders the re-prepared graph rather than
    /// the executor's silent checked-out block (doc 013, Phase 3 PR 13).
    /// Hosts reach it through `tutti_cpal::Stopped::settle_graph`, which only
    /// a restart hook is handed.
    ///
    /// Allocates nothing itself; a commit it installs was built on the
    /// control side.
    ///
    /// # Safety
    ///
    /// The caller guarantees that **no audio callback runs [`process`] or
    /// [`process_segment`] on this engine for the duration of the call** —
    /// the stream is stopped (dropped, or not yet started). This borrows the
    /// executor and the transport schedule the audio thread otherwise owns;
    /// their [`AudioThreadCell`](tutti_types::AudioThreadCell)s catch a
    /// concurrent borrow only in debug builds, so in release a call racing
    /// a callback is an unchecked data race.
    ///
    /// [`process`]: Self::process
    /// [`process_segment`]: Self::process_segment
    pub unsafe fn settle_graph(&self) -> bool {
        let graph = &mut *self.graph.borrow_mut();
        graph.settle(self.motion.timed());
        graph.exec.pending_prepare().is_none() && graph.exec.plan().is_some()
    }

    /// Reset the audio-thread ownership assertions on both cells.
    ///
    /// Call when the device switches and a different thread takes over the
    /// callback: `AudioThreadCell` pins the first thread that borrows it and
    /// panics in debug builds on any other, so a new callback thread must be
    /// announced rather than discovered.
    pub fn reset_owners(&self) {
        self.graph.reset_owner();
        self.motion.reset_owner();
    }
}

/// Whether `motion` is a fade in progress.
#[inline]
fn is_declicking(motion: MotionState) -> bool {
    matches!(
        motion,
        MotionState::DeclickToStop | MotionState::DeclickToLocate
    )
}

/// What a block's walk leaves for after the render.
struct Walk {
    /// The transport at the block's first frame, after the commands that
    /// landed there.
    start: GraphTransport,
    /// The declick gain's events in this block.
    gain: GainPlan,
}

/// Frames a declick fades over, out and in: the motion machine's fade
/// length (10 ms at 48 kHz).
const FADE: usize = DEFAULT_DECLICK_FRAMES.get();

/// Whether `command` asks for a declick (the motion machine grants it only
/// while the transport is audible, which the lead check mirrors).
fn declicks(command: &TransportCommand) -> bool {
    matches!(
        command,
        TransportCommand::Motion(
            MotionEvent::Stop {
                fade: FadeOut::Declick
            } | MotionEvent::Locate {
                fade: FadeOut::Declick,
                ..
            }
        )
    )
}

/// The frame of the next declicked command ahead of `env`'s first frame,
/// if playback reaches it within `env`'s block plus one fade: where a
/// fade-out has to end. Timed commands only (`At::Frame`, `At::Beat`); one
/// due on the first frame itself has no lead left.
fn lead_target(pending: &[Scheduled], env: &Env) -> Option<Frame> {
    if !env.transport.playing {
        return None;
    }
    let look = Env {
        block_len: env.block_len + Samples(FADE),
        ..*env
    };
    pending
        .iter()
        .filter(|c| declicks(&c.command) && !matches!(c.at, At::NextBlock))
        .filter_map(|c| match look.due(c.at) {
            Due::In(k) if k.index() > 0 => Some(look.frame_at(k)),
            _ => None,
        })
        .min()
}

/// One change to the declick gain, at an offset of the block.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gain {
    /// From here, the next declicked command lands on this frame (or none
    /// is in sight): the gain falls linearly to reach zero exactly there,
    /// starting at most one fade ahead of it.
    Aim(Option<Frame>),
    /// A declicked command landed here: the gain is zero on this frame, and
    /// fades in from it.
    Jump,
}

/// Capacity of a block's gain plan: an aim per walk step and a jump per
/// command, bounded by the commands a block can apply.
const GAIN_EVENTS: usize = 2 * (SCHEDULE_CAPACITY + 2);

/// A block's declick gain events, in offset order, on the stack.
struct GainPlan {
    len: usize,
    items: [(usize, Gain); GAIN_EVENTS],
}

impl GainPlan {
    fn new() -> Self {
        Self {
            len: 0,
            items: [(0, Gain::Aim(None)); GAIN_EVENTS],
        }
    }

    fn push(&mut self, at: usize, gain: Gain) {
        // A later aim at the same offset replaces an earlier one.
        if let Some(last) = self.items[..self.len].last_mut() {
            if last.0 == at && matches!(last.1, Gain::Aim(_)) && matches!(gain, Gain::Aim(_)) {
                last.1 = gain;
                return;
            }
        }
        debug_assert!(self.len < GAIN_EVENTS, "bounded by the commands applied");
        if self.len < GAIN_EVENTS {
            self.items[self.len] = (at, gain);
            self.len += 1;
        }
    }
}

/// The declick gain, carried across blocks: a fade-out that ends exactly on
/// a declicked command's frame, and a fade-in that starts there.
///
/// Continuous by construction: a fade-out falls linearly from wherever the
/// gain is to zero at its target, so with a full fade of lead each frame
/// moves it by at most `1/FADE`; a fade-in rises by `1/FADE` a frame. The one
/// step is a declicked command with no lead time (`At::NextBlock`, a late
/// command, an untimed `try_send`): the old audio ends on the jump frame
/// with the gain at zero there, and the new audio fades in.
#[derive(Clone, Copy, Debug)]
struct Fader {
    gain: f32,
    aim: Option<Frame>,
}

impl Fader {
    fn new() -> Self {
        Self {
            gain: 1.0,
            aim: None,
        }
    }

    /// Shape frames `base..base + frames` of the interleaved `output`
    /// (`channels` wide), the block that starts at `frame0`, by `plan`. The
    /// gain is per frame, the same across every channel.
    fn apply(
        &mut self,
        plan: &GainPlan,
        frame0: Frame,
        output: &mut [f32],
        channels: usize,
        base: usize,
        frames: usize,
    ) {
        let end = frame0.get() + frames as u64;
        // Nothing to do on a block with no event, full gain, and no fade-out
        // starting inside it: the common case.
        let quiet = self.aim.is_none_or(|to| to.get() > end + FADE as u64);
        if plan.len == 0 && self.gain == 1.0 && quiet {
            return;
        }
        let mut next = 0;
        for i in 0..frames {
            let mut jump = false;
            while next < plan.len && plan.items[next].0 == i {
                match plan.items[next].1 {
                    Gain::Aim(to) => self.aim = to,
                    // Clears the aim it lands on when read, so an aim at the
                    // next target pushed after it at this offset survives.
                    Gain::Jump => {
                        jump = true;
                        self.aim = None;
                    }
                }
                next += 1;
            }
            let x = frame0.get() + i as u64;
            if jump {
                self.gain = 0.0;
            } else {
                match self.aim {
                    Some(to) if x >= to.get() => {
                        // On the target: zero, and the fade-in starts from
                        // here (whether or not the command was applied).
                        self.gain = 0.0;
                        self.aim = None;
                    }
                    Some(to) if to.get() - x <= FADE as u64 => {
                        let left = (to.get() - x) as f32;
                        self.gain *= left / (left + 1.0);
                    }
                    _ => self.gain = (self.gain + 1.0 / FADE as f32).min(1.0),
                }
            }
            if self.gain != 1.0 {
                let at = (base + i) * channels;
                for s in &mut output[at..at + channels] {
                    *s *= self.gain;
                }
            }
        }
    }
}

/// What a block walk does with each piece between its cuts. A trait, with
/// one implementation in the engine ([`GraphPieces`]), so a test can wrap it
/// and stage a control-thread store between pieces deterministically.
trait Pieces {
    /// The transport from the current frame on: at the block's start
    /// (`after` is `None`), or after applying `after` there.
    fn begin(&mut self, after: Option<&TransportCommand>) -> GraphTransport;
    /// Run frames `start..end` of the block under `t`.
    fn run(&mut self, start: usize, end: usize, t: &GraphTransport);
    /// Whether another cut fits in this block.
    fn room(&self) -> bool;
    /// Record the transport changing to `t` at `at`.
    fn change(&mut self, at: Offset, t: GraphTransport);
}

/// Advance the engine's clock over each piece, and collect the cuts for the
/// block's `Env`; the render happens once, after.
struct GraphPieces<'a> {
    clock: &'a mut TransportClock,
    settings: &'a crate::TransportSettings,
    /// The untimed inputs, read once at the walk's start; only an applied
    /// command changes them.
    control: Control,
    changes: TransportChanges,
}

impl Pieces for GraphPieces<'_> {
    fn begin(&mut self, after: Option<&TransportCommand>) -> GraphTransport {
        match after {
            None => self.clock.begin(&self.control, true),
            Some(command) => {
                self.control.apply(command, self.settings);
                // Only a motion command can have requested a seek.
                let seeks = matches!(command, TransportCommand::Motion(_));
                self.clock.begin(&self.control, seeks)
            }
        }
    }

    fn run(&mut self, start: usize, end: usize, t: &GraphTransport) {
        self.clock.advance(end - start, t);
    }

    fn room(&self) -> bool {
        !self.changes.is_full()
    }

    fn change(&mut self, at: Offset, t: GraphTransport) {
        // Cannot fail: `room` was checked before the cut, cuts come in time
        // order, and one at the same frame replaces.
        let _ = self.changes.push(at, t);
    }
}

impl GraphRender {
    /// Bring the executor up to date before a graph block: install queued
    /// commits (a re-prepare's resume adopts its `Prepare` here, so the block
    /// length below is the new maximum and not a stale one), and follow a
    /// rate change with the clock. Returns the longest block to hand the
    /// executor, and its rate.
    ///
    /// The rate is followed on the block the re-prepare's **first** commit
    /// lands, not its resume: from that block the executor counts frames at
    /// the new rate (it has rescaled its clock and its own `At::Frame`
    /// commands, `Executor::pending_prepare`), and it renders silence while
    /// the transport rolls on. So the engine's clock steps the beat at the
    /// new rate from there too — a clock left at the old one would move the
    /// playhead by `old / new` of a frame per frame until the resume, a
    /// jump of the suspended stretch's worth — and the transport's
    /// frame-timed commands (`schedule`) move with the executor's.
    fn settle(&mut self, schedule: &Schedule) -> (usize, SampleRate) {
        self.exec.apply_pending();
        let rate = self
            .exec
            .pending_prepare()
            .unwrap_or(self.exec.prepare())
            .sample_rate();
        let was = self.clock.sample_rate();
        if rate != was {
            // A re-prepare changed the rate: the executor has rescaled its
            // frame clock; the beat increment and the transport's own
            // frame-timed commands follow.
            AudioUnit::set_sample_rate(&mut self.clock, rate);
            schedule.rescale(rate.get() / was.get());
        }
        let bound = self.exec.prepare().max_block().get();
        // The editor's limits keep every `MaxBlock` within the scratch.
        debug_assert!(bound <= self.stride, "MaxBlock {bound} past the scratch");
        let bound = bound.min(self.stride);
        // Chunk-major while a `Legacy` unit may poll the transport (the
        // module docs): after `apply_pending`, so a commit that adds or
        // removes the last one switches at this block.
        if self.exec.plan().is_some_and(|p| p.has_legacy()) {
            return (bound.min(LEGACY_CHUNK), rate);
        }
        (bound, rate)
    }

    /// Render one graph block of `len` frames into frames
    /// `at..at + len` of `output`, folded from the graph's width to
    /// `out_ch`.
    fn render(
        &mut self,
        output: &mut [f32],
        out_ch: usize,
        at: usize,
        len: usize,
        transport: &GraphTransport,
        changes: &TransportChanges,
    ) {
        // `settle` installed every queued commit, and a commit applied
        // inside `process_with_changes` is one sent after it: the next block
        // sees it. The editor's limits refuse a graph wider than the scratch.
        let width = self.exec.plan().map_or(0, |p| p.global_outputs());
        debug_assert!(
            width <= MAX_ROOT_CHANNELS,
            "the editor's limits refuse this"
        );
        let width = width.min(MAX_ROOT_CHANNELS);
        let block = &mut output[at * out_ch..(at + len) * out_ch];
        let stride = self.stride;
        let mut chunks = self.scratch.chunks_mut(stride);
        let mut outs: [&mut [f32]; MAX_ROOT_CHANNELS] =
            std::array::from_fn(|_| chunks.next().expect("MAX_ROOT_CHANNELS chunks"));
        self.exec
            .process_with_changes(len, transport, changes, &[], &mut outs[..width]);
        // Published after the block, not by the walk before it: through the
        // block the live playhead still reads its first frame (the last
        // block's end), which is what a `Legacy` unit polling it takes for
        // its call's first frame. And it only moves forward.
        self.clock.publish_position();
        if width == 0 {
            block.fill(0.0);
            return;
        }
        for i in 0..len {
            let mut frame = [0.0f32; MAX_ROOT_CHANNELS];
            let src = &mut frame[..width];
            for (c, s) in src.iter_mut().enumerate() {
                *s = outs[c][i];
            }
            tutti_types::fold_frame(src, &mut block[i * out_ch..(i + 1) * out_ch]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bpm, MotionEvent, Transport};
    use tutti_graph::Prepare;

    /// A graph walk's `Pieces` that stores a new tempo from "the control
    /// thread" while each piece runs: the store races the walk, staged
    /// deterministically.
    struct Racing<'a> {
        inner: GraphPieces<'a>,
        settings: &'a crate::TransportSettings,
    }

    impl Pieces for Racing<'_> {
        fn begin(&mut self, after: Option<&TransportCommand>) -> GraphTransport {
            self.inner.begin(after)
        }
        fn run(&mut self, start: usize, end: usize, t: &GraphTransport) {
            self.inner.run(start, end, t);
            self.settings.set_tempo(Bpm(200.0));
        }
        fn room(&self) -> bool {
            self.inner.room()
        }
        fn change(&mut self, at: Offset, t: GraphTransport) {
            self.inner.change(at, t);
        }
    }

    /// An untimed store racing the walk does not land at a cut: the change a
    /// timed `Play` records mid-block carries the tempo read at the block's
    /// start, and only the command's own effect (rolling). The store lands
    /// at the next block.
    ///
    /// Mutation (run): re-read `Control::read` after a command in
    /// `GraphPieces::begin` (the reviewed behaviour) → the change carries
    /// 200 BPM → fails.
    #[test]
    fn an_untimed_store_during_the_walk_waits_for_the_next_block() {
        let transport = Transport::new(48_000.0);
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        transport
            .motion
            .schedule(At::Frame(Frame(100)), MotionEvent::Play)
            .expect("room");
        // The engine holds the transport's one playhead writer; the walk
        // under test needs a clock over the same inputs, not a second writer.
        let mut clock = TransportClock::new(
            crate::transport::ClockLinks::bare(
                std::sync::Arc::clone(&transport.settings.tempo),
                std::sync::Arc::clone(&transport.settings.paused),
            ),
            48_000.0,
        );
        let settings = engine.motion.settings();
        let mut pieces = Racing {
            inner: GraphPieces {
                clock: &mut clock,
                settings,
                control: Control::read(settings),
                changes: TransportChanges::NONE,
            },
            settings,
        };
        let mut playhead = Playhead::new();
        let walk = engine.walk(
            Frame::ZERO,
            512,
            SampleRate(48_000.0),
            None,
            &mut playhead,
            &mut pieces,
        );
        assert!(!walk.start.playing);
        let changes = pieces.inner.changes;
        let change = changes.as_slice()[0];
        assert_eq!(change.at.index(), 100);
        assert!(change.to.playing, "the command's effect");
        assert_eq!(change.to.tempo, Bpm(120.0), "not the racing store");
        // The next block reads it.
        assert_eq!(Control::read(settings).tempo, Bpm(200.0));
    }

    /// A block with no declicked command in sight plans no gain event, and
    /// the fader leaves the output untouched: the fast path is reachable.
    /// A command in sight plans its aim once, not at every walk step.
    ///
    /// Mutation (run): push an aim at every walk step regardless of change
    /// (the reviewed behaviour) → the quiet block's plan is not empty →
    /// fails.
    #[test]
    fn a_quiet_block_plans_no_gain_event() {
        let transport = Transport::new(48_000.0);
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();
        // The engine holds the transport's one playhead writer; the walk
        // under test needs a clock over the same inputs, not a second writer.
        let mut clock = TransportClock::new(
            crate::transport::ClockLinks::bare(
                std::sync::Arc::clone(&transport.settings.tempo),
                std::sync::Arc::clone(&transport.settings.paused),
            ),
            48_000.0,
        );
        let settings = engine.motion.settings();
        let mut pieces = GraphPieces {
            clock: &mut clock,
            settings,
            control: Control::read(settings),
            changes: TransportChanges::NONE,
        };
        let mut playhead = Playhead::new();
        let walk = engine.walk(
            Frame::ZERO,
            512,
            SampleRate(48_000.0),
            None,
            &mut playhead,
            &mut pieces,
        );
        assert_eq!(walk.gain.len, 0, "nothing planned");
        let mut out = vec![0.25f32; 512];
        let mut fader = Fader::new();
        fader.apply(&walk.gain, Frame::ZERO, &mut out, 1, 0, 512);
        assert!(out.iter().all(|&x| x == 0.25), "untouched");

        // A declicked stop 300 frames into the next block: one aim, at 0.
        transport
            .motion
            .schedule(At::Frame(Frame(812)), crate::MotionEvent::stop())
            .expect("room");
        let walk = engine.walk(
            Frame::ZERO,
            512,
            SampleRate(48_000.0),
            None,
            &mut playhead,
            &mut pieces,
        );
        assert_eq!(walk.gain.len, 1);
        assert_eq!(walk.gain.items[0], (0, Gain::Aim(Some(Frame(812)))));
    }
}
