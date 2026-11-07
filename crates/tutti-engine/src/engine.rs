//! The per-buffer graph render, called from the audio callback.
//!
//! [`Engine`] ticks the graph's
//! [`Executor`] and the transport, and renders one
//! output buffer per block. It folds the graph's outputs to the device width, keeps
//! the declick, and applies timestamped transport commands on their frame.
//!
//! # Timestamped transport commands
//!
//! [`MotionFsm::schedule`] queues a play, stop, seek, tempo or loop change at
//! an [`At`]. Each block, the engine walks the commands due in it in time
//! order and **cuts the block's transport** at each one's frame: the pieces
//! before and after run under different transports. The executor never
//! splits a block: the engine advances its own clock piece by
//! piece, records each cut as a
//! [`TransportChange`](tutti_graph::TransportChange) in the block's `Env`,
//! and renders the whole block once. A node reads the transport at a frame
//! with `Env::transport_at`, and the graph's own `At::Beat` commands resolve
//! against the piece that reaches their beat.
//!
//! A block holds at most
//! [`MAX_TRANSPORT_CHANGES`](tutti_graph::MAX_TRANSPORT_CHANGES) cuts. A command due past
//! that lands at the start of the next block and is counted late, like any
//! command already past due ([`MotionFsm::late_command_count`]). (`At::NextBlock`
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
//!   looks one fade (10 ms) past each block for the next declicked
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
    CommitError, Due, Editor, Env, Executor, Limits, MAX_ROOT_CHANNELS, Offset, Playhead,
    TransportChanges,
};

use std::time::Instant;
use tutti_metering::{LoadMeter, LoadRecorder, MasterMeter, MeterWriter};
use tutti_rt::{AudioThreadCell, RtScope};
#[cfg(doc)]
use tutti_transport::MotionFsm;
use tutti_transport::{
    AudioTaken, FadeOut, IMMEDIATE_RESERVE, MotionEvent, SCHEDULE_CAPACITY, Scheduled,
    TransportAudio, TransportCommand,
};
use tutti_types::{At, Frame};
use tutti_types::{InterleavedMut, SampleRate, Samples, Seconds};

/// The block capacity an engine sizes its fold scratch for unless given
/// another ([`Engine::with_capacity`]): tutti-cpal's largest callback
/// (`MAX_FRAMES`). The engine bounds its editor's re-prepares to it.
pub const DEFAULT_BLOCK_CAPACITY: Samples = Samples(8192);

/// Why [`Engine::new`] refused a graph.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphEngineError {
    /// The executor is not the editor's: they were not built together.
    NotAPair,
    /// What the editor already sent is past what the engine can run: more
    /// than [`MAX_ROOT_CHANNELS`] global outputs, or a `MaxBlock` above the
    /// block capacity.
    Limits(CommitError),
    /// Another engine holds `transport`'s audio side (its motion machine,
    /// playhead and commands): one built over it, or over a clone of it, is
    /// still alive. Two would both consume every seek and both write the
    /// playhead ([`Transport::audio`](tutti_transport::Transport::audio)).
    TransportHeld(AudioTaken),
}

impl core::fmt::Display for GraphEngineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAPair => f.write_str("the executor is not the editor's"),
            Self::Limits(e) => write!(f, "the graph is past the engine's limits: {e}"),
            Self::TransportHeld(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GraphEngineError {}

/// The transport as a graph block sees it.
type GraphTransport = tutti_graph::Transport;

/// The audio engine: ticks the graph + transport and renders one
/// output buffer per block from the audio callback.
///
/// # What it owns, and what it deliberately does not
///
/// `Engine` is the *audio-thread half* of the runtime and holds only what a
/// render needs: the audio half of the transport's [`MotionFsm`] (its state
/// machine, as a plain field), the graph's [`Executor`]
/// with the clock that feeds its `Env`, and the declick gain it puts on the
/// output. It owns no graph topology, no parameter storage and no device
/// configuration — the control thread keeps the graph's editing half (the
/// `tutti_graph::Editor`) and hands changes over by committing, so nothing
/// here allocates, locks, or edits a graph.
///
/// That split is the reason this is a distinct type rather than a method on the
/// transport or the graph. Both of those are edited from the control thread;
/// this is touched only from the callback. Fusing it into either would put a
/// control-thread API and an RT-only API on one object, where the compiler could
/// not say which methods are safe to call from where — and the failure is
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
    graph: AudioThreadCell<GraphRender>,
    /// Cached from the transport so the fade path avoids a double deref.
    fader: AudioThreadCell<Fader>,
    /// The block capacity (the editor's `MaxBlock` limit).
    capacity: Samples,
    /// Records each block's render time against its budget.
    load: AudioThreadCell<LoadRecorder>,
    /// The reading half of `load`.
    meter: LoadMeter,
    /// The master meter's audio half, and the stereo fold it measures.
    heard: AudioThreadCell<Heard>,
    /// The reading half of `heard`'s writer.
    master: MasterMeter,
    /// The tap after the fader, the audio thread's ends.
    hearing: AudioThreadCell<crate::hearing::Hearing>,
    /// Its control side.
    heard_taps: crate::HeardTaps,
}

/// What the engine measures of the block it just rendered, after the fader.
struct Heard {
    writer: MeterWriter,
    /// A block folded to stereo, for a device that is not: `capacity`
    /// frames.
    fold: Vec<f32>,
}

