//! The per-buffer graph render, called from the audio callback.
//!
//! [`Engine`] ticks the DSP graph + transport and renders one output buffer per
//! block. It renders one of two graph runtimes, chosen at construction:
//! fundsp's `Net` ([`Engine::new`]) or the native graph's
//! [`Executor`](tutti_graph::Executor) ([`Engine::with_graph`], doc 013
//! Phase 2). Both fold the graph's outputs to the device width the same way,
//! keep the same declick, and apply timestamped transport commands on their
//! frame.
//!
//! # Timestamped transport commands
//!
//! [`MotionFsm::schedule`] queues a play, stop, seek, tempo or loop change at
//! an [`At`]. Each block, the engine walks the commands due in it in time
//! order and **cuts the block's transport** at each one's frame: the pieces
//! before and after run under different transports. What a cut means
//! depends on the runtime:
//!
//! - **Graph**: the executor never splits a block (doc 013 §6). The engine
//!   advances its own clock piece by piece, records each cut as a
//!   [`TransportChange`](tutti_graph::TransportChange) in the block's `Env`,
//!   and renders the whole block once. A node reads the transport at a frame
//!   with `Env::transport_at`, and the graph's own `At::Beat` commands resolve
//!   against the piece that reaches their beat. A `Legacy` unit reads no
//!   `Env`: one that follows the transport polls the live `Transport` (a
//!   sampler voice's `Arc<dyn Timeline>`) per 64-frame call, so while the
//!   block renders the engine **seats** the published playhead on the beat
//!   its clock had on each chunk's first frame (recorded as the walk
//!   advances it), and puts it back at the block's end afterwards
//!   ([`LegacyClock`], doc 013 §6). The play state stays the block's end
//!   state for such a unit: only the position is seated.
//! - **Net**: the engine renders the `Net` piece by piece (it already renders
//!   in 64-frame chunks, so this breaks no promise), applying the commands
//!   between pieces; the `TransportClock` inside the net reads them at the
//!   start of the next piece.
//!
//! A block holds at most [`MAX_TRANSPORT_CHANGES`] cuts. A command due past
//! that lands at the start of the next block and is counted late, like any
//! command already past due ([`MotionFsm::late_commands`]). (`At::NextBlock`
//! commands, late ones and crossed beats all land at the block's first frame
//! and need no cut, so they are never deferred.) Commands due on one frame
//! apply in send order.
//!
//! **Untimed state is read once per block.** On the Graph path the tempo,
//! loop and play state are read at the start of the walk; a store from the
//! control thread during the block lands at the next one, and only an
//! applied command changes them at a cut. The Net path cannot promise that:
//! its `TransportClock` is a node in the net and reads the shared atomics at
//! every 64-frame chunk, as it always has. The engine resolves beats for it
//! with the tempo the clock actually runs at (`TransportSettings::
//! tempo_in_force`, its hysteresis applied).
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
//! # Limits of a graph engine
//!
//! [`Engine::with_graph`] bounds the graph's editor ([`Editor::set_limits`])
//! to what its fold scratch holds: at most [`MAX_ROOT_CHANNELS`] global
//! outputs, and a `MaxBlock` no larger than its block capacity (the larger
//! of the prepared maximum and [`DEFAULT_GRAPH_BLOCK_CAPACITY`], or the
//! capacity given to [`Engine::with_graph_capacity`]). A commit or a
//! re-prepare past them is refused on the control thread
//! (`CommitError::TooManyOutputs`, `CommitError::BlockTooLong`), so the
//! callback never meets a graph it cannot render.

use tutti_graph::{
    CommitError, Due, Editor, Env, Executor, LegacyClock, Limits, Offset, Playhead,
    TransportChanges, LEGACY_CHUNK, MAX_TRANSPORT_CHANGES,
};

use crate::transport::fsm::DEFAULT_DECLICK_FRAMES;
use crate::transport::{Scheduled, SCHEDULE_CAPACITY};
use tutti_types::{At, Frame};

use crate::transport::{tempo_in_effect, Control};
use crate::transport::{
    FadeOut, MotionEvent, MotionFsm, MotionState, TransportClock, TransportCommand,
};
use crate::{AudioThreadCell, Beat, InterleavedMut, Ordering, SampleRate, Samples};
use fundsp::audiounit::AudioUnit;
use fundsp::buffer::BufferArray;
use fundsp::prelude::{BufferRef, U8};
use fundsp::realnet::NetBackend;
use fundsp::MAX_BUFFER_SIZE;

