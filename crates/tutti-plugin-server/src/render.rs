//! The render thread.
//!
//! The server runs two threads once the host has connected its audio and
//! control connections:
//!
//! - **The main thread** keeps the plugin's `Controller` and its editor, and
//!   serves the control connection and the idle tick ([`crate::main_loop`]).
//!   An editor's paint runs there, and may take as long as it takes.
//! - **The render thread** (this module) owns the plugin's `Processor` and the
//!   audio connection. It reads `ProcessAudio` and `Reset`, renders, and
//!   answers. **Nothing on that path is shared with the main thread but the
//!   queues below**: it takes no lock the main thread holds, so a paint can
//!   never delay a block.
//!
//! What crosses between them:
//!
//! - **The Processor** travels through `tutti_rt`'s handoff: the main thread lends it at an
//!   activation and recalls it before a
//!   deactivation, and this thread trades it at its next poll (between blocks,
//!   and every [`RENDER_WAIT`] while the host is quiet, so a recall is never
//!   left waiting for a block).
//! - **Commands** ([`ToRender`]) travel one way, main to render, through a
//!   `ringbuf` queue: the slab, the sizes the pipeline reserves at a load, the
//!   figures a block reads ([`RenderConfig`]) and the seat the Processor
//!   arrives in. What the render thread has finished with goes back through a
//!   second queue ([`Back`]) to be dropped on the main thread: a plugin's
//!   objects are never freed on the render thread.
//!
//! Commands are applied before the request that follows them is served, so a
//! command the main thread pushed before it answered the host is in effect for
//! every block the host sends after that answer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_plugin_ipc::{BridgeMessage, HostMessage, SlabWriter};
use tutti_plugin_types::{
    AudioBufferMut, BlockStatus, BridgeError, BridgeResult, Features, MidiEventVec, PluginResult,
    ProcessContext, ProcessOutput, Processor, SampleFormat,
};
use tutti_rt::Rt;

use crate::audio_pipeline::{AudioBlock, AudioPipeline, Clock, ProcessExtras};
use crate::served::{Render, through};
use crate::transport::Transport;

/// How long the render thread waits for a request before it looks at its
/// commands and its seat again: the longest a recall of the Processor waits
/// for a host that sends no blocks. A block's request ends the wait at once.
pub(crate) const RENDER_WAIT: Duration = Duration::from_millis(5);

/// How many commands may be queued to the render thread. A load is a handful;
/// the queue is sized so the handshake, which runs before the thread does,
/// never fills it.
const COMMANDS: usize = 64;

/// How long the main thread waits for room in a full queue before it gives up
/// on a command (a render thread that has stopped taking them).
const SEND_PATIENCE: Duration = Duration::from_secs(5);

/// What a block reads of the plugin and the session, published by the main
/// thread whenever it changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RenderConfig {
    /// The rate and sample format blocks are rendered in.
    pub clock: Clock,
    /// The longest block the plugin was activated for, 0 with none loaded.
    pub max_block: usize,
    /// Whether the plugin refused the rate the host asked for: it then renders
    /// silence and no block reaches it.
    pub rate_refused: bool,
    /// What the plugin consumes, as it reports now.
    pub features: Features,
}

impl RenderConfig {
    /// Nothing loaded.
    pub(crate) fn new() -> Self {
        Self {
            clock: Clock::default(),
            max_block: 0,
            rate_refused: false,
            features: Features::empty(),
        }
    }
}

/// The render thread's end of an active plugin: where its Processor arrives
/// and how a block is rendered with it.
///
/// One per loaded plugin, built with the plugin (it names the format's
/// Processor, which the session's type-erased plugin does not).
pub(crate) trait RenderEnd: Render + Send {
    /// Between blocks: trades with the main thread (takes a Processor lent to
    /// it, returns one that was recalled).
    fn poll(&mut self);
    /// Forgets the plugin's signal history, between blocks. A plugin whose
    /// Processor is not here has nothing to forget.
    fn reset(&mut self);
    /// What the plugin consumes, as it reports now.
    fn set_features(&mut self, features: Features);
    /// Whether the Processor is here: a block rendered now reaches the plugin
    /// (and drains the writes queued to it).
    fn has_processor(&mut self) -> bool;
}