impl Heard {
    /// Measures `output`, an interleaved block `channels` wide, as stereo.
    /// A device wider or narrower than stereo is folded first
    /// ([`tutti_types::fold_frame`]), `fold.len() / 2` frames at a time, and
    /// the pieces are one pass: the reading covers the whole block.
    #[inline]
    fn measure(&mut self, output: &[f32], channels: usize) {
        if !self.writer.is_enabled() {
            return;
        }
        if channels == 2 {
            self.writer.measure(output);
            return;
        }
        self.writer.begin();
        let piece = self.fold.len() / 2;
        for block in output.chunks(piece * channels) {
            let stereo = &mut self.fold[..block.len() / channels * 2];
            for (frame, out) in block
                .chunks_exact(channels)
                .zip(stereo.as_chunks_mut::<2>().0)
            {
                tutti_types::fold_frame(frame, out);
            }
            self.writer.add(stereo);
        }
        self.writer.publish();
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("graph", &self.graph)
            .field("fader", &self.fader)
            .field("capacity", &self.capacity)
            .field("meter", &self.meter)
            .field("master", &self.master)
            .finish_non_exhaustive()
    }
}

/// The graph's executor, the clock that feeds its `Env`, and the
/// planar scratch its outputs land in before the fold.
struct GraphRender {
    exec: Executor,
    /// The transport's audio side: motion machine, playhead clock and
    /// commands. The graph reads the transport from `Env`, so the engine
    /// drives the clock itself; every node reads what it publishes from its
    /// block's `Env`.
    transport: TransportAudio,
    /// `MAX_ROOT_CHANNELS` planar channels of `stride` frames each.
    scratch: Vec<f32>,
    stride: usize,
    playhead: Playhead,
    /// The executor's [`dropped_event_count`](Executor::dropped_event_count) already
    /// forwarded to the load meter.
    dropped_seen: u64,
}

impl Engine {
    /// Creates an engine that renders the graph run by `executor`.
    ///
    /// `executor` is the audio half of `editor`'s pair, whose `Prepare` comes
    /// from the device configuration (its rate, and the largest block the
    /// device hands over). The block capacity is the larger of that maximum
    /// and [`DEFAULT_BLOCK_CAPACITY`]; see
    /// [`with_capacity`](Self::with_capacity) for what building an engine
    /// does to the editor and the transport.
    ///
    /// # Errors
    ///
    /// The same as [`with_capacity`](Self::with_capacity).
    pub fn new(
        transport: &tutti_transport::Transport,
        editor: &mut Editor,
        executor: Executor,
    ) -> Result<Self, GraphEngineError> {
        Self::with_capacity(transport, editor, executor, DEFAULT_BLOCK_CAPACITY)
    }

    /// As [`new`](Self::new), with the fold scratch sized for blocks of up
    /// to `capacity` frames (or the prepared maximum, if larger).
    ///
    /// The engine renders whole device blocks through the executor — no
    /// 64-frame chunking; a device block longer than the prepared maximum is rendered as
    /// consecutive graph blocks of at most that — and builds each block's
    /// `Env` from `transport`: the frame is the executor's clock, which
    /// tracks device time, and the transport snapshot comes from the
    /// [`TransportAudio`] the engine takes from `transport`
    /// ([`Transport::audio`](tutti_transport::Transport::audio)) and drives, so
    /// it publishes the playhead, steady time and tempo in force.
    ///
    /// **Bounds the editor** ([`Editor::set_limits`]) to at most
    /// [`MAX_ROOT_CHANNELS`] global outputs and a `MaxBlock` of at most the
    /// capacity, so every later commit or re-prepare past them is refused
    /// there, with a `CommitError`. Refused here, building nothing, when
    /// `executor` is not `editor`'s, or when what the editor already sent is
    /// past them. The editor is borrowed mutably for the check, so no commit
    /// can slip in beside it.
    ///
    /// **One engine per transport.** The engine holds `transport`'s one audio
    /// side ([`Transport::audio`](tutti_transport::Transport::audio)): the
    /// playhead writer and the motion machine, which applies
    /// [`MotionFsm`]'s commands. While this engine is alive, another built
    /// over the same transport, or a clone of it, is refused with
    /// [`GraphEngineError::TransportHeld`] — two would both consume every
    /// seek and both write the playhead. It is given back when this engine is
    /// dropped, so a later engine resumes the same motion state.
    ///
    /// Nor can the graph hold a second writer: only that audio side writes
    /// a playhead. A node that
    /// follows the beat (`ClickNode`, a beat-synced LFO or automation lane)
    /// reads it from its block's `Env`, which shares nothing.
    ///
    /// Control thread. Allocates the fold scratch
    /// (`MAX_ROOT_CHANNELS × capacity` samples).
    ///
    /// # Errors
    ///
    /// Returns an error, leaving `editor` unchanged, when:
    ///
    /// - [`GraphEngineError::NotAPair`]: `executor` is not `editor`'s.
    /// - [`GraphEngineError::TransportHeld`]: another engine over
    ///   `transport` (or a clone of it, or a transport sharing its `motion`
    ///   handle) is alive.
    /// - [`GraphEngineError::Limits`]: what the editor already sent has more
    ///   than [`MAX_ROOT_CHANNELS`] global outputs or a `MaxBlock` above the
    ///   capacity.
    pub fn with_capacity(
        transport: &tutti_transport::Transport,
        editor: &mut Editor,
        executor: Executor,
        capacity: Samples,
    ) -> Result<Self, GraphEngineError> {
        if !editor.is_paired_with(&executor) {
            return Err(GraphEngineError::NotAPair);
        }
        // Before the limits, which change the editor: a refused engine
        // leaves it as it was.
        let rate = executor.prepare().sample_rate();
        let audio = transport
            .audio(rate)
            .map_err(GraphEngineError::TransportHeld)?;
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
        // Seeded from the executor; no test here can cover it, since the
        // executor is always fresh at `Engine::new`.
        let dropped_seen = executor.dropped_event_count();
        let (recorder, meter) = LoadMeter::pair();
        let (writer, master) = MasterMeter::pair();
        let (heard_taps, hearing) = crate::hearing::link();
        Ok(Self {
            graph: AudioThreadCell::new(GraphRender {
                exec: executor,
                transport: audio,
                scratch: vec![0.0; MAX_ROOT_CHANNELS * stride],
                stride,
                playhead: Playhead::new(),
                dropped_seen,
            }),
            fader: AudioThreadCell::new(Fader::new(fade_frames(rate))),
            capacity: Samples(stride),
            load: AudioThreadCell::new(recorder),
            meter,
            heard: AudioThreadCell::new(Heard {
                writer,
                fold: vec![0.0; 2 * stride],
            }),
            master,
            hearing: AudioThreadCell::new(hearing),
            heard_taps,
        })
    }