/// Widest graph root [`Engine::process_segment`] renders without dropping
/// channels — and the widest output (device) width it folds to. The scratch is
/// stack-allocated, so this is a fixed ceiling: mono through 7.1. A root or
/// device wider than this clamps (its extra channels are dropped / silent).
///
/// For the native graph the ceiling is harder: the executor must be handed a
/// buffer for **every** global output, and the engine's scratch holds this
/// many. So a graph engine refuses more: [`Engine::with_graph`] bounds the
/// editor to it, and a commit with more global outputs is an error on the
/// control thread.
pub const MAX_ROOT_CHANNELS: usize = 8;

/// The block capacity a graph engine sizes its fold scratch for unless given
/// another ([`Engine::with_graph_capacity`]): tutti-cpal's largest callback
/// (`MAX_FRAMES`). The engine bounds its editor's re-prepares to it.
pub const DEFAULT_GRAPH_BLOCK_CAPACITY: Samples = Samples(8192);

/// Why [`Engine::with_graph`] refused a graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEngineError {
    /// The executor is not the editor's: they were not built together.
    NotAPair,
    /// What the editor already sent is past what the engine can run: more
    /// than [`MAX_ROOT_CHANNELS`] global outputs, or a `MaxBlock` above the
    /// block capacity.
    Limits(CommitError),
}

impl core::fmt::Display for GraphEngineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAPair => f.write_str("the executor is not the editor's"),
            Self::Limits(e) => write!(f, "the graph is past the engine's limits: {e}"),
        }
    }
}

impl std::error::Error for GraphEngineError {}

/// Type-level [`MAX_ROOT_CHANNELS`], for sizing the scratch [`BufferArray`].
type MaxRootChannels = U8;

/// The transport as a native graph block sees it.
type GraphTransport = tutti_graph::Transport;

/// The audio engine: ticks the DSP graph + transport and renders one output
/// buffer per block from the audio callback.
///
/// # What it owns, and what it deliberately does not
///
/// `Engine` is the *audio-thread half* of the runtime and holds only what a
/// render needs: the transport's [`MotionFsm`], the graph runtime (a committed
/// [`NetBackend`], or a native [`Executor`] with the clock that feeds its
/// `Env`), and the declick gain it puts on the output. It owns no graph topology, no parameter
/// storage and no device configuration — the control thread keeps the graph's
/// editing half (fundsp's `Net` frontend, or the `tutti_graph::Editor`) and
/// hands changes over by committing, so nothing here allocates, locks, or
/// edits a graph.
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
    backend: AudioThreadCell<Backend>,
    /// Cached from the transport so the fade path avoids a double deref.
    fader: AudioThreadCell<Fader>,
    /// A graph engine's block capacity (its editor's `MaxBlock` limit).
    graph_capacity: Option<Samples>,
}

/// The graph runtime an engine renders. An enum rather than a trait object:
/// two variants, matched once per block, keeps the RT path monomorphic.
///
/// The variants differ in size (the executor is about twice the `Net`
/// side); there is one per engine, held in place for its life, so boxing
/// either would buy a pointer hop a block and save nothing.
#[allow(clippy::large_enum_variant)]
enum Backend {
    Net(NetRender),
    Graph(GraphRender),
}

/// fundsp's `Net`, and the transport bookkeeping the engine keeps beside it.
struct NetRender {
    backend: NetBackend,
    /// Frames rendered since the engine was built: the clock `At::Frame`
    /// names. (The native graph's executor keeps its own.)
    frame: Frame,
    playhead: Playhead,
}

/// The native graph's executor, the clock that feeds its `Env`, and the
/// planar scratch its outputs land in before the fold.
struct GraphRender {
    exec: Executor,
    /// The engine's playhead. A `Net` carries its `TransportClock` as a node;
    /// the native graph reads the transport from `Env`, so the engine drives
    /// the same clock itself ([`TransportClock::begin`] /
    /// [`TransportClock::advance`]) and the two backends see the same beats.
    clock: TransportClock,
    /// The clock's beat on the first frame of each `Legacy` chunk
    /// ([`LEGACY_CHUNK`] frames from the graph block's start) of the block
    /// being rendered: recorded as the walk advances the clock, and
    /// published to the live playhead chunk by chunk while the block renders
    /// ([`Seats`]). Sized for a `stride`-frame block.
    seats: Vec<Beat>,
    /// `MAX_ROOT_CHANNELS` planar channels of `stride` frames each.
    scratch: Vec<f32>,
    stride: usize,
    playhead: Playhead,
}