/// [`RenderEnd`] over the format's Processor, in a handoff.
pub(crate) struct End<P: Processor> {
    rt: Rt<P>,
    features: Features,
}

impl<P: Processor> End<P> {
    pub(crate) fn new(rt: Rt<P>, features: Features) -> Self {
        Self { rt, features }
    }
}

impl<P: Processor + 'static> Render for End<P> {
    fn features(&self) -> Features {
        self.features
    }

    fn render(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext<'_>,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        // A Processor that is not here (recalled for a rate change, or
        // refused): the block is silence and the plugin is not touched.
        let Some(processor) = self.rt.held() else {
            out.clear();
            match buffer {
                AudioBufferMut::F32(mut b) => b.clear_outputs(),
                AudioBufferMut::F64(mut b) => b.clear_outputs(),
            }
            return Ok(());
        };
        // The closures are not redundant: the variant's constructor is one
        // function over fixed lifetimes, `through` wants one for any lifetime.
        #[allow(clippy::redundant_closure)]
        match buffer {
            AudioBufferMut::F32(b) => through(processor, b, ctx, out, |b| AudioBufferMut::F32(b)),
            AudioBufferMut::F64(b) => through(processor, b, ctx, out, |b| AudioBufferMut::F64(b)),
        }
    }
}

impl<P: Processor + 'static> RenderEnd for End<P> {
    fn poll(&mut self) {
        let _ = self.rt.poll(Processor::stop);
    }

    fn reset(&mut self) {
        if let Some(processor) = self.rt.held() {
            processor.reset();
        }
    }

    fn set_features(&mut self, features: Features) {
        self.features = features;
    }

    fn has_processor(&mut self) -> bool {
        self.rt.held().is_some()
    }
}

/// What the main thread asks of the render thread.
#[allow(clippy::large_enum_variant)] // sent a few times per session, never per block
pub(crate) enum ToRender {
    /// The shared-memory slab the host set up; the scratch is sized to it.
    Slab(SlabWriter),
    /// The slab is gone (an unload).
    DropSlab,
    /// A plugin loaded: the sample format blocks are rendered in.
    Format(SampleFormat),
    /// A plugin loaded: its parameter output is held in fixed storage, a queue
    /// per parameter of `points` each.
    Params { queues: usize, points: usize },
    /// The parameter list may have grown past what was reserved.
    FitParams(usize),
    /// New figures for the blocks that follow.
    Config(RenderConfig),
    /// The plugin's end: its Processor arrives in it.
    Seat(Box<dyn RenderEnd>),
    /// A write was queued to the Processor before this: the next block that
    /// reaches the plugin applies it (the Processor drains its edit ring at the
    /// head of a block, after this command was taken), and is answered with
    /// [`Back::Ack`]. See [`RenderLink::barrier`].
    Barrier(u64),
    /// Stop rendering with the seat and give it back. The Processor has been
    /// recalled by now.
    Unseat,
}

/// What the render thread hands back to be dropped on the main thread.
// The fields are never read: a value is handed back only to be dropped by the
// thread that may free it.
#[allow(dead_code)]
pub(crate) enum Back {
    /// The seat of a plugin that is gone.
    Seat(Box<dyn RenderEnd>),
    /// The slab.
    Slab(SlabWriter),
    /// A block that reached the plugin after the [`ToRender::Barrier`] of this
    /// number was taken.
    Ack(u64),
}

/// The main thread's end of the render thread's queues.
pub(crate) struct RenderLink {
    to: HeapProd<ToRender>,
    back: HeapCons<Back>,
    /// The number of the last barrier sent, and of the last one acknowledged.
    barrier_sent: u64,
    barrier_acked: u64,
}