    /// The block capacity: the largest `MaxBlock` the editor may re-prepare
    /// to.
    pub fn block_capacity(&self) -> Samples {
        self.capacity
    }

    /// The DSP-load meter every [`process`](Self::process) and
    /// [`process_segment`](Self::process_segment) call feeds: the call's
    /// wall time against its block's real-time budget. Clone it to read from
    /// another thread.
    ///
    /// What it times is the whole call, which is what a device deadline
    /// counts, the master meter's pass included; a driver's own work around
    /// it (tutti-cpal's sample-format conversion, one linear pass) is not in
    /// it. The
    /// budget is each graph block's frames at the rate that block ran at,
    /// summed, so a call that straddles a re-prepare's rate change is held
    /// to the rate in effect for each piece. A block refused the graph
    /// (rendered silent, an audible dropout) is not timed; it is counted in
    /// [`LoadReading::silenced`](tutti_metering::LoadReading::silenced). Events the
    /// graph dropped (an event port past its capacity) are counted in
    /// [`LoadReading::dropped_events`](tutti_metering::LoadReading::dropped_events).
    pub fn load_meter(&self) -> &LoadMeter {
        &self.meter
    }

    /// The master meter: the peak and RMS of what [`process`](Self::process)
    /// and [`process_segment`](Self::process_segment) last rendered, measured
    /// on a stereo fold of the output buffer after the declick, whatever
    /// renders it (a device callback, or a host pulling blocks offline).
    /// Disabled until [`enable`](MasterMeter::enable)d, and then one pass
    /// over each block. Clone it to read from another thread.
    pub fn master_meter(&self) -> &MasterMeter {
        &self.master
    }

    /// The control side of the tap after the fader ("what I'm hearing"): the
    /// device-width output of every [`process`](Self::process) and
    /// [`process_segment`](Self::process_segment) call, read through a
    /// [`TapReader`](tutti_graph::TapReader). See [`HeardTaps`](crate::HeardTaps).
    pub fn heard_taps(&self) -> crate::HeardTaps {
        self.heard_taps.clone()
    }