impl Engine {
    /// Build an engine over a transport's motion FSM and a committed graph
    /// backend.
    ///
    /// `net_backend` is the audio-thread half of fundsp's `Net`; the control
    /// thread keeps the frontend and hands changes over by committing.
    pub fn new(motion: MotionFsm, net_backend: NetBackend) -> Self {
        Self {
            motion,
            backend: AudioThreadCell::new(Backend::Net(NetRender {
                backend: net_backend,
                frame: Frame::ZERO,
                playhead: Playhead::new(),
            })),
            fader: AudioThreadCell::new(Fader::new()),
            graph_capacity: None,
        }
    }

    /// Build an engine that renders a native graph: `executor`, the audio
    /// half of `editor`'s pair, whose `Prepare` comes from the device
    /// configuration (its rate, and the largest block the device hands
    /// over). The block capacity is the larger of that maximum and
    /// [`DEFAULT_GRAPH_BLOCK_CAPACITY`]; see
    /// [`with_graph_capacity`](Self::with_graph_capacity).
    pub fn with_graph(
        transport: &crate::Transport,
        editor: &mut Editor,
        executor: Executor,
    ) -> Result<Self, GraphEngineError> {
        Self::with_graph_capacity(transport, editor, executor, DEFAULT_GRAPH_BLOCK_CAPACITY)
    }