impl RenderLink {
    /// Queues `command`, waiting a little for room if the queue is full.
    /// A command the render thread does not take within [`SEND_PATIENCE`] is
    /// dropped and logged: the render thread has stopped.
    pub(crate) fn send(&mut self, command: ToRender) {
        self.reap();
        let mut command = command;
        let giving_up = Instant::now() + SEND_PATIENCE;
        loop {
            match self.to.try_push(command) {
                Ok(()) => return,
                Err(back) => command = back,
            }
            if Instant::now() >= giving_up {
                tracing::error!("plugin-server: the render thread takes no commands");
                return;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// Drops what the render thread handed back, noting the barriers it
    /// acknowledged.
    pub(crate) fn reap(&mut self) {
        while let Some(back) = self.back.try_pop() {
            if let Back::Ack(id) = back {
                self.barrier_acked = self.barrier_acked.max(id);
            }
        }
    }

    /// Asks to be told when a block that reached the plugin has run **after**
    /// the writes queued to its Processor so far; the number to
    /// [`wait_applied`](Self::wait_applied) for.
    ///
    /// The order is the queue's. The writes were pushed (release) before this
    /// command was, the render thread takes the command (acquire) before it
    /// serves the next block, and the Processor drains its edit ring at the head
    /// of that block, so the block sees every write that preceded the barrier.
    /// The acknowledgement comes back through [`Back`] after that block: no
    /// block that began before the command was taken can answer it. No atomic
    /// of its own: the two queues carry the order.
    pub(crate) fn barrier(&mut self) -> u64 {
        self.barrier_sent += 1;
        let id = self.barrier_sent;
        self.send(ToRender::Barrier(id));
        id
    }

    /// Whether the barrier `id` has been acknowledged, waiting up to `within`
    /// for the next block to do it. `false` when no block reached the plugin
    /// in that time (the host is quiet).
    pub(crate) fn wait_applied(&mut self, id: u64, within: Duration) -> bool {
        let giving_up = Instant::now() + within;
        loop {
            self.reap();
            if self.barrier_acked >= id {
                return true;
            }
            if Instant::now() >= giving_up {
                return false;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}

/// The render thread's state: what a block is rendered with.
pub(crate) struct RenderSide {
    commands: HeapCons<ToRender>,
    back: HeapProd<Back>,
    slab: Option<SlabWriter>,
    pipeline: AudioPipeline,
    config: RenderConfig,
    seat: Option<Box<dyn RenderEnd>>,
    /// The barrier the next block that reaches the plugin acknowledges.
    barrier: Option<u64>,
    /// Blocks refused for their length (empty, or past the activated length or
    /// the slab's room), since the session began.
    refused_blocks: u64,
}

/// A session's two halves: the control side the main thread drives, and the
/// render side, which a thread runs ([`RenderThread::spawn`]) once the host's
/// audio connection is there.
pub(crate) fn link() -> (RenderLink, RenderSide) {
    let (to, commands) = HeapRb::<ToRender>::new(COMMANDS).split();
    let (back_tx, back) = HeapRb::<Back>::new(COMMANDS).split();
    let config = RenderConfig::new();
    (
        RenderLink {
            to,
            back,
            barrier_sent: 0,
            barrier_acked: 0,
        },
        RenderSide {
            commands,
            back: back_tx,
            slab: None,
            pipeline: AudioPipeline::new(config.clock.format),
            config,
            seat: None,
            barrier: None,
            refused_blocks: 0,
        },
    )
}

impl RenderSide {
    /// Applies what the main thread queued.
    pub(crate) fn drain(&mut self) {
        while let Some(command) = self.commands.try_pop() {
            match command {
                ToRender::Slab(slab) => {
                    // The scratch is sized here, between blocks, to the most a
                    // block may carry; `handle_process` refuses any longer.
                    let layout = slab.layout_ref();
                    self.pipeline.reserve(
                        layout.input_channels().max(layout.output_channels()),
                        layout.samples_per_channel,
                    );
                    if let Some(old) = self.slab.replace(slab) {
                        self.give_back(Back::Slab(old));
                    }
                }
                ToRender::DropSlab => {
                    if let Some(old) = self.slab.take() {
                        self.give_back(Back::Slab(old));
                    }
                }
                ToRender::Format(format) => self.pipeline.set_format(format),
                ToRender::Params { queues, points } => {
                    self.pipeline.reserve_params(queues, points);
                }
                ToRender::FitParams(queues) => self.pipeline.fit_params(queues),
                ToRender::Config(config) => {
                    self.config = config;
                    if let Some(seat) = self.seat.as_mut() {
                        seat.set_features(config.features);
                    }
                }
                ToRender::Seat(mut seat) => {
                    seat.set_features(self.config.features);
                    if let Some(old) = self.seat.replace(seat) {
                        self.give_back(Back::Seat(old));
                    }
                }
                ToRender::Barrier(id) => self.barrier = Some(id),
                ToRender::Unseat => {
                    if let Some(old) = self.seat.take() {
                        self.give_back(Back::Seat(old));
                    }
                }
            }
        }
    }

    /// Hands something back to the main thread. A full queue drops it here,
    /// which only an abandoned session reaches: the main thread reaps on every
    /// command it sends.
    fn give_back(&mut self, back: Back) {
        if self.back.try_push(back).is_err() {
            tracing::error!("plugin-server: the main thread reaps nothing");
        }
    }

    /// Between blocks: trades the Processor with the main thread.
    pub(crate) fn poll_seat(&mut self) {
        if let Some(seat) = self.seat.as_mut() {
            seat.poll();
        }
    }

    /// One pass of the thread: applies the commands, trades the Processor,
    /// waits up to `wait` for a request on `io` and serves it. With no `io`
    /// (the host's audio connection has gone) it only waits.
    ///
    /// # Errors
    ///
    /// The transport fails (the host went away, or sent a frame that does not
    /// decode), or the host sent a message that is not the audio connection's.
    pub(crate) fn step(
        &mut self,
        io: Option<&mut Box<dyn Transport>>,
        wait: Duration,
    ) -> BridgeResult<()> {
        self.drain();
        self.poll_seat();
        let Some(io) = io else {
            std::thread::sleep(wait);
            return Ok(());
        };
        if !io.wait_readable(wait)? {
            return Ok(());
        }
        let msg = io.recv()?;
        // A command pushed before the answer that let the host send this
        // request is in effect for it.
        self.drain();
        self.poll_seat();
        if let Some(reply) = self.handle(msg)? {
            io.send(&reply)?;
        }
        Ok(())
    }

    /// Serves one message of the audio connection: `ProcessAudio` (answered)
    /// and `Reset` (not).
    ///
    /// # Errors
    ///
    /// The message is not one the audio connection carries: a peer that
    /// confuses its two connections.
    pub(crate) fn handle(&mut self, msg: HostMessage) -> BridgeResult<Option<BridgeMessage>> {
        match msg {
            HostMessage::ProcessAudio(data) => {
                let midi = &data.midi_events;
                let extras = ProcessExtras {
                    param_changes: &data.param_changes,
                    note_expression: &data.note_expression,
                    chords: &data.chords,
                    scales: &data.scales,
                    expr_texts: &data.expr_texts,
                    expr_ints: &data.expr_ints,
                    transport: &data.transport,
                    transport_changes: &data.transport_changes,
                };
                self.handle_process(AudioBlock {
                    seq: data.seq,
                    num_samples: data.num_samples,
                    midi,
                    extras: Some(extras),
                })
                .map(Some)
            }
            // The plugin forgets its signal history,
            // in order with the blocks on this connection, so a reset is never
            // reordered against a block. The pipeline is left alone, and the
            // host depends on that: clearing it on a seek is entirely the
            // host's job (it drops its in-flight block and never rewinds its
            // sequence, so a pre-seek block published late is structurally
            // unmatchable), and anything done to it here would be a second
            // mechanism that could disagree with the first.
            HostMessage::Reset => {
                if let Some(seat) = self.seat.as_mut() {
                    seat.reset();
                }
                Ok(None)
            }
            other => Err(BridgeError::ProtocolError(format!(
                "{other:?} is not carried on the audio connection"
            ))),
        }
    }

    /// Whether a block of `frames` may reach the plugin: not empty, within the
    /// length it was activated for, and within the slab's per-channel room
    /// (the scratch's size). The length is the peer's to choose, so it is
    /// checked here, once, for every format: unchecked, a long one sized the
    /// scratch to it on the audio thread, and the formats disagreed past the
    /// activated length (CLAP refused it, VST2 split it, VST3 passed it to the
    /// plugin). A refused block is counted and reads back as silence.
    fn block_fits(&mut self, frames: usize) -> bool {
        let room = self
            .slab
            .as_ref()
            .map_or(0, |slab| slab.layout_ref().samples_per_channel);
        let fits = frames > 0 && frames <= self.config.max_block && frames <= room;
        if !fits {
            self.refused_blocks += 1;
        }
        fits
    }

    fn handle_process(&mut self, block: AudioBlock<'_>) -> BridgeResult<BridgeMessage> {
        // Read before `block` is moved into the pipeline: every reply below
        // echoes it.
        let seq = block.seq;
        let silent = || BridgeMessage::AudioProcessed {
            latency_us: 0,
            seq,
            midi_out: MidiEventVec::new(),
            status: BlockStatus::Continue,
        };

        // NOTE: both early returns below reply WITHOUT publishing to the slab,
        // and that is deliberate. The host's read is gated on the output
        // sequence, so an unpublished block reads back as silence; there is no
        // separate "this block failed" signal to send, and none is wanted.
        // Publishing something here to "answer properly" would be actively
        // wrong: it would hand the host a slot full of whatever the previous
        // block left, marked valid.
        if self.seat.is_none() {
            return Ok(BridgeMessage::Error {
                message: "No plugin loaded".to_string(),
            });
        }
        // A plugin that refused the rate renders silence: no block reaches it
        // and nothing is published (the host reads an unpublished block as
        // silence). Its MIDI is dropped; the host cut any open SysEx when it
        // re-prepared. A block of a length the plugin was not activated for,
        // or the slab cannot hold, is refused the same way (`block_fits`).
        if self.config.rate_refused || !self.block_fits(block.num_samples) {
            return Ok(silent());
        }
        // Neither the no-slab return just below nor a block the plugin's host
        // refuses before staging its MIDI (CLAP's `BlockTooLarge`) can drop
        // MIDI past a SysEx open in the reassembler, leaving a gap, from
        // tutti's own host: it sets the slab up before its bridge sends any
        // block, and its chunk never exceeds `max_buffer_size`, the
        // `block_size` the plugin was loaded with, which nothing here changes.
        // A peer that broke either would splice.
        let Self {
            slab,
            pipeline,
            config,
            seat,
            barrier,
            back,
            ..
        } = self;
        let (Some(seat), Some(slab)) = (seat.as_mut(), slab.as_mut()) else {
            // The process path only ever ran under the full `plugin + slab`
            // pair. Reply so the host's queue stays in step; no publish, so it
            // hears silence.
            return Ok(silent());
        };

        let reaches = seat.has_processor();
        let output = pipeline.process(seat.as_mut(), slab, &config.clock, block)?;
        if reaches && let Some(id) = barrier.take() {
            // This block ran the Processor after the barrier was taken.
            if back.try_push(Back::Ack(id)).is_err() {
                tracing::error!("plugin-server: the main thread reaps nothing");
            }
        }
        Ok(BridgeMessage::AudioProcessed {
            latency_us: output.latency_us,
            seq,
            // Borrowed from the pipeline's reusable buffer rather than moved
            // out of the result: moving it would take the buffer's heap
            // capacity with it and put the reallocation back on the audio
            // thread. See [`AudioOutput`](crate::audio_pipeline::AudioOutput).
            midi_out: encode_midi_out(pipeline.midi_out(), pipeline.midi_dropped()),
            status: pipeline.status(),
        })
    }

    /// The pipeline, for the tests that read what it counted.
    #[cfg(all(test, any(feature = "clap", feature = "vst3")))]
    pub(crate) fn pipeline(&self) -> &AudioPipeline {
        &self.pipeline
    }

    /// The pipeline, mutably, for the test that undersizes it.
    #[cfg(all(test, feature = "vst3"))]
    pub(crate) fn pipeline_mut(&mut self) -> &mut AudioPipeline {
        &mut self.pipeline
    }

    /// Blocks refused for their length so far.
    #[cfg(all(test, feature = "clap"))]
    pub(crate) fn refused_blocks(&self) -> u64 {
        self.refused_blocks
    }

    /// Whether a slab is attached.
    #[cfg(test)]
    pub(crate) fn has_slab(&self) -> bool {
        self.slab.is_some()
    }
}

/// A block's plugin MIDI-out for the wire. Already capped at
/// `MIDI_STACK_CAPACITY` (the pipeline's list cannot hold more), so the reply
/// carries at most that many; `dropped` is what the cap cost this block, from
/// a pathological plugin, and is logged.
fn encode_midi_out(events: &MidiEventVec, dropped: usize) -> MidiEventVec {
    if dropped > 0 {
        tracing::warn!(
            "plugin emitted {} MIDI events this block; dropped {dropped} past the cap",
            events.len() + dropped,
        );
    }
    events.clone()
}

/// The thread that renders: owns the [`RenderSide`] and the host's audio
/// connection until it is stopped.
pub(crate) struct RenderThread {
    handle: Option<JoinHandle<RenderSide>>,
    stop: Arc<AtomicBool>,
    /// Why the audio connection ended, if it did: the thread then only trades
    /// the Processor, until it is stopped.
    ended: Arc<OnceLock<BridgeError>>,
}

impl RenderThread {
    /// Starts the thread over `audio`, the host's audio connection.
    ///
    /// The thread asks the OS for the highest priority it grants
    /// ([`crate::server::request_audio_priority`]): it is the one that renders.
    pub(crate) fn spawn(side: RenderSide, audio: Box<dyn Transport>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(OnceLock::new());
        let handle = std::thread::Builder::new()
            .name("plugin-render".to_string())
            .spawn({
                let (stop, ended) = (Arc::clone(&stop), Arc::clone(&ended));
                move || {
                    crate::server::request_audio_priority("the render thread");
                    run(side, audio, &stop, &ended)
                }
            })?;
        Ok(Self {
            handle: Some(handle),
            stop,
            ended,
        })
    }

    /// Why the audio connection ended, once it has.
    pub(crate) fn ended(&self) -> Option<&BridgeError> {
        self.ended.get()
    }

    /// Stops the thread and takes its state back.
    ///
    /// # Panics
    ///
    /// If the render thread panicked (a plugin that panics is caught at the
    /// lifecycle; anything else here is a bug).
    pub(crate) fn stop(mut self) -> RenderSide {
        self.stop.store(true, Ordering::Release);
        self.handle
            .take()
            .expect("stopped once")
            .join()
            .expect("the render thread does not panic")
    }
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The thread's loop: serves the audio connection, and after it ends goes on
/// trading the Processor until `stop`.
fn run(
    mut side: RenderSide,
    audio: Box<dyn Transport>,
    stop: &AtomicBool,
    ended: &OnceLock<BridgeError>,
) -> RenderSide {
    let mut audio = Some(audio);
    while !stop.load(Ordering::Acquire) {
        if let Err(error) = side.step(audio.as_mut(), RENDER_WAIT) {
            // The connection is gone; the Processor may still be recalled, so
            // the thread keeps trading it until the main thread has it back.
            audio = None;
            let _ = ended.set(error);
        }
    }
    side
}