    /// Renders the whole of `output` with no transport motion and no declick.
    ///
    /// `output` is an interleaved device buffer that carries its own width.
    /// This is the pure render.
    ///
    /// One executor block per device block (up to its prepared maximum),
    /// under the
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
        // A monotonic clock read, two per call, no lock and no allocation:
        // normally a vDSO read on Linux (a real syscall if the clocksource
        // is not vDSO-capable, e.g. some VMs), the commpage on macOS, QPC
        // on Windows.
        let start = Instant::now();
        let out_ch = output.stride();
        let frames = output.len();
        let output = output.samples_mut();
        // A free in a block is a debug panic: the hand-over of the heard tap
        // sends what it replaces back to the control side.
        let _rt = RtScope::enter();
        self.hearing.borrow_mut().swap();
        // Refused only while the control side holds the graph
        // (`apply_pending` racing a callback): this block is silent.
        let Some(mut graph) = self.graph.try_borrow_mut() else {
            output.fill(0.0);
            self.heard.borrow_mut().measure(output, out_ch);
            self.hearing.borrow_mut().write(output, out_ch);
            if let Some(mut load) = self.load.try_borrow_mut() {
                load.record_silenced();
            }
            return;
        };
        let g = &mut *graph;
        let mut done = 0;
        let mut budget = Seconds(0.0);
        while done < frames {
            let (bound, rate) = g.settle();
            let len = (frames - done).min(bound);
            budget += Samples(len).to_seconds(rate);
            let t = g.transport.begin_block();
            // Kept current, so a later `process` tells a late beat from one
            // jumped over across this block too.
            g.playhead.observe(&Env {
                frame: g.exec.frame(),
                sample_rate: rate,
                block_len: Samples(len),
                transport: t,
                changes: TransportChanges::NONE,
                play: tutti_graph::PlayClock::ZERO,
            });
            g.transport.advance(len, &t);
            g.render(output, out_ch, done, len, &t, &TransportChanges::NONE);
            self.hearing
                .borrow_mut()
                .write(&output[done * out_ch..(done + len) * out_ch], out_ch);
            done += len;
        }
        self.heard.borrow_mut().measure(output, out_ch);
        let mut load = self.load.borrow_mut();
        load.record_dropped_events(g.take_dropped_events());
        load.record(Seconds(start.elapsed().as_secs_f32()), budget);
    }

    /// Renders one block into `output`, an interleaved device buffer.
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
        // Timed for the load meter (`load_meter`): see `process_segment`.
        let start = Instant::now();
        let out_ch = output.stride();
        let frames = output.len();
        let output = output.samples_mut();
        // A free in a block is a debug panic: the hand-over of the heard tap
        // sends what it replaces back to the control side.
        let _rt = RtScope::enter();
        self.hearing.borrow_mut().swap();
        // The audio thread never panics on a borrow. The graph is refused
        // only while the control side holds it (`apply_pending` racing a
        // callback): this block is silent, and the motion queue, which the
        // render state's motion machine drains, is left for the next one.
        // The fader and the schedule's pending list need no `try_`:
        // the fader is private to this method, and the pending list is
        // reached only under the graph borrow (here, or `apply_pending`'s).
        let Some(mut graph) = self.graph.try_borrow_mut() else {
            output.fill(0.0);
            self.heard.borrow_mut().measure(output, out_ch);
            self.hearing.borrow_mut().write(output, out_ch);
            if let Some(mut load) = self.load.try_borrow_mut() {
                load.record_silenced();
            }
            return;
        };
        let g = &mut *graph;
        let mut done = 0;
        let mut budget = Seconds(0.0);
        while done < frames {
            let (bound, rate) = g.settle();
            let len = (frames - done).min(bound);
            budget += Samples(len).to_seconds(rate);
            let frame0 = g.exec.frame();
            let mut pieces = GraphPieces {
                changes: TransportChanges::NONE,
            };
            let aim = self.fader.borrow().aim;
            let walk = self.walk(
                Track {
                    transport: &mut g.transport,
                    playhead: &mut g.playhead,
                },
                frame0,
                len,
                rate,
                aim,
                &mut pieces,
            );
            let changes = pieces.changes;
            g.render(output, out_ch, done, len, &walk.start, &changes);
            let mut fader = self.fader.borrow_mut();
            fader.fade = fade_frames(rate);
            fader.apply(&walk.gain, frame0, output, out_ch, done, len);
            self.hearing
                .borrow_mut()
                .write(&output[done * out_ch..(done + len) * out_ch], out_ch);
            done += len;
        }
        self.heard.borrow_mut().measure(output, out_ch);
        let mut load = self.load.borrow_mut();
        load.record_dropped_events(g.take_dropped_events());
        load.record(Seconds(start.elapsed().as_secs_f32()), budget);
    }

    /// Walks one block's scheduled transport commands in time order, running
    /// each piece between them through `pieces` and planning the declick
    /// gain over the block. See the module docs for the rules.
    ///
    /// `aim` is the fader's aim going in: an aim is pushed only where it
    /// changes, so a block with no command in sight plans nothing and the
    /// fader skips it.
    fn walk(
        &self,
        track: Track<'_>,
        frame0: Frame,
        frames: usize,
        rate: SampleRate,
        mut aim: Option<Frame>,
        pieces: &mut impl Pieces,
    ) -> Walk {
        let Track {
            transport,
            playhead,
        } = track;
        let fade = fade_frames(rate);
        let mut walk = Walk {
            start: transport.begin_block(),
            gain: GainPlan::new(),
        };
        let mut t = walk.start;
        let mut cursor = 0;
        // The playhead as of the start of the current piece. Each piece is
        // observed with its real length once it is cut; before that, a copy
        // observes it with the rest of the block, to answer `crossed`.
        let mut base = *playhead;
        transport.gather();
        loop {
            let env = Env {
                frame: frame0 + Samples(cursor),
                sample_rate: rate,
                block_len: Samples(frames - cursor),
                transport: t,
                changes: TransportChanges::NONE,
                play: tutti_graph::PlayClock::ZERO,
            };
            // The next declicked command ahead, within one fade of the end of
            // this block: the fade-out that ends on its frame may start here.
            let next = lead_target(transport.pending(), &env, fade);
            if next != aim {
                walk.gain.push(cursor, Gain::Aim(next));
                aim = next;
            }
            let mut ph = base;
            ph.observe(&env);
            // The earliest command due from the cursor on; send order breaks
            // ties.
            let mut best: Option<(usize, usize, bool)> = None;
            for (i, cmd) in transport.pending().iter().enumerate() {
                let (k, late) = match (cmd.at(), env.due(cmd.at())) {
                    (_, Due::In(k)) => (k.index(), false),
                    (At::Frame(_), Due::Late) => (0, true),
                    (At::Beat(b), _) if ph.crossed(b.get()) => (0, true),
                    _ => continue,
                };
                let earlier = best.is_none_or(|(bk, bi, _)| {
                    k < bk || (k == bk && cmd.seq() < transport.pending()[bi].seq())
                });
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
                transport.advance(at - cursor, &t);
                pieces.ran();
                cursor = at;
            }
            let (applied, now) = transport.fire(i, late);
            if applied.declicked() {
                // A jump clears the fader's aim.
                walk.gain.push(cursor, Gain::Jump);
                aim = None;
            }
            t = now;
            match Offset::new(cursor, Samples(frames)) {
                Some(o) if cursor > 0 => pieces.change(o, t),
                _ => walk.start = t,
            }
        }
        base.observe(&Env {
            frame: frame0 + Samples(cursor),
            sample_rate: rate,
            block_len: Samples(frames - cursor),
            transport: t,
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        });
        *playhead = base;
        transport.advance(frames - cursor, &t);
        pieces.ran();
        walk
    }

    /// Installs every commit the engine's editor has sent, outside the audio
    /// callback.
    ///
    /// It also follows a re-prepare's rate change with the engine's clock and
    /// the transport's frame-timed commands — what the first block after it
    /// would do, done now. Returns `true` when the graph is running a plan
    /// with no re-prepare between its halves, `false` while a re-prepare is
    /// still half done.
    ///
    /// Not [`Executor::apply_pending`](tutti_graph::Executor::apply_pending),
    /// which drains only the executor's commit queue: this is the engine-level
    /// call that also moves the clock and the transport.
    ///
    /// A device restart calls it between the two halves of
    /// `Editor::reprepare` so the re-prepare finishes before the first block
    /// at the new rate, which then renders the re-prepared graph rather than
    /// the executor's silent checked-out block.
    /// Hosts reach it through `tutti_cpal::Stopped::apply_pending`, which only
    /// a restart hook is handed.
    ///
    /// Allocates nothing itself; a commit it installs was built on the
    /// control side.
    ///
    /// # Panics
    ///
    /// If an audio callback is inside [`process`] or [`process_segment`] on
    /// this engine at the same moment: this borrows the executor and the
    /// transport schedule the audio thread otherwise owns, and their
    /// [`AudioThreadCell`]s refuse a second borrow in every build. The panic
    /// is on this, the control, side: a callback that meets this call's
    /// borrow instead renders a silent block and does not panic. Call it
    /// only while the stream is stopped (dropped, or not yet started), as
    /// `Stopped` arranges.
    ///
    /// [`process`]: Self::process
    /// [`process_segment`]: Self::process_segment
    pub fn apply_pending(&self) -> bool {
        let graph = &mut *self.graph.borrow_mut();
        graph.settle();
        graph.exec.pending_prepare().is_none() && graph.exec.plan().is_some()
    }
}