    /// As [`with_graph`](Self::with_graph), with the fold scratch sized for
    /// blocks of up to `capacity` frames (or the prepared maximum, if
    /// larger).
    ///
    /// The engine renders whole device blocks through the executor — no
    /// 64-frame chunking; a device block longer than the prepared maximum is
    /// rendered as consecutive graph blocks of at most that — and builds each
    /// block's `Env` from `transport`: the frame is the executor's clock,
    /// which tracks device time, and the transport snapshot comes from a
    /// [`TransportClock`] the engine drives over `transport`'s
    /// [`clock_links`](crate::Transport::clock_links), so it publishes the
    /// playhead, steady time and tempo in force as the clock node in a `Net`
    /// would.
    ///
    /// **Bounds the editor** ([`Editor::set_limits`]) to at most
    /// [`MAX_ROOT_CHANNELS`] global outputs and a `MaxBlock` of at most the
    /// capacity, so every later commit or re-prepare past them is refused
    /// there, with a `CommitError`. Refused here, building nothing, when
    /// `executor` is not `editor`'s, or when what the editor already sent is
    /// past them. The editor is borrowed mutably for the check, so no commit
    /// can slip in beside it.
    ///
    /// The graph must not also hold a `TransportClock` of its own: two clocks
    /// would both consume the seek and both write the playhead. A node that
    /// takes the beat as a signal (`ClickNode`, a beat-driven LFO or
    /// automation lane) is fed by an [`EnvClock`](crate::EnvClock) instead,
    /// which emits the same samples from the block's `Env` and shares
    /// nothing.
    ///
    /// Control thread. Allocates the fold scratch
    /// (`MAX_ROOT_CHANNELS × capacity` samples).
    pub fn with_graph_capacity(
        transport: &crate::Transport,
        editor: &mut Editor,
        executor: Executor,
        capacity: Samples,
    ) -> Result<Self, GraphEngineError> {
        if !editor.is_paired_with(&executor) {
            return Err(GraphEngineError::NotAPair);
        }
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
            backend: AudioThreadCell::new(Backend::Graph(GraphRender {
                exec: executor,
                clock: TransportClock::new(transport.clock_links(), rate),
                seats: vec![Beat(0.0); stride.div_ceil(LEGACY_CHUNK)],
                scratch: vec![0.0; MAX_ROOT_CHANNELS * stride],
                stride,
                playhead: Playhead::new(),
            })),
            fader: AudioThreadCell::new(Fader::new()),
            graph_capacity: Some(Samples(stride)),
        })
    }

    /// A graph engine's block capacity: the largest `MaxBlock` its editor may
    /// re-prepare to. `None` for a `Net` engine.
    pub fn graph_block_capacity(&self) -> Option<Samples> {
        self.graph_capacity
    }

    /// Render the whole of `output` — an interleaved device buffer that carries
    /// its own width — with no transport motion and no declick: the pure
    /// render.
    ///
    /// For a `Net`, drives the graph through fundsp's SIMD block path
    /// ([`NetBackend::process`]) in [`MAX_BUFFER_SIZE`] chunks rather than one
    /// frame at a time; for a native graph, one executor block per device
    /// block (up to its prepared maximum), under the transport as it stands.
    /// The graph root has no inputs. The root is rendered at its **own**
    /// output width (up to [`MAX_ROOT_CHANNELS`]) into scratch, then each
    /// frame is folded to the *output's* width — the device / target width —
    /// via the ITU/Dolby matrices ([`tutti_types::fold_frame`]): a surround
    /// root plays folded to a stereo device, or straight through to a
    /// matching-width surround device; a mono root duplicates into every
    /// target channel of a wider output.
    ///
    /// # Three widths are live here; only one is the buffer's
    ///
    /// The output width (`out_ch`) comes from `output`'s own layout; the
    /// root's from the graph (`backend.outputs()`, or the plan's global
    /// outputs), clamped to the scratch. They are different numbers from
    /// different sources, and confusing them writes past the end of one
    /// buffer or reads garbage from the other. The output width arrives welded
    /// to the buffer it strides, which is what makes the third confusion — a
    /// width disagreeing with its slice — unrepresentable rather than merely
    /// unlikely.
    ///
    /// For a `Net`, the scratch is a stack-allocated [`BufferArray`] sized to
    /// [`MAX_ROOT_CHANNELS`], **sliced to the root's actual output count**
    /// before each `process` call. The slicing is load-bearing:
    /// `Net::process` iterates `output.channels()` and indexes its own
    /// `output_edge` table by that channel, so handing it a wider buffer than
    /// the net's width indexes past the end and panics — in release, inside
    /// the audio callback. The whole path is alloc-free (stack scratch + a
    /// stack `[f32; MAX_ROOT_CHANNELS]` frame).
    #[inline]
    pub fn process_segment(&self, output: &mut InterleavedMut<'_>) {
        let out_ch = output.stride();
        let frames = output.len();
        let output = output.samples_mut();
        match &mut *self.backend.borrow_mut() {
            Backend::Net(net) => {
                net.backend.pump();
                render_net(&mut net.backend, output, out_ch, 0, frames);
                net.frame += Samples(frames);
            }
            Backend::Graph(g) => {
                let mut done = 0;
                while done < frames {
                    let (bound, rate) = g.settle();
                    let len = (frames - done).min(bound);
                    let control = Control::read(self.motion.settings());
                    let t = g.clock.begin(&control, true);
                    // Kept current, so a later `process` tells a late beat
                    // from one jumped over across this block too.
                    g.playhead.observe(&Env {
                        frame: g.exec.frame(),
                        sample_rate: rate,
                        block_len: Samples(len),
                        transport: t,
                        changes: TransportChanges::NONE,
                    });
                    run_clock(&mut g.clock, &mut g.seats, 0, len, &t);
                    g.render(output, out_ch, done, len, &t, &TransportChanges::NONE);
                    done += len;
                }
            }
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
        match &mut *self.backend.borrow_mut() {
            Backend::Net(net) => {
                net.backend.pump();
                let rate = SampleRate(net.backend.sample_rate());
                let frame0 = net.frame;
                let mut pieces = NetPieces {
                    engine: self,
                    backend: &mut net.backend,
                    output,
                    out_ch,
                    cuts: 0,
                };
                let aim = self.fader.borrow().aim;
                let walk = self.walk(frame0, frames, rate, aim, &mut net.playhead, &mut pieces);
                self.fader
                    .borrow_mut()
                    .apply(&walk.gain, frame0, output, out_ch, 0, frames);
                net.frame += Samples(frames);
            }
            Backend::Graph(g) => {
                let mut done = 0;
                while done < frames {
                    let (bound, rate) = g.settle();
                    let len = (frames - done).min(bound);
                    let frame0 = g.exec.frame();
                    let mut pieces = GraphPieces {
                        clock: &mut g.clock,
                        seats: &mut g.seats,
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

    /// Reset the audio-thread ownership assertions on both cells.
    ///
    /// Call when the device switches and a different thread takes over the
    /// callback: `AudioThreadCell` pins the first thread that borrows it and
    /// panics in debug builds on any other, so a new callback thread must be
    /// announced rather than discovered.
    pub fn reset_owners(&self) {
        self.backend.reset_owner();
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

/// One runtime's side of a block walk.
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

/// The `Net` side: render each piece as it is cut; the transport comes from
/// the shared atomics, which the net's `TransportClock` reads at the start of
/// each piece.
struct NetPieces<'a> {
    engine: &'a Engine,
    backend: &'a mut NetBackend,
    output: &'a mut [f32],
    out_ch: usize,
    cuts: usize,
}

impl Pieces for NetPieces<'_> {
    fn begin(&mut self, _: Option<&TransportCommand>) -> GraphTransport {
        let motion = &self.engine.motion;
        let settings = motion.settings();
        // A seek not yet taken by the clock is where it will emit from.
        let beat = if motion.seek.is_pending() {
            crate::Beat(motion.seek.target.load(Ordering::Acquire))
        } else {
            settings.beat()
        };
        GraphTransport {
            playing: !settings.is_paused(),
            // The tempo the net's clock will run the next piece at: the one
            // asked for, through its hysteresis.
            tempo: tempo_in_effect(
                settings.tempo(),
                crate::Bpm(settings.tempo_in_force.load(Ordering::Acquire)),
            ),
            beat,
            looping: settings.loop_span.range().map(|r| tutti_graph::LoopRange {
                start: r.start(),
                end: r.end(),
            }),
        }
    }

    fn run(&mut self, start: usize, end: usize, _: &GraphTransport) {
        render_net(self.backend, self.output, self.out_ch, start, end);
    }

    fn room(&self) -> bool {
        self.cuts < MAX_TRANSPORT_CHANGES
    }

    fn change(&mut self, _: Offset, _: GraphTransport) {
        self.cuts += 1;
    }
}

/// The native graph side: advance the engine's clock over each piece, and
/// collect the cuts for the block's `Env` and the chunk seats; the render
/// happens once, after.
struct GraphPieces<'a> {
    clock: &'a mut TransportClock,
    /// [`GraphRender::seats`].
    seats: &'a mut [Beat],
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
        run_clock(self.clock, self.seats, start, end, t);
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
    fn settle(&mut self) -> (usize, SampleRate) {
        self.exec.apply_pending();
        let rate = self.exec.prepare().sample_rate();
        if rate != self.clock.sample_rate() {
            // A re-prepare changed the rate: the executor has rescaled its
            // frame clock; the beat increment follows.
            AudioUnit::set_sample_rate(&mut self.clock, rate);
        }
        let bound = self.exec.prepare().max_block().get();
        // The editor's limits keep every `MaxBlock` within the scratch.
        debug_assert!(bound <= self.stride, "MaxBlock {bound} past the scratch");
        (bound.min(self.stride), rate)
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
        let seats = Seats {
            clock: &self.clock,
            seats: &self.seats[..len.div_ceil(LEGACY_CHUNK)],
        };
        self.exec
            .process_with_clock(len, transport, changes, &seats, &[], &mut outs[..width]);
        // The last seat is wherever the last `Legacy` chunk began: put the
        // playhead back where the block ends, where the walk left the clock.
        self.clock.publish_position(self.clock.current_beat());
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

/// Advance `clock` over frames `start..end` of a graph block under `t`,
/// recording in `seats` its beat on each [`LEGACY_CHUNK`] boundary it
/// crosses (a `Legacy` chunk's first frame, counted from the block's start).
///
/// The clock steps frame by frame either way, so stepping it chunk by chunk
/// lands on the bit it would reach in one call; only the position writeback
/// and the steady-time count happen per chunk, and they end where one call
/// would leave them. A cut at a chunk boundary records the transport after
/// the cut: `walk` runs up to the cut, applies it, then runs on from it.
fn run_clock(
    clock: &mut TransportClock,
    seats: &mut [Beat],
    start: usize,
    end: usize,
    t: &GraphTransport,
) {
    let mut at = start;
    while at < end {
        if at.is_multiple_of(LEGACY_CHUNK) {
            seats[at / LEGACY_CHUNK] = clock.current_beat();
        }
        let next = ((at / LEGACY_CHUNK + 1) * LEGACY_CHUNK).min(end);
        clock.advance(next - at, t);
        at = next;
    }
}

/// The live [`LegacyClock`]: seats the playhead a `Transport` reports (and so
/// every sampler voice or other `Legacy` unit polling it) on the beat the
/// engine's clock had on each chunk's first frame, as a `Net` engine's clock
/// node published it between its 64-frame chunks (doc 013 §6).
struct Seats<'a> {
    clock: &'a TransportClock,
    /// [`GraphRender::seats`], for this block.
    seats: &'a [Beat],
}

impl LegacyClock for Seats<'_> {
    fn seat(&self, at: Offset) {
        debug_assert!(
            at.index().is_multiple_of(LEGACY_CHUNK),
            "a chunk's first frame"
        );
        self.clock
            .publish_position(self.seats[at.index() / LEGACY_CHUNK]);
    }
}

/// Render frames `start..end` of the interleaved `output` (`out_ch` wide)
/// through `backend`, in `MAX_BUFFER_SIZE` chunks from `start`.
///
/// Destructured by the caller, per the `Interleaved` rule: the stride and the
/// frame count are read once and the loops below index raw.
#[inline]
fn render_net(
    backend: &mut NetBackend,
    output: &mut [f32],
    out_ch: usize,
    start: usize,
    end: usize,
) {
    debug_assert!(backend.inputs() == 0);
    // The root's real width, clamped to what the scratch can hold. A wider
    // root drops its extra channels (they can't be rendered), but must never
    // index past the buffer.
    // NOTE this is the ROOT's width, not the output buffer's. `out_ch` is the
    // output's, and it is NOT clamped to MAX_ROOT_CHANNELS — `fold_frame`
    // writes exactly `out_ch` channels (zero-filling any past the root
    // width), so a wider-than-8 device simply gets silent extra channels.
    // Only the render scratch is bounded. The caller pumped the backend
    // first, so a just-committed width change is reflected here.
    let root_channels = backend.outputs().clamp(1, MAX_ROOT_CHANNELS);

    let empty_input = BufferRef::new(&[]);
    let mut scratch = BufferArray::<MaxRootChannels>::new();

    let mut done = start;
    while done < end {
        let block = (end - done).min(MAX_BUFFER_SIZE);

        // Slice to the root's width so `Net::process` iterates exactly the
        // channels it has edges for.
        let mut full = scratch.buffer_mut();
        let mut buffer_mut = full.subset(0, root_channels);
        backend.process(block, &empty_input, &mut buffer_mut);

        // Fold each planar frame (root_channels wide) to the interleaved
        // output width. Gather into a stack frame sliced to the root width —
        // no allocation.
        for i in 0..block {
            let mut frame = [0.0f32; MAX_ROOT_CHANNELS];
            let src = &mut frame[..root_channels];
            for (c, s) in src.iter_mut().enumerate() {
                *s = buffer_mut.channel_f32(c)[i];
            }
            let o = (done + i) * out_ch;
            tutti_types::fold_frame(src, &mut output[o..o + out_ch]);
        }

        done += block;
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
        let engine = Engine::with_graph(&transport, &mut ed, exec).expect("empty graph");
        transport
            .motion
            .schedule(At::Frame(Frame(100)), MotionEvent::Play)
            .expect("room");
        let mut clock = TransportClock::new(transport.clock_links(), 48_000.0);
        let settings = engine.motion.settings();
        let mut pieces = Racing {
            inner: GraphPieces {
                clock: &mut clock,
                seats: &mut [Beat(0.0); 512 / LEGACY_CHUNK],
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
        let engine = Engine::with_graph(&transport, &mut ed, exec).expect("empty graph");
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();
        let mut clock = TransportClock::new(transport.clock_links(), 48_000.0);
        let settings = engine.motion.settings();
        let mut pieces = GraphPieces {
            clock: &mut clock,
            seats: &mut [Beat(0.0); 512 / LEGACY_CHUNK],
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