/// The render state a block's walk moves: the transport its commands go
/// through, and the playhead it observes.
struct Track<'a> {
    transport: &'a mut TransportAudio,
    playhead: &'a mut Playhead,
}

/// What a block's walk leaves for after the render.
struct Walk {
    /// The transport at the block's first frame, after the commands that
    /// landed there.
    start: GraphTransport,
    /// The declick gain's events in this block.
    gain: GainPlan,
}

/// How long a declick fades, out and in: long enough to kill a click, short
/// enough not to read as a fade.
const FADE_TIME: Seconds = Seconds(0.01);

/// Frames a declick fades over at `rate`, at least one.
fn fade_frames(rate: SampleRate) -> usize {
    FADE_TIME.to_samples(rate).get().max(1)
}

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
fn lead_target(pending: &[Scheduled], env: &Env, fade: usize) -> Option<Frame> {
    if !env.transport.playing {
        return None;
    }
    let look = Env {
        block_len: env.block_len + Samples(fade),
        ..*env
    };
    pending
        .iter()
        .filter(|c| declicks(&c.command()) && !matches!(c.at(), At::NextBlock))
        .filter_map(|c| match look.due(c.at()) {
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
const GAIN_EVENTS: usize = 2 * (SCHEDULE_CAPACITY + IMMEDIATE_RESERVE + 2);

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
        if let Some(last) = self.items[..self.len].last_mut()
            && last.0 == at
            && matches!(last.1, Gain::Aim(_))
            && matches!(gain, Gain::Aim(_))
        {
            last.1 = gain;
            return;
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
/// moves it by at most `1/fade`; a fade-in rises by `1/fade` a frame. The one
/// step is a declicked command with no lead time (`At::NextBlock`, a late
/// command, an untimed `try_send`): the old audio ends on the jump frame
/// with the gain at zero there, and the new audio fades in.
#[derive(Clone, Copy, Debug)]
struct Fader {
    gain: f32,
    aim: Option<Frame>,
    /// Frames a fade spans, at the rate of the block being shaped.
    fade: usize,
}

impl Fader {
    fn new(fade: usize) -> Self {
        Self {
            gain: 1.0,
            aim: None,
            fade,
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
        let fade = self.fade;
        let end = frame0.get() + frames as u64;
        // Nothing to do on a block with no event, full gain, and no fade-out
        // starting inside it: the common case.
        let quiet = self.aim.is_none_or(|to| to.get() > end + fade as u64);
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
                    Some(to) if to.get() - x <= fade as u64 => {
                        let left = (to.get() - x) as f32;
                        self.gain *= left / (left + 1.0);
                    }
                    _ => self.gain = (self.gain + 1.0 / fade as f32).min(1.0),
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

/// What a block walk reports as it cuts the block into pieces. A trait, with
/// one implementation in the engine ([`GraphPieces`]), so a test can wrap it
/// and stage a control-thread store between pieces deterministically.
trait Pieces {
    /// A piece of the block has just run.
    fn ran(&mut self) {}
    /// Whether another cut fits in this block.
    fn room(&self) -> bool;
    /// Record the transport changing to `t` at `at`.
    fn change(&mut self, at: Offset, t: GraphTransport);
}

/// Collects the cuts for the block's `Env`; the render happens once, after.
struct GraphPieces {
    changes: TransportChanges,
}

impl Pieces for GraphPieces {
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
    fn settle(&mut self) -> (usize, SampleRate) {
        self.exec.apply_pending();
        let rate = self
            .exec
            .pending_prepare()
            .unwrap_or(self.exec.prepare())
            .sample_rate();
        // A re-prepare that changed the rate: the executor has rescaled its
        // frame clock; the beat increment and the transport's own
        // frame-timed commands follow.
        self.transport.follow_rate(rate);
        let bound = self.exec.prepare().max_block().get();
        // The editor's limits keep every `MaxBlock` within the scratch.
        debug_assert!(bound <= self.stride, "MaxBlock {bound} past the scratch");
        (bound.min(self.stride), rate)
    }

    /// The events the executor dropped since the last call: a port past its
    /// capacity, or a full delay or feedback FIFO.
    fn take_dropped_events(&mut self) -> u64 {
        let total = self.exec.dropped_event_count();
        let new = total.wrapping_sub(self.dropped_seen);
        self.dropped_seen = total;
        new
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
        // `settle` installed every queued commit, and the executor renders
        // without applying again: a commit sent since waits for the next
        // block, so the width, the block bound and the rate read here are the
        // ones the block renders with. The editor's limits refuse a graph
        // wider than the scratch.
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
            .render_applied(len, transport, changes, &[], &mut outs[..width]);
        // Published after the block, not by the walk before it: through the
        // block the live playhead still reads its first frame (the last
        // block's end), and it only moves forward.
        self.transport.publish_position();
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
    use tutti_graph::Prepare;
    use tutti_transport::{MotionEvent, Transport};
    use tutti_types::Bpm;

    /// A graph walk's `Pieces` that stores a new tempo from "the control
    /// thread" while each piece runs: the store races the walk, staged
    /// deterministically.
    struct Racing<'a> {
        inner: GraphPieces,
        settings: &'a tutti_transport::TransportSettings,
    }

    impl Pieces for Racing<'_> {
        fn ran(&mut self) {
            self.inner.ran();
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
    /// Mutation (run): re-read the control inputs after a command in
    /// `TransportAudio::fire` → the change carries 200 BPM → fails.
    #[test]
    fn an_untimed_store_during_the_walk_waits_for_the_next_block() {
        let transport = Transport::new(48_000.0);
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        transport
            .motion
            .schedule(At::Frame(Frame(100)), MotionEvent::Play)
            .expect("room");
        let settings = &transport.settings;
        let mut render = engine.graph.borrow_mut();
        let mut pieces = Racing {
            inner: GraphPieces {
                changes: TransportChanges::NONE,
            },
            settings,
        };
        let mut playhead = Playhead::new();
        let walk = engine.walk(
            Track {
                transport: &mut render.transport,
                playhead: &mut playhead,
            },
            Frame::ZERO,
            512,
            SampleRate(48_000.0),
            None,
            &mut pieces,
        );
        assert!(!walk.start.playing);
        let changes = pieces.inner.changes;
        let change = changes.as_slice()[0];
        assert_eq!(change.at.index(), 100);
        assert!(change.to.playing, "the command's effect");
        assert_eq!(change.to.tempo, Bpm(120.0), "not the racing store");
        // The next block reads it.
        assert_eq!(render.transport.begin_block().tempo, Bpm(200.0));
    }

    /// The audio thread is never the side that panics: a block that meets
    /// the graph held on the control side (`apply_pending` racing a callback)
    /// renders silence and leaves the command queue for the next block, which
    /// applies it.
    ///
    /// Mutations (run): `process` on a panicking `borrow_mut` → panics →
    /// fails; the refused block not zeroed → its `1.0`s survive → fails.
    #[test]
    fn a_block_refused_the_graph_is_silent_and_retries() {
        use tutti_types::ChannelLayout;
        let transport = Transport::new(48_000.0);
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let _ = transport.motion.try_send(MotionEvent::Play);
        let mut buf = vec![1.0f32; 64 * 2];
        let mut seg = vec![1.0f32; 64 * 2];
        {
            let _control = engine.graph.borrow_mut();
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
            engine.process_segment(&mut InterleavedMut::new(&mut seg, ChannelLayout::STEREO));
        }
        assert!(buf.iter().all(|&x| x == 0.0), "a refused block is silent");
        assert!(seg.iter().all(|&x| x == 0.0), "a refused segment is silent");
        assert!(transport.motion.is_stopped(), "the queued Play waits");
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        assert!(transport.motion.is_playing(), "the next block applies it");
    }

    /// The motion machine outlives the engine that ran it: an engine built
    /// after the first is dropped resumes the same state, so a transport
    /// left rolling can still be stopped.
    ///
    /// Mutations (run): `MotionAudio::drop` not handing the FSM back → the
    /// second engine is refused → fails; handing a fresh FSM back → it is
    /// `Stopped`, so the stop is a no-op → fails. There is no drop order to
    /// keep: the claim is the machine's home, so it comes back with it.
    #[test]
    fn a_later_engine_resumes_the_motion_machine() {
        use tutti_types::ChannelLayout;
        let transport = Transport::new(48_000.0);
        let mut buf = vec![0.0f32; 64 * 2];
        {
            let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
            let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
            let _ = transport.motion.try_send(MotionEvent::Play);
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
            assert!(transport.motion.is_playing());
        }
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let engine = Engine::new(&transport, &mut ed, exec).expect("the first engine is gone");
        let _ = transport.motion.try_send(MotionEvent::stop_unfaded());
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        assert!(
            transport.motion.is_stopped(),
            "the machine knew it was rolling"
        );
    }

    /// A transport whose `motion` handle came from another transport shares
    /// that one's motion machine, which is where the one claim is held: an
    /// engine over it while the other's runs is refused.
    ///
    /// Mutation (run): `MotionFsm::audio` building a fresh machine instead
    /// of popping the home → the second engine is built → fails.
    #[test]
    fn a_shared_motion_handle_is_refused_as_held() {
        let first = Transport::new(48_000.0);
        let mut second = Transport::new(48_000.0);
        second.motion = first.motion.clone();
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let _running = Engine::new(&first, &mut ed, exec).expect("empty graph");
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        assert!(matches!(
            Engine::new(&second, &mut ed, exec),
            Err(GraphEngineError::TransportHeld(_))
        ));
    }

    /// `try_send` and a `NextBlock` command are one queue, so they apply in
    /// the order they were sent: a `Stop` then a `Play` ends rolling, a
    /// `Play` then a `Stop` ends stopped.
    ///
    /// Mutation (run): break the walk's tie-break on `seq` (take the later
    /// send first) → the stop-then-play case ends stopped → fails.
    #[test]
    fn try_send_and_a_next_block_command_apply_in_send_order() {
        use tutti_types::ChannelLayout;
        let run = |first_is_play: bool| {
            let transport = Transport::new(48_000.0);
            let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
            let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
            let stop = MotionEvent::stop_unfaded();
            if first_is_play {
                transport.motion.try_send(MotionEvent::Play).expect("room");
                transport
                    .motion
                    .schedule(At::NextBlock, stop)
                    .expect("room");
            } else {
                transport
                    .motion
                    .schedule(At::NextBlock, stop)
                    .expect("room");
                transport.motion.try_send(MotionEvent::Play).expect("room");
            }
            let mut buf = vec![0.0f32; 64 * 2];
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
            transport.motion.is_playing()
        };
        assert!(!run(true), "play then stop ends stopped");
        assert!(run(false), "stop then play ends rolling");
    }

    /// A locate sent with `try_send` lands on the block's first frame and
    /// its seek is taken once, after the command: the block starts at the
    /// target, a loop wrap inside it is counted once, and the next block
    /// continues from the end of this one without taking the seek again.
    ///
    /// Analytic figures: 120 BPM at 48 kHz is 1/24 000 beat a frame.
    ///
    /// Mutation (run): `TransportAudio::fire` beginning with `take_seek`
    /// false → the seek lands in the next block, so the first block's
    /// published beat is 64 / 24 000, not the target's → fails.
    #[test]
    fn a_locate_on_the_first_frame_is_taken_once_and_a_wrap_counted_once() {
        use tutti_transport::LoopEdit;
        use tutti_types::{Beat, ChannelLayout, LoopRange};
        let engine_for = |transport: &Transport| {
            let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(2048)));
            Engine::new(transport, &mut ed, exec).expect("empty graph")
        };

        let transport = Transport::new(48_000.0);
        let engine = engine_for(&transport);
        transport
            .motion
            .try_send(MotionEvent::play_from(Beat(8.0)))
            .expect("room");
        let mut buf = vec![0.0f32; 64 * 2];
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        let first = transport.settings.beat().get();
        assert!((first - (8.0 + 64.0 / 24_000.0)).abs() < 1e-9, "{first}");
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        let second = transport.settings.beat().get();
        assert!((second - (8.0 + 128.0 / 24_000.0)).abs() < 1e-9, "{second}");

        // Into a loop's last frames: one wrap, counted once.
        let transport = Transport::new(48_000.0);
        let engine = engine_for(&transport);
        transport
            .motion
            .schedule(
                At::NextBlock,
                LoopEdit::over(LoopRange::new(0.0, 4.0).expect("ordered")),
            )
            .expect("room");
        transport
            .motion
            .try_send(MotionEvent::play_from(Beat(3.99)))
            .expect("room");
        let mut buf = vec![0.0f32; 1024 * 2];
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        let wrapped = transport.settings.beat().get();
        let expected = 3.99 + 1024.0 / 24_000.0 - 4.0;
        assert!((wrapped - expected).abs() < 1e-9, "{wrapped} vs {expected}");
    }

    /// A block with no declicked command in sight plans no gain event, and
    /// the fader leaves the output untouched: the fast path is reachable.
    /// A command in sight plans its aim once, not at every walk step.
    ///
    /// Mutation (run): push an aim at every walk step regardless of change
    /// → the quiet block's plan is not empty →
    /// fails.
    #[test]
    fn a_quiet_block_plans_no_gain_event() {
        let transport = Transport::new(48_000.0);
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let _ = transport.motion.try_send(MotionEvent::Play);
        let mut render = engine.graph.borrow_mut();
        let mut pieces = GraphPieces {
            changes: TransportChanges::NONE,
        };
        let mut playhead = Playhead::new();
        let walk = engine.walk(
            Track {
                transport: &mut render.transport,
                playhead: &mut playhead,
            },
            Frame::ZERO,
            512,
            SampleRate(48_000.0),
            None,
            &mut pieces,
        );
        assert_eq!(walk.gain.len, 0, "nothing planned");
        let mut out = vec![0.25f32; 512];
        let mut fader = Fader::new(fade_frames(SampleRate(48_000.0)));
        fader.apply(&walk.gain, Frame::ZERO, &mut out, 1, 0, 512);
        assert!(out.iter().all(|&x| x == 0.25), "untouched");

        // A declicked stop 300 frames into the next block: one aim, at 0.
        transport
            .motion
            .schedule(At::Frame(Frame(812)), tutti_transport::MotionEvent::stop())
            .expect("room");
        let walk = engine.walk(
            Track {
                transport: &mut render.transport,
                playhead: &mut playhead,
            },
            Frame::ZERO,
            512,
            SampleRate(48_000.0),
            None,
            &mut pieces,
        );
        assert_eq!(walk.gain.len, 1);
        assert_eq!(walk.gain.items[0], (0, Gain::Aim(Some(Frame(812)))));
    }

    /// A block refused the graph is a dropout the meter counts, not times:
    /// `silenced` counts both entry points, `blocks` neither, and a reset
    /// zeroes it.
    ///
    /// Mutations (run): `process` not counting the refusal → `silenced` is
    /// 1 → fails; the same in `process_segment` → fails; `reset` leaving
    /// `silenced` → fails.
    #[test]
    fn a_block_refused_the_graph_counts_as_silenced() {
        use tutti_types::ChannelLayout;
        let transport = Transport::new(48_000.0);
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let mut buf = vec![0.0f32; 64 * 2];
        {
            let _control = engine.graph.borrow_mut();
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
            engine.process_segment(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        }
        let r = engine.load_meter().peek();
        assert_eq!((r.silenced, r.blocks), (2, 0));
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        let r = engine.load_meter().peek();
        assert_eq!((r.silenced, r.blocks), (2, 1));
        engine.load_meter().reset();
        assert_eq!(engine.load_meter().peek().silenced, 0);
    }

    /// `width` outputs, channel `c` holding `10 * width + c + 1`.
    struct Ramp(u16);

    impl tutti_graph::Node for Ramp {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::audio(
                tutti_types::ChannelLayout::EMPTY,
                tutti_types::ChannelLayout::from_count(self.0),
            )
            .with_tail(tutti_types::Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            for c in 0..usize::from(self.0) {
                io.output(c).fill((10 * usize::from(self.0) + c + 1) as f32);
            }
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// Wire the globals to a `Ramp` of `width` at key 1, replacing any, and
    /// commit.
    fn rewire(ed: &mut Editor, width: u16) {
        use tutti_types::NodeKey;
        use tutti_types::graph::{OutPort, Source};
        ed.insert(NodeKey(1), "ramp", tutti_graph::Unforkable(Ramp(width)));
        ed.spec_mut().topology.outputs = (0..width)
            .map(|port| {
                Source::Node(OutPort {
                    node: NodeKey(1),
                    port,
                })
            })
            .collect();
        ed.commit().expect("commits");
    }

    /// One graph block as `process_segment` runs it, with `between` run after
    /// `settle` and before `render`: the control thread's commit landing in
    /// the gap.
    fn block_with(engine: &Engine, out_ch: usize, between: impl FnOnce()) -> Vec<f32> {
        block_of(engine, out_ch, 8, between)
    }

    /// [`block_with`] for a block of `want` frames (at most the bound).
    fn block_of(engine: &Engine, out_ch: usize, want: usize, between: impl FnOnce()) -> Vec<f32> {
        let mut g = engine.graph.borrow_mut();
        let (bound, _) = g.settle();
        assert!(bound >= want);
        let len = want;
        between();
        let t = g.transport.begin_block();
        g.transport.advance(len, &t);
        let mut out = vec![-9.0f32; len * out_ch];
        g.render(&mut out, out_ch, 0, len, &t, &TransportChanges::NONE);
        out
    }

    /// A commit that lands between `settle` and `render` waits for the next
    /// block: this block renders the plan `settle` left, at its width, and
    /// the next one the new plan. Deterministic: the gap is staged, not
    /// timed. From no plan, then widening.
    ///
    /// Mutation (run): `render` calls `process_with_changes` (which applies
    /// again) with `outs[..width]` → the first block panics (empty slice) →
    /// fails.
    #[test]
    fn a_commit_between_settle_and_render_waits_for_the_next_block() {
        use std::cell::RefCell;
        let transport = Transport::new(48_000.0);
        let (ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        let ed = RefCell::new(ed);
        let engine = Engine::new(&transport, &mut ed.borrow_mut(), exec).expect("empty graph");
        let want = |w: usize| -> Vec<f32> {
            (0..8)
                .flat_map(|_| (0..w).map(|c| (10 * w + c + 1) as f32))
                .collect()
        };
        // No plan at `settle`: this block is silent.
        let got = block_with(&engine, 2, || rewire(&mut ed.borrow_mut(), 2));
        assert_eq!(got, vec![0.0; 16]);
        // The next one has it.
        assert_eq!(block_with(&engine, 2, || {}), want(2));
        // Widening to 4 in the gap: this block is still 2 wide.
        assert_eq!(
            block_with(&engine, 2, || rewire(&mut ed.borrow_mut(), 4)),
            want(2)
        );
        assert_eq!(block_with(&engine, 4, || {}), want(4));
    }

    /// A narrowing commit in the gap: this block folds the wide plan whole,
    /// the next the narrow one, with no stale channel in it.
    ///
    /// Mutation (run): `render` calls `process_with_changes` → the gap block
    /// renders the narrow plan, folded at the wide width over stale channels
    /// → fails.
    #[test]
    fn a_narrowing_commit_between_settle_and_render_waits_for_the_next_block() {
        use std::cell::RefCell;
        let transport = Transport::new(48_000.0);
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        rewire(&mut ed, 4);
        let ed = RefCell::new(ed);
        let engine = Engine::new(&transport, &mut ed.borrow_mut(), exec).expect("within limits");
        let wide: Vec<f32> = (0..8).flat_map(|_| [41.0, 42.0, 43.0, 44.0]).collect();
        let narrow: Vec<f32> = (0..8).flat_map(|_| [21.0, 22.0]).collect();
        assert_eq!(block_with(&engine, 4, || {}), wide);
        let got = block_with(&engine, 4, || rewire(&mut ed.borrow_mut(), 2));
        assert_eq!(got, wide);
        assert_eq!(block_with(&engine, 2, || {}), narrow);
    }

    /// A re-prepare's resume landing in the gap changes neither the block
    /// bound nor the rate under the block: this block (settled at 512)
    /// renders at 512, and the next is bound at 256.
    ///
    /// Mutation (run): `render` calls `process_with_changes` → "block of 512
    /// frames against a max of 256" → fails.
    #[test]
    fn a_resume_between_settle_and_render_waits_for_the_next_block() {
        use std::cell::RefCell;
        let transport = Transport::new(48_000.0);
        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
        rewire(&mut ed, 2);
        let ed = RefCell::new(ed);
        let engine = Engine::new(&transport, &mut ed.borrow_mut(), exec).expect("within limits");
        ed.borrow_mut()
            .reprepare(Prepare::new(SampleRate(48_000.0), Samples(256)))
            .expect("shrinks");
        // The suspend commit lands in this block's `settle`; silent.
        let _ = block_of(&engine, 2, 512, || {});
        // The resume is sent after `settle` bound the block at 512.
        let _ = block_of(&engine, 2, 512, || drop(ed.borrow_mut().collect()));
        let max = |e: &Engine| e.graph.borrow_mut().exec.prepare().max_block().get();
        assert_eq!(max(&engine), 512, "not yet applied");
        let got = block_of(&engine, 2, 256, || {});
        assert_eq!(max(&engine), 256);
        let want: Vec<f32> = (0..256).flat_map(|_| [21.0, 22.0]).collect();
        assert_eq!(got, want);
    }
}
