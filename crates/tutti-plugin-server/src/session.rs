//! Pure message dispatch — no I/O, no async, no sockets.
//!
//! [`Session`] owns the plugin + shared memory + audio pipeline. [`Session::handle`] translates one [`HostMessage`] into state
//! changes and returns a [`Reaction`] describing what the transport
//! loop should do next. Because it has no `async` and no `Transport`
//! dependency, it's unit-testable without tokio or sockets.

use crate::audio_pipeline::{AudioBlock, AudioPipeline, Clock, ProcessExtras};
use crate::plugin::{AsyncEvent, Plugin};
use tutti_plugin::server::state_chunk::{self, ChunkError, Reassembler};
use tutti_plugin::server::{
    AudioSlab, BridgeMessage, HostMessage, MAX_STATE_BYTES, MidiEventVec, Normalized,
    ParameterQueue, SampleFormat, SlabWriter, StagedState,
};
use tutti_plugin_types::MainThread;
use tutti_plugin_types::{BridgeError, BridgeResult};

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

/// What [`Session::handle`] asks the transport loop to do.
// One value per message, moved once to the transport loop; a `BridgeMessage`
// carries a block's inline MIDI list.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum Reaction {
    /// Send this reply back to the host.
    Reply(BridgeMessage),
    /// Send these replies back, in order, as one logical answer.
    ///
    /// Exists for plugin state, which does not fit a single frame and so is
    /// answered as a `StateChunk` sequence. Modelled as one `Reaction` rather
    /// than letting the handler write to the transport itself, so dispatch
    /// stays a pure function of the request — the property the whole `Reaction`
    /// enum exists to preserve, and what makes `handle` testable without a
    /// socket.
    ReplyMany(Vec<BridgeMessage>),
    /// No reply; keep looping.
    None,
    /// The host asked for shutdown; leave the loop.
    Shutdown,
}

impl From<BridgeMessage> for Reaction {
    fn from(m: BridgeMessage) -> Self {
        Self::Reply(m)
    }
}

#[cfg(test)]
impl Reaction {
    fn into_reply(self) -> BridgeMessage {
        match self {
            Reaction::Reply(m) => m,
            // A single-frame `ReplyMany` is the same answer wearing the
            // sequence shape state now uses; unwrapping it here keeps every
            // one-reply assertion in this file reading the same way.
            Reaction::ReplyMany(mut ms) if ms.len() == 1 => ms.remove(0),
            other => panic!("expected one reply, got {other:?}"),
        }
    }

    /// Every frame of a multi-frame answer, in order.
    #[cfg(feature = "clap")]
    fn into_replies(self) -> Vec<BridgeMessage> {
        match self {
            Reaction::Reply(m) => vec![m],
            Reaction::ReplyMany(ms) => ms,
            other => panic!("expected replies, got {other:?}"),
        }
    }
}

/// All state one hosted plugin needs, and the dispatch over it.
///
/// Holds no transport, so a test drives it by calling [`Session::handle`]
/// directly with a [`HostMessage`].
pub(crate) struct Session {
    /// The hosted plugin, or `None` before a load and after an unload. Every
    /// handler that needs one refuses explicitly rather than going silent.
    pub(crate) plugin: Option<Plugin>,
    /// The shared-memory audio slab, established during the handshake phase.
    /// `None` until then, and cleared on unload alongside `plugin`.
    pub(crate) shm: Option<SlabWriter>,
    /// Per-block scratch and the process driver. Outlives individual plugins so
    /// its buffers survive a reload without re-allocating.
    pub(crate) pipeline: AudioPipeline,
    /// Sample rate + negotiated format. Written on load and on an explicit
    /// `SetSampleRate`; read by every block.
    pub(crate) clock: Clock,
    /// Why the plugin is not running at the rate the host asked for, while it
    /// is not. A refused plugin renders silence (no block reaches it) and the
    /// next `SetSampleRate` retries it.
    pub(crate) rate_refused: Option<String>,
    /// The longest block the loaded plugin was activated for (`LoadPlugin`'s
    /// `block_size`), 0 with none loaded. A block past it, or past the slab's
    /// per-channel room, is refused before anything is sized for it.
    pub(crate) max_block: usize,
    /// Blocks refused for their length (empty, or past `max_block` or the
    /// slab's room), since the session began.
    pub(crate) refused_blocks: u64,
    /// Partially-received `LoadStateChunk` sequence, or `None` between loads.
    ///
    /// On the session rather than threaded through `handle` because a chunk
    /// sequence spans several messages and `handle` sees one at a time. Cleared
    /// on completion *and* on any failure, so a malformed sequence cannot leave
    /// bytes behind that would corrupt the next load.
    pub(crate) state_in: Option<Reassembler>,
    /// A load sequence refused part-way, with why: its remaining chunks are
    /// taken without reply and the refusal is the answer to its `last`.
    pub(crate) state_in_refused: Option<String>,
    /// A state staged for the next load (`StageStateChunk`), being received.
    pub(crate) stage_in: Option<Reassembler>,
    /// A state received and waiting for the next load, which restores it into
    /// the plugin before activating it.
    pub(crate) staged: Option<Vec<u8>>,
}

impl Session {
    /// An empty session: no plugin, no slab, and a default
    /// [`Clock`] that the first load overwrites.
    pub(crate) fn new() -> Self {
        let clock = Clock::default();
        Self {
            plugin: None,
            shm: None,
            pipeline: AudioPipeline::new(clock.format),
            clock,
            rate_refused: None,
            max_block: 0,
            refused_blocks: 0,
            state_in: None,
            state_in_refused: None,
            stage_in: None,
            staged: None,
        }
    }

    /// Translate `msg` into state changes + a [`Reaction`].
    ///
    /// **A request that expects a frame always gets one**, including on the
    /// refusal paths: the host blocks waiting for the reply, so answering
    /// [`Reaction::None`] where it expects one hangs it until its timeout.
    ///
    /// # Errors
    ///
    /// Returns an error only when a failure cannot be expressed as a reply the
    /// host can act on. Plugin-level failures — a load that fails, a request
    /// with no plugin loaded — come back as an error-carrying `Reply` instead.
    pub(crate) fn handle(&mut self, main: &MainThread, msg: HostMessage) -> BridgeResult<Reaction> {
        self.handle_with(main, msg, &mut |_| {})
    }

    /// [`handle`](Self::handle), with `interim` for a message that cannot wait
    /// for the reply: the host is told a load has begun restoring its state
    /// ([`BridgeMessage::RestoringState`]) before the restore, so what it waits
    /// for changes with it. Dispatch stays a function of the request; `interim`
    /// is a sink, not a source.
    pub(crate) fn handle_with(
        &mut self,
        main: &MainThread,
        msg: HostMessage,
        interim: &mut dyn FnMut(BridgeMessage),
    ) -> BridgeResult<Reaction> {
        use HostMessage as M;
        match msg {
            M::LoadPlugin {
                path,
                sample_rate,
                block_size,
                preferred_format,
                shm_name: _,
                state,
            } => self.handle_load(
                main,
                path,
                sample_rate.get(),
                block_size,
                preferred_format,
                state,
                interim,
            ),
            M::UnloadPlugin => {
                self.discard_staged();
                self.plugin = None;
                self.rate_refused = None;
                self.shm = None;
                Ok(Reaction::None)
            }

            M::ProcessAudio(data) => {
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
            }

            M::SetParameter { param_id, value } => {
                if let Some(plugin) = self.plugin.as_mut() {
                    // The wire's decode refuses an out-of-range or NaN value
                    // (`Normalized::new`), so one from the peer fails the frame
                    // and never reaches a live plugin.
                    plugin.instance_mut().set_parameter(main, param_id, value);
                }
                Ok(Reaction::None)
            }
            M::SetAutomationState { mode } => {
                if let Some(plugin) = self.plugin.as_mut() {
                    plugin.instance_mut().set_automation_state(main, mode);
                }
                Ok(Reaction::None)
            }
            M::GetParameter { param_id } => {
                let value = self
                    .plugin
                    .as_mut()
                    .map(|p| Normalized::clamped(p.instance_mut().get_parameter(main, param_id)));
                Ok(BridgeMessage::ParameterValue { value }.into())
            }
            M::GetParameterList => {
                let parameters = self
                    .plugin
                    .as_ref()
                    .map(|p| p.instance().get_parameter_list(main))
                    .unwrap_or_default();
                Ok(BridgeMessage::ParameterList { parameters }.into())
            }
            M::GetPresetList => {
                // `unwrap_or_default` — an empty list, matching
                // `GetParameterList` above. A request arriving before a plugin
                // is loaded is a host sequencing error, not something to report
                // as a preset-less plugin, and the same shape already covers it
                // for parameters.
                let presets = self
                    .plugin
                    .as_mut()
                    .map(|p| p.instance_mut().get_presets(main))
                    .unwrap_or_default();
                Ok(BridgeMessage::PresetList { presets }.into())
            }
            M::LoadPreset { id } => {
                let ok = self
                    .plugin
                    .as_mut()
                    .is_some_and(|p| p.instance_mut().load_preset(main, &id));
                Ok(BridgeMessage::PresetLoaded { ok }.into())
            }
            M::GetCurrentPreset => {
                let id = self
                    .plugin
                    .as_mut()
                    .and_then(|p| p.instance_mut().get_current_preset(main));
                Ok(BridgeMessage::CurrentPreset { id }.into())
            }
            M::GetParameterText { param_id, value } => {
                let text = self
                    .plugin
                    .as_ref()
                    .and_then(|p| p.instance().parameter_text(main, param_id, value));
                Ok(BridgeMessage::ParameterText { text }.into())
            }
            M::GetParameterValueFromText { param_id, text } => {
                // Reads as a query, but is not one on every format: VST2's
                // `effString2Parameter` writes the parsed value into the plugin
                // as it reads it. The trait takes `&self` because the plugin
                // handle is interior-mutable at the FFI edge, so this stays on
                // the `as_ref` path with the other queries.
                let value = self.plugin.as_ref().and_then(|p| {
                    p.instance()
                        .parameter_value_from_text(main, param_id, &text)
                });
                Ok(BridgeMessage::ParameterValueFromText { value }.into())
            }
            M::GetParameterInfo { param_id } => {
                let info = self.plugin.as_ref().and_then(|p| {
                    p.instance()
                        .get_parameter_list(main)
                        .into_iter()
                        .find(|info| info.id == param_id)
                });
                Ok(BridgeMessage::ParameterInfoResponse { info }.into())
            }

            M::SetSampleRate { rate } => {
                // The wire's rate is not range-checked on decode: refuse one no
                // clock can run at before the clock or the plugin sees it. The
                // plugin keeps the rate it had, and the host is told why.
                if !(rate.get().is_finite() && rate.get() > 0.0) {
                    return Ok(BridgeMessage::SampleRateSet {
                        rate,
                        error: Some(format!("{} is not a sample rate", rate.get())),
                    }
                    .into());
                }
                // The clock follows the rate the host runs at, so a plugin
                // that takes a later rate is on the right one. A refusal
                // leaves it silent (`handle_process` publishes nothing) and
                // the host is told, because a rolled-back plugin would
                // otherwise render at its old rate against the host's blocks.
                self.clock.sample_rate = rate.get();
                let error = match self.plugin.as_mut() {
                    Some(plugin) => match plugin.instance_mut().set_sample_rate(main, rate.get()) {
                        Ok(()) => None,
                        Err(e) => {
                            tracing::warn!(requested = %rate, "sample rate not applied: {e}");
                            Some(e.to_string())
                        }
                    },
                    None => None,
                };
                self.rate_refused.clone_from(&error);
                Ok(BridgeMessage::SampleRateSet { rate, error }.into())
            }
            M::SetRenderMode { mode } => {
                // The plugin's answer is deliberately not returned to the host:
                // whether a plugin honours the mode is a load-time capability
                // (`Features::RENDER_MODE`), not a per-call outcome, and the
                // host already has it. Logged rather than dropped so a refusal
                // is greppable when a bounce sounds like the live path.
                if let Some(plugin) = self.plugin.as_mut()
                    && !plugin.instance_mut().set_render_mode(main, mode)
                {
                    tracing::debug!(?mode, "plugin did not accept the render mode");
                }
                Ok(Reaction::None)
            }
            // The plugin forgets its signal history (design 016 §6.2), in order
            // with the `ProcessAudio` blocks on this loop, so a reset is never
            // reordered against a block. The pipeline is left alone, and the
            // host depends on that: clearing it on a seek is entirely the
            // host's job (it drops its in-flight block and never rewinds its
            // sequence, so a pre-seek block published late is structurally
            // unmatchable), and anything done to it here would be a second
            // mechanism that could disagree with the first.
            M::Reset => {
                if let Some(plugin) = self.plugin.as_mut()
                    && let Err(e) = plugin.instance_mut().reset()
                {
                    tracing::warn!("the plugin did not reset: {e}");
                }
                Ok(Reaction::None)
            }

            M::SaveState => Ok(self.handle_save_state(main)),
            M::LoadStateChunk { seq, last, bytes } => {
                Ok(self.handle_load_state_chunk(main, seq, last, &bytes))
            }
            M::StageStateChunk { seq, last, bytes } => {
                Ok(self.handle_stage_state_chunk(seq, last, &bytes))
            }

            M::SetupSharedMemory { shm_name, layout } => {
                let slab = AudioSlab::open(shm_name, layout)?;
                // The scratch is sized here, off the audio thread, to the most a
                // block may carry; `handle_process` refuses any block longer.
                let layout = slab.layout_ref();
                self.pipeline.reserve(
                    layout.input_channels().max(layout.output_channels()),
                    layout.samples_per_channel,
                );
                self.shm = Some(slab);
                Ok(BridgeMessage::SharedMemoryReady.into())
            }
            M::Shutdown => Ok(Reaction::Shutdown),
        }
    }

    /// Drain async events from the loaded plugin, mapping each into a
    /// `BridgeMessage` the transport can push back. Called from the
    /// server's audio phase after every message dispatch.
    pub(crate) fn drain_async_events(&mut self, main: &MainThread) -> Vec<BridgeMessage> {
        let Some(plugin) = self.plugin.as_mut() else {
            return Vec::new();
        };
        let events = plugin.poll_async_events(main);
        // A parameter list that changed may have grown past the output
        // reserved at load. This runs on the audio-phase thread between
        // blocks, so the count is a count query, not the descriptor list.
        // What it helps: a VST3 `Reloaded` (the reload builds a new instance,
        // whose output lists are sized at its activation) and an AU list
        // change. A VST3 `kParamTitlesChanged` without a reload keeps the
        // instance's activation-time output and emitted queue counts, so the
        // instance drops (and counts) points past them before they reach
        // this output. A CLAP plugin's `params.rescan` that changes the
        // list reaches this as `ParamTitlesChanged` (the instance re-read it).
        if events
            .iter()
            .any(|e| matches!(e, AsyncEvent::ParamTitlesChanged | AsyncEvent::Reloaded))
        {
            let params = plugin.instance().parameter_count(main);
            self.pipeline.fit_params(params);
        }
        events
            .into_iter()
            .map(|e| match e {
                AsyncEvent::ParameterChanged { index, value } => {
                    BridgeMessage::ParameterChanged { index, value }
                }
                AsyncEvent::LatencyChanged { samples } => BridgeMessage::LatencyChanged { samples },
                AsyncEvent::TailChanged { tail } => BridgeMessage::TailChanged { tail },
                AsyncEvent::ParamValuesChanged => BridgeMessage::PluginParamValuesChanged,
                AsyncEvent::ParamTitlesChanged => BridgeMessage::PluginParamTitlesChanged,
                AsyncEvent::IoChanged => BridgeMessage::PluginIoChanged,
                AsyncEvent::Reloaded => BridgeMessage::PluginReloaded,
            })
            .collect()
    }

    /// Forgets a state staged for a load that has not come: the stage belongs to
    /// the one load that declares it.
    fn discard_staged(&mut self) {
        self.stage_in = None;
        self.staged = None;
    }

    fn handle_load(
        &mut self,
        main: &MainThread,
        path: std::path::PathBuf,
        sample_rate: f64,
        block_size: usize,
        preferred_format: SampleFormat,
        declared: Option<StagedState>,
        interim: &mut dyn FnMut(BridgeMessage),
    ) -> BridgeResult<Reaction> {
        // What a host staged for this load, restored before the activation: only
        // when the load declares exactly it. Staged here or declared here and not
        // the other, or of another length, is a host that is out of step with
        // this server (or a stage left by another load), and restoring it would
        // hand a plugin a state nobody meant for it.
        let staged = self.staged.take();
        self.stage_in = None;
        // A load replaces what the server held, whether or not it succeeds:
        // a refused one must not leave the old plugin answering a host that
        // was told the load failed (and holding its instance while the next
        // one loads).
        self.plugin = None;
        self.rate_refused = None;
        match (&declared, &staged) {
            (None, None) => {}
            (Some(d), Some(bytes)) if d.len == bytes.len() as u64 => {}
            (declared, staged) => {
                return Ok(BridgeMessage::Error {
                    message: format!(
                        "the load declares a state of {:?} bytes and the server holds {:?}",
                        declared.map(|d| d.len),
                        staged.as_ref().map(Vec::len),
                    ),
                }
                .into());
            }
        }
        let mut restoring = || interim(BridgeMessage::RestoringState);
        let (plugin, descriptor, loaded, negotiated, state_error) = match Plugin::load_with_state(
            main,
            &path,
            sample_rate,
            block_size,
            preferred_format,
            staged.as_deref(),
            &mut restoring,
        ) {
            Ok(loaded) => loaded,
            Err(e) => return Ok(load_failure(&e).into()),
        };
        self.clock = Clock {
            sample_rate,
            format: negotiated,
        };
        self.pipeline.set_format(negotiated);
        self.max_block = block_size;
        // The plugin's own parameter output is held in fixed storage
        // reserved here, off the audio thread: a queue per parameter, each
        // within a block's chunk room (`ParameterChanges::reserved`).
        self.pipeline.reserve_params(
            plugin.instance().parameter_count(main),
            ParameterQueue::chunk_room(block_size),
        );
        self.plugin = Some(plugin);
        self.rate_refused = None;
        Ok(BridgeMessage::PluginLoaded {
            descriptor: Box::new(descriptor),
            loaded,
            negotiated_format: negotiated,
            state_error,
        }
        .into())
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
            .shm
            .as_ref()
            .map_or(0, |slab| slab.layout_ref().samples_per_channel);
        let fits = frames > 0 && frames <= self.max_block && frames <= room;
        if !fits {
            self.refused_blocks += 1;
        }
        fits
    }

    fn handle_process(&mut self, block: AudioBlock<'_>) -> BridgeResult<Reaction> {
        // Read before `block` is moved into the pipeline: every reply below
        // echoes it.
        let seq = block.seq;

        // NOTE — both early returns below reply WITHOUT publishing to the slab,
        // and that is deliberate. The host's read is gated on the output
        // sequence, so an unpublished block reads back as silence; there is no
        // separate "this block failed" signal to send, and none is wanted.
        // Publishing something here to "answer properly" would be actively
        // wrong: it would hand the host a slot full of whatever the previous
        // block left, marked valid.
        if self.plugin.is_none() {
            return Ok(BridgeMessage::Error {
                message: "No plugin loaded".to_string(),
            }
            .into());
        }
        // A plugin that refused the rate renders silence: no block reaches it
        // and nothing is published (the host reads an unpublished block as
        // silence). Its MIDI is dropped; the host cut any open SysEx when it
        // re-prepared. A block of a length the plugin was not activated for,
        // or the slab cannot hold, is refused the same way (`block_fits`).
        if self.rate_refused.is_some() || !self.block_fits(block.num_samples) {
            return Ok(BridgeMessage::AudioProcessed {
                latency_us: 0,
                seq,
                midi_out: MidiEventVec::new(),
            }
            .into());
        }
        // Neither the no-slab return just below nor a block the plugin's host
        // refuses before staging its MIDI (CLAP's `BlockTooLarge`) can drop
        // MIDI past a SysEx open in the reassembler, leaving a gap, from
        // tutti's own host:
        // it sets the slab up before its bridge sends any block, and its
        // chunk never exceeds `max_buffer_size`, the `block_size` the plugin
        // was loaded with, which nothing here changes. A peer that broke
        // either would splice.
        // Borrowed here, after `block_fits` needed `&mut self`; the check at
        // the top already answered a session with no plugin.
        let Some(plugin) = self.plugin.as_mut() else {
            return Ok(BridgeMessage::Error {
                message: "No plugin loaded".to_string(),
            }
            .into());
        };
        let Some(shm) = self.shm.as_mut() else {
            // The process path only ever ran under the full `plugin + shm` pair.
            // Reply so the host's queue stays in step; no publish, so it hears
            // silence.
            return Ok(BridgeMessage::AudioProcessed {
                latency_us: 0,
                seq,
                midi_out: MidiEventVec::new(),
            }
            .into());
        };

        let output = self
            .pipeline
            .process(plugin.instance_mut(), shm, &self.clock, block)?;
        Ok(BridgeMessage::AudioProcessed {
            latency_us: output.latency_us,
            seq,
            // Borrowed from the pipeline's reusable buffer rather than moved
            // out of the result: moving it would take the buffer's heap
            // capacity with it and put the reallocation back on the audio
            // thread. See [`AudioOutput`](crate::audio_pipeline::AudioOutput).
            midi_out: encode_midi_out(self.pipeline.midi_out(), self.pipeline.midi_dropped()),
        }
        .into())
    }

    /// Answer `SaveState` as a `StateChunk` sequence.
    ///
    /// Chunked because a plugin's state is the one payload whose size the
    /// plugin picks, and a sample-embedding instrument's exceeds what a single
    /// frame carries. The empty cases still emit one terminating chunk — a host
    /// waits for `last`, so answering nothing would hang it rather than tell it
    /// there is no state.
    fn handle_save_state(&mut self, main: &MainThread) -> Reaction {
        let data = match self.plugin.as_mut() {
            Some(p) => match p.instance_mut().get_state(main) {
                Ok(data) => data,
                // A plugin that fails to serialise is not a transport problem:
                // one `Error` frame, and the host reports it as a rejection.
                Err(e) => {
                    return BridgeMessage::Error {
                        message: format!("Failed to save state: {e}"),
                    }
                    .into();
                }
            },
            None => Vec::new(),
        };
        // Refuse an over-limit state here rather than streaming a gigabyte the
        // host is only going to reject on reassembly.
        if data.len() > MAX_STATE_BYTES {
            return BridgeMessage::Error {
                message: format!(
                    "plugin state is {} bytes, over the {MAX_STATE_BYTES}-byte limit",
                    data.len()
                ),
            }
            .into();
        }
        Reaction::ReplyMany(
            state_chunk::split(&data)
                .map(|(seq, last, bytes)| BridgeMessage::StateChunk {
                    seq,
                    last,
                    bytes: bytes.to_vec(),
                })
                .collect(),
        )
    }

    /// Accumulate one `StageStateChunk`, keeping the state for the next load.
    ///
    /// Answers, as `LoadStateChunk` does, only on the final chunk or a failure;
    /// `error: None` means staged. Nothing is applied until `LoadPlugin`.
    fn handle_stage_state_chunk(&mut self, seq: u32, last: bool, bytes: &[u8]) -> Reaction {
        let acc = self.stage_in.get_or_insert_with(Reassembler::default);
        match acc.push(seq, last, bytes) {
            Ok(false) => Reaction::None,
            Ok(true) => {
                let data = self.stage_in.take().expect("just inserted").into_inner();
                self.staged = Some(data);
                BridgeMessage::StateLoaded { error: None }.into()
            }
            Err(e) => {
                self.stage_in = None;
                self.staged = None;
                let error = chunk_refusal(e);
                BridgeMessage::StateLoaded { error: Some(error) }.into()
            }
        }
    }

    /// Always answers, so the host's waiting caller is never left guessing.
    ///
    /// Every path produces a `StateLoaded`, including "no plugin loaded",
    /// which is a refusal rather than a silence.
    /// Accumulate one `LoadStateChunk`, applying the state once `last` lands.
    ///
    /// Answers **only** on the final chunk (or on a failure): a `StateLoaded`
    /// per chunk would leave the host's single waiting caller matched against
    /// the first of many replies, with the rest arriving as unsolicited traffic
    /// that desynchronises the next request's reply.
    ///
    /// A malformed or over-limit sequence resets the accumulator and answers
    /// with the reason. Resetting matters: leaving the partial buffer in place
    /// would make the *next*, well-formed load fail too, turning one bad
    /// project file into a permanently broken session.
    fn handle_load_state_chunk(
        &mut self,
        main: &MainThread,
        seq: u32,
        last: bool,
        bytes: &[u8],
    ) -> Reaction {
        // **One answer per sequence, on its `last` chunk**, whatever went
        // wrong before it: the host reads one `StateLoaded` per load, and an
        // early answer left every later chunk of the sequence to fail again
        // (as a new sequence at a `seq` past 0) and answer again, each read
        // as the reply to a later request. A refusal waits for `last`; a chunk
        // 0 starts a new sequence.
        if seq == 0 {
            self.state_in = None;
            self.state_in_refused = None;
        }
        if let Some(error) = &self.state_in_refused {
            if !last {
                return Reaction::None;
            }
            let error = error.clone();
            self.state_in_refused = None;
            return BridgeMessage::StateLoaded { error: Some(error) }.into();
        }
        let acc = self.state_in.get_or_insert_with(Reassembler::default);
        let complete = match acc.push(seq, last, bytes) {
            Ok(done) => done,
            Err(e) => {
                self.state_in = None;
                let error = chunk_refusal(e);
                if last {
                    return BridgeMessage::StateLoaded { error: Some(error) }.into();
                }
                self.state_in_refused = Some(error);
                return Reaction::None;
            }
        };
        if !complete {
            return Reaction::None;
        }
        let data = self.state_in.take().expect("just inserted").into_inner();

        let Some(plugin) = self.plugin.as_mut() else {
            return BridgeMessage::StateLoaded {
                error: Some("no plugin is loaded".to_string()),
            }
            .into();
        };
        let error = match plugin.instance_mut().set_state(main, &data) {
            Ok(()) => None,
            Err(e) => Some(format!("Failed to load state: {e}")),
        };
        BridgeMessage::StateLoaded { error }.into()
    }
}

/// Why a state chunk sequence was refused, as the reply's text.
fn chunk_refusal(e: ChunkError) -> String {
    match e {
        ChunkError::TooLarge { bytes, limit } => {
            format!("state is {bytes} bytes, over the {limit}-byte limit")
        }
        ChunkError::OutOfOrder { got, expected } => {
            format!("state chunk {got} arrived where {expected} was expected")
        }
        ChunkError::Empty { seq } => format!("state chunk {seq} is empty and not the last"),
    }
}

/// The reply to a load that failed. The wire carries a message and no stage, so
/// a loader failure keeps its stage in the text.
fn load_failure(e: &BridgeError) -> BridgeMessage {
    let message = match e {
        BridgeError::LoadFailed { stage, reason, .. } => format!("{stage}: {reason}"),
        other => other.to_string(),
    };
    BridgeMessage::Error { message }
}

// Tests — pure dispatch, no tokio, no sockets.

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::main_thread;

    // `Features` is named only by `load_clap_plugin_f64`, which carries the
    // same gate.
    use tutti_plugin::LoadStage;
    #[cfg(feature = "clap")]
    use tutti_plugin::server::Features;
    use tutti_plugin::server::{
        MidiEventVec, NoteExpressionChanges, ParamId, ParameterChanges, TransportInfo,
    };

    fn assert_none(r: Reaction) {
        assert!(matches!(r, Reaction::None), "got {r:?}");
    }

    fn assert_error_contains(r: Reaction, needle: &str) {
        let reply = r.into_reply();
        match reply {
            BridgeMessage::Error { ref message } if message.contains(needle) => {}
            _ => panic!("expected Error containing {needle:?}, got {reply:?}"),
        }
    }

    #[test]
    fn new_session_defaults() {
        let s = Session::new();
        assert_eq!(s.clock.sample_rate, 44100.0);
        assert_eq!(s.clock.format, SampleFormat::Float32);
        assert!(s.plugin.is_none());
        assert!(s.shm.is_none());
    }

    /// Every request a host can send before a plugin is loaded must be
    /// *answered* — with an absent value, an empty list, or a refusal, but
    /// never with silence. The host blocks on a reply frame for each of the
    /// query-shaped ones, so a missing answer is a hang rather than an error.
    ///
    /// The table is the point: adding a `HostMessage` variant without adding a
    /// row leaves its no-plugin path uncovered, and the panic messages name the
    /// variant so a failure says which one.
    #[test]
    fn every_request_is_answered_with_no_plugin_loaded() {
        /// What a no-plugin reply is allowed to be.
        enum Expect {
            /// Nothing to say, and nothing waiting on it.
            Silent,
            /// A refusal whose message carries this substring.
            Refusal(&'static str),
            /// A reply frame whose payload must be absent or empty.
            EmptyPayload,
        }
        use Expect::*;

        let cases: Vec<(&str, HostMessage, Expect)> = vec![
            (
                "GetParameter",
                HostMessage::GetParameter {
                    param_id: ParamId::new(0),
                },
                EmptyPayload,
            ),
            (
                "GetParameterList",
                HostMessage::GetParameterList,
                EmptyPayload,
            ),
            (
                "GetParameterInfo",
                HostMessage::GetParameterInfo {
                    param_id: ParamId::new(0),
                },
                EmptyPayload,
            ),
            ("SaveState", HostMessage::SaveState, EmptyPayload),
            (
                "ProcessAudio",
                HostMessage::ProcessAudio(tutti_plugin::server::ProcessAudioData {
                    seq: 0,
                    num_samples: 256,
                    midi_events: MidiEventVec::new(),
                    param_changes: ParameterChanges::new(),
                    note_expression: NoteExpressionChanges::new(),
                    transport: TransportInfo::default(),
                    ..Default::default()
                }),
                Refusal("No plugin loaded"),
            ),
            (
                "SetParameter",
                HostMessage::SetParameter {
                    param_id: ParamId::new(0),
                    value: Normalized::of(0.5),
                },
                Silent,
            ),
            ("Reset", HostMessage::Reset, Silent),
        ];

        for (name, msg, expect) in cases {
            let mut s = Session::new();
            let r = s
                .handle(&main_thread(), msg)
                .unwrap_or_else(|e| panic!("{name} errored with no plugin: {e:?}"));

            match expect {
                Silent => assert!(matches!(r, Reaction::None), "{name}: got {r:?}"),
                Refusal(needle) => assert_error_contains(r, needle),
                EmptyPayload => match r.into_reply() {
                    BridgeMessage::ParameterValue { value } => {
                        assert!(value.is_none(), "{name}: got a value with no plugin")
                    }
                    BridgeMessage::ParameterList { parameters } => {
                        assert!(parameters.is_empty(), "{name}: got parameters")
                    }
                    BridgeMessage::ParameterInfoResponse { info } => {
                        assert!(info.is_none(), "{name}: got param info")
                    }
                    BridgeMessage::StateChunk { bytes, last, .. } => {
                        assert!(bytes.is_empty(), "{name}: got state bytes");
                        assert!(last, "{name}: an empty state must terminate its sequence");
                    }
                    other => panic!("{name}: unexpected reply {other:?}"),
                },
            }
        }
    }

    #[test]
    fn set_sample_rate_no_plugin_updates_clock() {
        let mut s = Session::new();
        let r = s
            .handle(
                &main_thread(),
                HostMessage::SetSampleRate {
                    rate: tutti_plugin_types::SampleRate(96000.0),
                },
            )
            .unwrap()
            .into_reply();
        assert!(matches!(
            r,
            BridgeMessage::SampleRateSet { rate, error: None } if rate.get() == 96000.0
        ));
        assert_eq!(s.clock.sample_rate, 96000.0);
    }

    /// A `LoadState` with no plugin loaded answers, and answers with a refusal.
    ///
    /// Answering `Reaction::None` — silence — is the failure this pins. The host
    /// *waits* for this frame, so a silence hangs it until the state timeout;
    /// and paired with a host that assumes success, it reports a state load that
    /// never happened.
    #[test]
    fn load_state_no_plugin_refuses_rather_than_going_silent() {
        let mut s = Session::new();
        let reply = s
            .handle(
                &main_thread(),
                HostMessage::LoadStateChunk {
                    seq: 0,
                    last: true,
                    bytes: vec![1, 2, 3],
                },
            )
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::StateLoaded { error: Some(msg) } => {
                assert!(msg.contains("no plugin"), "unhelpful message: {msg:?}");
            }
            other => panic!("expected a StateLoaded carrying a refusal, got {other:?}"),
        }
    }

    /// **A refused load sequence is answered once, on its `last` chunk.** The
    /// host reads one `StateLoaded` per load; an answer at the refused chunk
    /// left every later chunk of the sequence to fail again and answer again,
    /// each read as the reply to a later request. A chunk 0 starts afresh.
    ///
    /// Mutation (run): answering at the refused chunk as before (dropping the
    /// `if last` wait) → the out-of-order chunk 2 is answered → fails.
    #[test]
    fn a_refused_load_sequence_is_answered_once_at_its_end() {
        let mut s = Session::new();
        let mut chunk = |seq, last| {
            s.handle(
                &main_thread(),
                HostMessage::LoadStateChunk {
                    seq,
                    last,
                    bytes: vec![1, 2, 3],
                },
            )
            .unwrap()
        };
        assert_none(chunk(0, false));
        assert_none(chunk(2, false)); // out of order: refused, not yet answered
        assert_none(chunk(3, false));
        match chunk(4, true).into_reply() {
            BridgeMessage::StateLoaded { error: Some(msg) } => {
                assert!(msg.contains("chunk 2"), "the refusal's own reason: {msg:?}");
            }
            other => panic!("expected the refusal at `last`, got {other:?}"),
        }
        // A new sequence is not refused for the last one's fault.
        match chunk(0, true).into_reply() {
            BridgeMessage::StateLoaded { error: Some(msg) } => {
                assert!(msg.contains("no plugin"), "{msg:?}");
            }
            other => panic!("expected the no-plugin refusal, got {other:?}"),
        }
    }

    #[test]
    fn unload_plugin_clears_shm() {
        let mut s = Session::new();
        let r = s.handle(&main_thread(), HostMessage::UnloadPlugin).unwrap();
        assert_none(r);
        assert!(s.plugin.is_none());
        assert!(s.shm.is_none());
    }

    #[test]
    fn shutdown_signals_shutdown() {
        let mut s = Session::new();
        let r = s.handle(&main_thread(), HostMessage::Shutdown).unwrap();
        assert!(matches!(r, Reaction::Shutdown));
    }

    #[test]
    fn load_plugin_missing_path_errors() {
        let mut s = Session::new();
        let reply = s
            .handle(
                &main_thread(),
                HostMessage::LoadPlugin {
                    path: std::path::PathBuf::from("/nonexistent/path/plugin.vst3"),
                    sample_rate: tutti_plugin_types::SampleRate(44100.0),
                    block_size: 512,
                    preferred_format: SampleFormat::Float32,
                    shm_name: "x".into(),
                    state: None,
                },
            )
            .expect("a failed load is a reply, not a server error")
            .into_reply();
        match reply {
            BridgeMessage::Error { message } => {
                assert!(message.starts_with(&format!("{}:", LoadStage::Scanning)));
                assert!(message.contains("not found"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(s.plugin.is_none());
    }

    // --------------------------------------------------------------------
    // CLAP integration tests — require TAL-NoiseMaker installed.
    // --------------------------------------------------------------------

    #[cfg(feature = "clap")]
    /// The reference CLAP plugin, built as a dev-dependency by this same
    /// `cargo test` run. Resolved rather than hard-coded so these tests run on
    /// any machine.
    ///
    /// Uses the `.clap`-suffixed link: these tests load through
    /// `HostMessage::LoadPlugin`, which picks the format from the file
    /// extension, and the raw cdylib is a `.so` — which that table reads as
    /// VST2.
    fn clap_plugin() -> &'static str {
        crate::test_utils::clap_probe_path_dot_clap()
    }

    /// Load the real CLAP plugin into a Session and attach shared memory.
    /// Returns (session, shm_guard). The guard keeps the creator-side mmap
    /// alive while the session holds its own mapping.
    #[cfg(feature = "clap")]
    fn load_clap(name: &str, preferred_format: SampleFormat) -> (Session, SlabWriter) {
        let mut s = Session::new();
        s.handle(
            &main_thread(),
            HostMessage::LoadPlugin {
                path: std::path::PathBuf::from(clap_plugin()),
                sample_rate: tutti_plugin_types::SampleRate(44100.0),
                block_size: 512,
                preferred_format,
                shm_name: name.to_string(),
                state: None,
            },
        )
        .unwrap();

        let buffer_name = format!("tutti_vst_buffer_{}_{}", name, std::process::id());
        // The negotiated format, not the requested one, as the host's
        // `setup_shm` sizes the slab.
        let layout = tutti_plugin::server::SlabLayout {
            samples_per_channel: 8192,
            format: s.clock.format,
            slots: tutti_plugin::server::RING_SLOTS as u32,
            inputs: vec![tutti_plugin::server::ChannelLayout::STEREO],
            outputs: vec![tutti_plugin::server::ChannelLayout::STEREO],
        };
        let shm_guard = AudioSlab::create(buffer_name.clone(), layout.clone()).unwrap();
        s.shm = Some(AudioSlab::open(buffer_name, layout).unwrap());
        (s, shm_guard)
    }

    /// Run one block of `input` on both channels through `s`'s slab.
    #[cfg(feature = "clap")]
    fn process_block(s: &mut Session, shm: &mut SlabWriter, seq: u64, input: f64) {
        for ch in 0..2 {
            shm.write::<f64>(seq, ch, &[input; F64_FRAMES]).unwrap();
        }
        shm.publish(seq);
        let reply = s
            .handle(
                &main_thread(),
                HostMessage::ProcessAudio(tutti_plugin::server::ProcessAudioData {
                    seq,
                    num_samples: F64_FRAMES,
                    ..Default::default()
                }),
            )
            .unwrap()
            .into_reply();
        assert!(
            matches!(reply, BridgeMessage::AudioProcessed { .. }),
            "{reply:?}"
        );
    }

    /// **A refused rate is answered, silent, and retried by the next rate.**
    /// The clock follows the host's rate throughout. The plugin here drops
    /// its 64-bit ports, so the refusal leaves it deactivated; it publishes
    /// nothing while refused (the host reads silence), refuses again at the
    /// same rate, and a later rate it can take revives it, with audio at the
    /// right rate and no `deactivate` of the inactive instance.
    ///
    /// Mutations (run): drop the `rate_refused` early return in
    /// `handle_process` → the refused block is published (or fails to be
    /// silent) → fails. Never clear `rate_refused` on success → the revived
    /// block is not published → fails. Move the clock assignment into the
    /// `Ok` arm → the clock stays 48 kHz after the refusal → fails. (The
    /// `deactivate` guard is pinned by `clap_ports_rescan`.)
    #[test]
    #[cfg(feature = "clap")]
    fn a_refused_rate_is_silent_and_a_later_rate_revives_the_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let _render = crate::test_utils::clap_probe_render_mode(
            tutti_clap_test_plugin::RenderMode::TagPassthrough as u32,
        );
        let (mut s, mut shm) = load_clap("rate_refusal_cycle", SampleFormat::Float64);
        assert_eq!(s.clock.format, SampleFormat::Float64, "fixture");
        let set = |s: &mut Session, rate: f64| {
            s.handle(
                &main_thread(),
                HostMessage::SetSampleRate {
                    rate: tutti_plugin_types::SampleRate(rate),
                },
            )
            .unwrap()
            .into_reply()
        };

        let accepted = set(&mut s, 48000.0);
        assert!(
            matches!(accepted, BridgeMessage::SampleRateSet { error: None, .. }),
            "{accepted:?}"
        );
        assert_eq!(s.clock.sample_rate, 48000.0);
        assert!(
            s.drain_async_events(&main_thread()).is_empty(),
            "nothing changed"
        );
        process_block(&mut s, &mut shm, 1, 0.5);
        assert!(shm.has_output(1), "a running plugin publishes");

        // The plugin drops its 64-bit ports: the next rate is refused.
        let dropped = crate::test_utils::clap_probe_ports_64bit(0);
        let refused = set(&mut s, 96000.0);
        assert!(
            matches!(
                &refused,
                BridgeMessage::SampleRateSet { rate, error: Some(e) }
                    if rate.get() == 96000.0 && !e.is_empty()
            ),
            "the host is told, with a reason: {refused:?}"
        );
        assert_eq!(s.clock.sample_rate, 96000.0, "the clock follows the host");
        assert!(s.rate_refused.is_some());
        assert!(
            s.drain_async_events(&main_thread())
                .iter()
                .any(|m| matches!(m, BridgeMessage::PluginIoChanged)),
            "the host is told the instance can no longer run"
        );
        process_block(&mut s, &mut shm, 2, 0.5);
        assert!(!shm.has_output(2), "a refused plugin publishes nothing");

        // Refused again at the same rate: still silent.
        assert!(matches!(
            set(&mut s, 96000.0),
            BridgeMessage::SampleRateSet { error: Some(_), .. }
        ));
        process_block(&mut s, &mut shm, 3, 0.5);
        assert!(!shm.has_output(3), "still silent");

        // The ports come back: the next rate revives it.
        drop(dropped);
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let revived = set(&mut s, 88200.0);
        assert!(
            matches!(revived, BridgeMessage::SampleRateSet { error: None, .. }),
            "{revived:?}"
        );
        assert!(s.rate_refused.is_none());
        assert_eq!(s.clock.sample_rate, 88200.0);
        process_block(&mut s, &mut shm, 4, 0.5);
        assert!(shm.has_output(4), "a revived plugin publishes again");
        let mut out = vec![0.0f64; F64_FRAMES];
        shm.read_output_into::<f64>(4, 0, &mut out).unwrap();
        let want = 0.5 + f64::from(tutti_clap_test_plugin::probe_tag(0, 0));
        assert!(out.iter().all(|&x| x == want), "{:?}", &out[..4]);
    }

    /// An f64 session with a plugin that advertises no 64-bit port runs in
    /// f32: the request is a preference, and the reference plugin's default
    /// ports are f32-only.
    #[test]
    #[cfg(feature = "clap")]
    fn load_clap_plugin_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (s, _shm) = load_clap("load_clap_f64", SampleFormat::Float64);
        let loaded = s.plugin.as_ref().unwrap().instance().loaded();
        assert!(!loaded.features.contains(Features::F64_AUDIO));
        assert_eq!(s.clock.format, SampleFormat::Float32);
    }

    /// The frames each 64-bit round trip below renders.
    #[cfg(feature = "clap")]
    const F64_FRAMES: usize = 64;

    /// Load the reference plugin with the 64-bit port `mask` into a session
    /// that prefers `format`, in `TagPassthrough` mode
    /// (`out[0][ch] = in[0][ch] + probe_tag(0, ch)`), and run one block with
    /// `input` on both channels through the shared-memory slab, as the host
    /// does. The render mode goes back to `Inert` when the guard drops, on a
    /// failed assertion too.
    #[cfg(feature = "clap")]
    fn clap_round_trip<T>(name: &str, mask: u32, format: SampleFormat, input: T) -> RoundTrip<T>
    where
        T: tutti_plugin::server::Sample + bytemuck::Pod,
    {
        const SEQ: u64 = 1;
        let _ports = crate::test_utils::clap_probe_ports_64bit(mask);
        let _render = crate::test_utils::clap_probe_render_mode(
            tutti_clap_test_plugin::RenderMode::TagPassthrough as u32,
        );
        let (mut s, mut shm) = load_clap(name, format);
        for ch in 0..2 {
            shm.write::<T>(SEQ, ch, &[input; F64_FRAMES]).unwrap();
        }
        shm.publish(SEQ);
        let reply = s
            .handle(
                &main_thread(),
                HostMessage::ProcessAudio(tutti_plugin::server::ProcessAudioData {
                    seq: SEQ,
                    num_samples: F64_FRAMES,
                    ..Default::default()
                }),
            )
            .unwrap()
            .into_reply();
        assert!(
            matches!(reply, BridgeMessage::AudioProcessed { .. }),
            "unexpected reply: {reply:?}"
        );
        assert!(
            shm.has_output(SEQ),
            "the block was not published: the plugin refused it"
        );
        let out = [0, 1].map(|ch| {
            let mut v = vec![T::default(); F64_FRAMES];
            shm.read_output_into::<T>(SEQ, ch, &mut v).unwrap();
            v
        });
        let loaded = s.plugin.as_ref().unwrap().instance().loaded();
        RoundTrip {
            format: s.clock.format,
            f64_audio: loaded.features.contains(Features::F64_AUDIO),
            inputs: loaded.input_widths(),
            outputs: loaded.output_widths(),
            out,
        }
    }

    /// The probe's call-site record, from the image `load_clap` loaded.
    #[cfg(feature = "clap")]
    fn clap_probe_sites() -> tutti_clap_test_plugin::ThreadCapture {
        crate::test_utils::clap().thread_capture()
    }

    /// **A reset reaches the plugin, between the blocks it was sent between**
    /// (design 016 §6.2). The server dropped `HostMessage::Reset`, so a seek
    /// left every out-of-process CLAP and VST3 plugin's delay lines, reverb
    /// tails and filter memories sounding across the jump.
    ///
    /// The probe counts its `reset` and `process` calls; after each message
    /// the counts must be the ones that message implies, so the reset is seen
    /// once and in order.
    ///
    /// Mutation (run): restore `M::Reset => Ok(Reaction::None)` → the probe's
    /// `reset` count stays 0 → fails.
    #[test]
    #[cfg(feature = "clap")]
    fn a_reset_reaches_the_plugin_between_two_blocks() {
        use tutti_clap_test_plugin::Site;

        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, mut shm) = load_clap("reset_reaches_the_plugin", SampleFormat::Float32);
        let visits = |site: Site| clap_probe_sites().sites[site as usize].visits;
        let resets_at_load = visits(Site::Reset);
        let processes_at_load = visits(Site::Process);

        let mut block = |s: &mut Session, seq: u64| {
            for ch in 0..2 {
                shm.write::<f32>(seq, ch, &[0.0; F64_FRAMES]).unwrap();
            }
            shm.publish(seq);
            let reply = s
                .handle(
                    &main_thread(),
                    HostMessage::ProcessAudio(tutti_plugin::server::ProcessAudioData {
                        seq,
                        num_samples: F64_FRAMES,
                        ..Default::default()
                    }),
                )
                .unwrap()
                .into_reply();
            assert!(
                matches!(reply, BridgeMessage::AudioProcessed { .. }),
                "{reply:?}"
            );
        };

        block(&mut s, 1);
        assert_eq!(visits(Site::Process), processes_at_load + 1);
        assert_eq!(visits(Site::Reset), resets_at_load, "no reset was sent yet");

        s.handle(&main_thread(), HostMessage::Reset).unwrap();
        assert_eq!(
            visits(Site::Reset),
            resets_at_load + 1,
            "the reset reached the plugin"
        );
        assert_eq!(
            visits(Site::Process),
            processes_at_load + 1,
            "and no block ran"
        );

        block(&mut s, 2);
        assert_eq!(visits(Site::Process), processes_at_load + 2);
        assert_eq!(
            visits(Site::Reset),
            resets_at_load + 1,
            "one reset, between the blocks"
        );
    }

    /// **A block's length is the server's to check (doc 027 D12).** An empty
    /// block, one past the length the plugin was activated for (`block_size`,
    /// 512 here), and one past anything memory holds are refused before the
    /// plugin, or the scratch, sees them. Each is answered, published nothing
    /// (the host hears silence) and counted. A block within bounds still
    /// renders afterwards.
    ///
    /// Mutation (run): `block_fits` returning `true` → the 2^40-frame block
    /// sizes the scratch to it (an allocation failure aborts the test), and
    /// the 513-frame one reaches the plugin's `process` → fails.
    #[test]
    #[cfg(feature = "clap")]
    fn a_block_of_a_length_the_plugin_cannot_take_never_reaches_it() {
        use tutti_clap_test_plugin::Site;

        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, mut shm) = load_clap("block_bounds", SampleFormat::Float32);
        let visits = |site: Site| clap_probe_sites().sites[site as usize].visits;
        let processes_at_load = visits(Site::Process);

        let mut block = |s: &mut Session, seq: u64, frames: usize| {
            for ch in 0..2 {
                shm.write::<f32>(seq, ch, &[0.0; 512]).unwrap();
            }
            shm.publish(seq);
            let reply = s
                .handle(
                    &main_thread(),
                    HostMessage::ProcessAudio(tutti_plugin::server::ProcessAudioData {
                        seq,
                        num_samples: frames,
                        ..Default::default()
                    }),
                )
                .unwrap()
                .into_reply();
            assert!(
                matches!(reply, BridgeMessage::AudioProcessed { .. }),
                "every block is answered: {reply:?}"
            );
            shm.has_output(seq)
        };

        for (seq, frames) in [(1, 0), (2, 513), (3, 1 << 40)] {
            assert!(!block(&mut s, seq, frames), "{frames} frames published");
        }
        assert_eq!(s.refused_blocks, 3);
        assert_eq!(
            visits(Site::Process),
            processes_at_load,
            "no refused block reached the plugin"
        );

        assert!(block(&mut s, 4, 512), "a block within bounds renders");
        assert_eq!(visits(Site::Process), processes_at_load + 1);
        assert_eq!(s.refused_blocks, 3);
    }

    /// **A rate no clock can run at is refused at the edge (doc 027 D13).** The
    /// wire does not range-check a `SampleRate`; a NaN or zero must not reach
    /// the plugin or the session's clock, and the host is told why.
    ///
    /// Mutation (run): delete the finite-and-positive check → the clock takes
    /// NaN → fails.
    #[test]
    #[cfg(feature = "clap")]
    fn a_rate_that_is_not_a_rate_is_refused() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("bad_rate", SampleFormat::Float32);
        let before = s.clock.sample_rate;
        for bad in [f64::NAN, 0.0, -48_000.0, f64::INFINITY] {
            let reply = s
                .handle(
                    &main_thread(),
                    HostMessage::SetSampleRate {
                        rate: tutti_plugin_types::SampleRate(bad),
                    },
                )
                .unwrap()
                .into_reply();
            assert!(
                matches!(reply, BridgeMessage::SampleRateSet { error: Some(_), .. }),
                "{bad}: {reply:?}"
            );
            assert_eq!(s.clock.sample_rate.to_bits(), before.to_bits(), "{bad}");
            assert!(
                s.rate_refused.is_none(),
                "{bad}: the plugin keeps rendering"
            );
        }
    }

    /// What [`clap_round_trip`] observed.
    #[cfg(feature = "clap")]
    struct RoundTrip<T> {
        /// The session's negotiated format.
        format: SampleFormat,
        /// The plugin's `F64_AUDIO` bit.
        f64_audio: bool,
        /// The node's graph-facing input bus widths, as the server reports them.
        inputs: tutti_plugin::server::BusChannels,
        /// The node's graph-facing output buses.
        outputs: tutti_plugin::server::BusChannels,
        /// The two output channels.
        out: [Vec<T>; 2],
    }

    /// An f32 session (the default) runs a plugin whose every port can do
    /// 64-bit at f32, and its audio comes back.
    ///
    /// Mutation (run): activating `F64` whenever the plugin supports it (the
    /// old `if supports_f64`, ignoring `prefer_f64`) → every block is refused
    /// as a format mismatch and nothing is published; fails. Reporting
    /// `F64_AUDIO` from the capability rather than the activation (the old
    /// `features.set(F64_AUDIO, supports_f64)`) also fails.
    #[test]
    #[cfg(feature = "clap")]
    fn an_f32_session_processes_audio_through_a_64bit_capable_clap_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let RoundTrip {
            format,
            f64_audio,
            out,
            ..
        } = clap_round_trip(
            "f32_session_64bit_clap",
            both,
            SampleFormat::Float32,
            0.5f32,
        );
        assert_eq!(format, SampleFormat::Float32);
        assert!(!f64_audio, "activated f64 for an f32 session");
        for (ch, samples) in out.iter().enumerate() {
            let want = 0.5 + tutti_clap_test_plugin::probe_tag(0, ch as u32);
            assert!(
                samples.iter().all(|&x| x == want),
                "channel {ch}: want {want}, got {:?}",
                &samples[..4]
            );
        }
    }

    /// An f64 session runs a plugin whose every port can do 64-bit at f64,
    /// end to end: the input keeps precision f32 cannot carry.
    ///
    /// `0.1 + tag` is correctly rounded and deterministic, so the plugin's
    /// f64 sum is compared exactly against the same sum here; the same sum
    /// through f32 lands on a different value (asserted, so the check cannot
    /// pass on a path that rounds through f32).
    ///
    /// Mutation (run): `prefer_f64 && supports_f64` → `false` in
    /// `ClapInstance::load` → the session negotiates f32; fails.
    #[test]
    #[cfg(feature = "clap")]
    fn an_f64_session_runs_a_64bit_capable_clap_plugin_in_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let RoundTrip {
            format,
            f64_audio,
            out,
            ..
        } = clap_round_trip(
            "f64_session_64bit_clap",
            both,
            SampleFormat::Float64,
            0.1f64,
        );
        assert_eq!(format, SampleFormat::Float64);
        assert!(f64_audio);
        for (ch, samples) in out.iter().enumerate() {
            let tag = tutti_clap_test_plugin::probe_tag(0, ch as u32);
            let want = 0.1f64 + f64::from(tag);
            assert_ne!(want, f64::from(0.1f32 + tag), "the input must not fit f32");
            assert!(
                samples.iter().all(|&x| x == want),
                "channel {ch}: want {want}, got {:?}",
                &samples[..4]
            );
        }
    }

    /// A plugin whose outputs claim 64-bit but whose inputs do not runs at
    /// f32 even in an f64 session: the host hands one width for every buffer.
    ///
    /// Mutation (run): `check_f64_support` scanning the outputs only (the old
    /// check) → the session negotiates f64; fails. The loader
    /// using `requested` instead of `negotiate_format`'s answer → the
    /// activation is refused; fails.
    #[test]
    #[cfg(feature = "clap")]
    fn an_f64_session_runs_a_clap_plugin_with_f32_inputs_in_f32() {
        let _lock = crate::test_utils::plugin_load_lock();
        let RoundTrip {
            format,
            f64_audio,
            out,
            ..
        } = clap_round_trip(
            "f64_session_f32_inputs_clap",
            tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS,
            SampleFormat::Float64,
            0.5f32,
        );
        assert_eq!(format, SampleFormat::Float32);
        assert!(!f64_audio);
        let want = 0.5 + tutti_clap_test_plugin::probe_tag(0, 0);
        assert!(out[0].iter().all(|&x| x == want));
    }

    /// An f64 instrument (no input port, 64-bit stereo out) runs end to end
    /// in an f64 session and is handed no input buffer, as CLAP requires
    /// (`audio_inputs_count` equals the plugin's own input port count), and
    /// reports no input bus: a direction with no audio is an empty list.
    ///
    /// The slab's inputs carry `0.1`; `TagPassthrough` adds input port 0 to
    /// output port 0 when the host hands one, so an undeclared input bus
    /// shows up as `0.1 + tag` rather than `tag`. With the bus handed at f64
    /// its `data32` is null, which a plugin that reads `data32` for every
    /// counted input would dereference.
    ///
    /// Mutations (run): handing the plugin a stereo input port it did not
    /// declare (`PortLayout::inputs` given one when empty, in
    /// `read_audio_ports`) → the plugin is handed a stereo input bus carrying
    /// the slab's `0.1`, and every output sample is `0.1 + tag`; fails.
    /// Reporting a stereo input for a plugin with none (the totals read off a
    /// stereo-defaulted copy, as before) → the input list is a stereo bus;
    /// fails.
    #[test]
    #[cfg(feature = "clap")]
    fn an_f64_clap_instrument_is_handed_no_input_buffer() {
        let _lock = crate::test_utils::plugin_load_lock();
        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::Instrument as u32,
        );
        let RoundTrip {
            format,
            f64_audio,
            inputs,
            outputs,
            out,
        } = clap_round_trip(
            "f64_clap_instrument",
            tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS,
            SampleFormat::Float64,
            0.1f64,
        );
        assert_eq!(format, SampleFormat::Float64);
        assert!(f64_audio, "an instrument with 64-bit outputs runs at f64");
        assert!(
            inputs.is_empty(),
            "an instrument has no input bus: {inputs:?}"
        );
        assert_eq!(
            outputs.as_slice(),
            &[tutti_plugin::server::ChannelLayout::STEREO]
        );
        for (ch, samples) in out.iter().enumerate() {
            let want = f64::from(tutti_clap_test_plugin::probe_tag(0, ch as u32));
            assert!(
                samples.iter().all(|&x| x == want),
                "channel {ch}: want {want} (no input handed), got {:?}",
                &samples[..4]
            );
        }
    }

    /// Fetch any param_id from the loaded plugin's parameter list.
    #[cfg(feature = "clap")]
    fn first_param_id(s: &mut Session) -> ParamId {
        let reply = s
            .handle(&main_thread(), HostMessage::GetParameterList)
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterList { parameters } => {
                assert!(!parameters.is_empty(), "need at least one parameter");
                parameters[0].id
            }
            other => panic!("expected ParameterList, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn get_parameter_list_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("get_param_list_clap", SampleFormat::Float32);
        let reply = s
            .handle(&main_thread(), HostMessage::GetParameterList)
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterList { parameters } => assert!(!parameters.is_empty()),
            other => panic!("expected ParameterList, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn get_parameter_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("get_param_clap", SampleFormat::Float32);
        let param_id = first_param_id(&mut s);
        let reply = s
            .handle(&main_thread(), HostMessage::GetParameter { param_id })
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterValue { value } => assert!(value.is_some()),
            other => panic!("expected ParameterValue, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn get_parameter_info_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("get_param_info_clap", SampleFormat::Float32);
        let param_id = first_param_id(&mut s);
        let reply = s
            .handle(&main_thread(), HostMessage::GetParameterInfo { param_id })
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterInfoResponse { info } => {
                let info = info.expect("info should exist");
                assert_eq!(info.id, param_id);
            }
            other => panic!("expected ParameterInfoResponse, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn set_parameter_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("set_param_clap", SampleFormat::Float32);
        let param_id = first_param_id(&mut s);
        let r = s
            .handle(
                &main_thread(),
                HostMessage::SetParameter {
                    param_id,
                    value: Normalized::of(0.5),
                },
            )
            .unwrap();
        assert_none(r);
    }

    /// A `LoadState` that the plugin accepted.
    ///
    /// Asserts `StateLoaded { error: None }` rather than mere silence, which is
    /// strictly stronger: against an `assert_none`, a refusal is
    /// indistinguishable from a success at this layer.
    #[cfg(feature = "clap")]
    fn assert_state_loaded(r: Reaction) {
        match r.into_reply() {
            BridgeMessage::StateLoaded { error: None } => {}
            other => panic!("expected an accepted StateLoaded, got {other:?}"),
        }
    }

    /// Pull a SaveState blob out of a session, asserting it is non-empty.
    ///
    /// Reassembles the `StateChunk` sequence exactly as the host does, so these
    /// tests exercise the chunked path rather than a shape that only happens to
    /// be one frame. The sequence must terminate, and `Reassembler` rejects a
    /// gap — so a server that mis-numbers its chunks fails here rather than
    /// producing a silently short blob.
    #[cfg(feature = "clap")]
    fn save_state_bytes(s: &mut Session) -> Vec<u8> {
        let mut acc = Reassembler::default();
        let mut done = false;
        for m in s
            .handle(&main_thread(), HostMessage::SaveState)
            .unwrap()
            .into_replies()
        {
            match m {
                BridgeMessage::StateChunk { seq, last, bytes } => {
                    assert!(!done, "chunks continued after `last`");
                    done = acc
                        .push(seq, last, &bytes)
                        .expect("well-formed chunk sequence");
                }
                other => panic!("expected StateChunk, got {other:?}"),
            }
        }
        assert!(done, "the state sequence never set `last`");
        let data = acc.into_inner();
        assert!(!data.is_empty(), "state should be non-empty");
        data
    }

    /// Feed a whole blob in as a chunk sequence and return the final reaction.
    ///
    /// Mirrors `save_state_bytes`: the host splits, so a test that pushed one
    /// frame would leave the multi-chunk accumulation path untested.
    #[cfg(feature = "clap")]
    fn load_state_bytes(s: &mut Session, data: &[u8]) -> Reaction {
        let mut last_reaction = Reaction::None;
        for (seq, last, bytes) in state_chunk::split(data) {
            last_reaction = s
                .handle(
                    &main_thread(),
                    HostMessage::LoadStateChunk {
                        seq,
                        last,
                        bytes: bytes.to_vec(),
                    },
                )
                .unwrap();
        }
        last_reaction
    }

    /// Stages `data` as a host does before a load: the whole sequence, and the
    /// final reaction.
    #[cfg(feature = "clap")]
    fn stage_state_bytes(s: &mut Session, data: &[u8]) -> Reaction {
        let mut last_reaction = Reaction::None;
        for (seq, last, bytes) in state_chunk::split(data) {
            last_reaction = s
                .handle(
                    &main_thread(),
                    HostMessage::StageStateChunk {
                        seq,
                        last,
                        bytes: bytes.to_vec(),
                    },
                )
                .unwrap();
        }
        last_reaction
    }

    #[cfg(feature = "clap")]
    fn load_reply(s: &mut Session, declared: Option<StagedState>) -> Option<String> {
        let reply = s
            .handle(
                &main_thread(),
                HostMessage::LoadPlugin {
                    path: std::path::PathBuf::from(clap_plugin()),
                    sample_rate: tutti_plugin_types::SampleRate(44100.0),
                    block_size: 512,
                    preferred_format: SampleFormat::Float32,
                    shm_name: "staged".into(),
                    state: declared,
                },
            )
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::PluginLoaded { state_error, .. } => state_error,
            other => panic!("expected PluginLoaded, got {other:?}"),
        }
    }

    fn load_declaring(s: &mut Session, declared: Option<StagedState>) -> BridgeMessage {
        s.handle(
            &main_thread(),
            HostMessage::LoadPlugin {
                path: std::path::PathBuf::from("/nonexistent/plugin.clap"),
                sample_rate: tutti_plugin_types::SampleRate(44100.0),
                block_size: 512,
                preferred_format: SampleFormat::Float32,
                shm_name: "x".into(),
                state: declared,
            },
        )
        .unwrap()
        .into_reply()
    }

    /// **A refused load leaves no plugin behind.** The server hosts one
    /// plugin and `LoadPlugin` replaces it; when the second load fails, the
    /// first is gone too, not kept answering for a host that was told the load
    /// failed.
    ///
    /// Mutation (run): removing the `self.plugin = None` at the top of
    /// `handle_load` leaves the first plugin loaded after the refusal.
    #[cfg(feature = "clap")]
    #[test]
    fn a_failed_second_load_unloads_the_first() {
        let mut s = Session::new();
        load_reply(&mut s, None);
        assert!(s.plugin.is_some(), "the first load holds a plugin");
        let reply = load_declaring(&mut s, None);
        assert!(matches!(reply, BridgeMessage::Error { .. }), "{reply:?}");
        assert!(s.plugin.is_none(), "the refused load left the first plugin");
    }

    fn stage(s: &mut Session, seq: u32, last: bool, bytes: &[u8]) -> Reaction {
        s.handle(
            &main_thread(),
            HostMessage::StageStateChunk {
                seq,
                last,
                bytes: bytes.to_vec(),
            },
        )
        .unwrap()
    }

    fn assert_mismatch(reply: BridgeMessage) {
        match reply {
            BridgeMessage::Error { message } => {
                assert!(message.contains("declares a state"), "{message}");
            }
            other => panic!("expected the mismatch refused, got {other:?}"),
        }
    }

    /// **A load restores the state it declares and holds, no other.** A state
    /// declared and none staged, a state staged and none declared, and a length
    /// that disagrees are each refused, and each leaves nothing staged. A load that
    /// declares nothing and holds nothing goes on to load (here: to not find the
    /// plugin).
    ///
    /// Mutation (run): `handle_load` restoring whatever is staged, whatever the
    /// load declares, fails the second and third assertions.
    #[test]
    fn a_load_restores_only_the_state_it_declares() {
        let mut s = Session::new();
        assert_mismatch(load_declaring(&mut s, Some(StagedState { len: 3 })));

        let mut s = Session::new();
        assert_state_loaded_reply(stage(&mut s, 0, true, &[1, 2, 3]));
        assert_mismatch(load_declaring(&mut s, None));
        assert!(s.staged.is_none(), "and the stage is gone");

        let mut s = Session::new();
        assert_state_loaded_reply(stage(&mut s, 0, true, &[1, 2, 3]));
        assert_mismatch(load_declaring(&mut s, Some(StagedState { len: 4 })));
        assert!(s.staged.is_none());

        let mut s = Session::new();
        match load_declaring(&mut s, None) {
            BridgeMessage::Error { message } => {
                assert!(message.contains("not found"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    fn assert_state_loaded_reply(r: Reaction) {
        match r.into_reply() {
            BridgeMessage::StateLoaded { error: None } => {}
            other => panic!("expected a staged state, got {other:?}"),
        }
    }

    /// **A stage belongs to the load that declares it**: an unload that comes
    /// between discards it, and so does a half-received one.
    ///
    /// Mutation (run): `discard_staged` not called in the unload arm fails the
    /// first and second assertions.
    #[test]
    fn a_stage_does_not_outlive_the_next_request() {
        let mut s = Session::new();
        assert_state_loaded_reply(stage(&mut s, 0, true, &[1]));
        let _ = s.handle(&main_thread(), HostMessage::UnloadPlugin);
        assert!(s.staged.is_none(), "an unload discards the stage");

        assert!(matches!(stage(&mut s, 0, false, &[1]), Reaction::None));
        assert!(s.stage_in.is_some());
        let _ = s.handle(&main_thread(), HostMessage::UnloadPlugin);
        assert!(s.stage_in.is_none(), "and a half-received one");
    }

    /// **A state staged before a load is restored by the load**, before the
    /// plugin is activated, and consumed: a plugin's own state goes in and the
    /// reply carries no refusal; bytes it cannot parse are refused in the reply
    /// and the plugin loads at its defaults; and the next load has nothing
    /// staged.
    ///
    /// Mutation (run): `handle_load` passing `None` for the staged state leaves
    /// `state_error` empty for the garbage and fails the second assertion.
    #[test]
    #[cfg(feature = "clap")]
    fn a_staged_state_is_restored_by_the_next_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut source, _shm) = load_clap("stage_source", SampleFormat::Float32);
        let own = save_state_bytes(&mut source);

        let mut s = Session::new();
        assert_state_loaded(stage_state_bytes(&mut s, &own));
        assert!(s.staged.is_some(), "the state waits for the load");
        assert_eq!(
            load_reply(
                &mut s,
                Some(StagedState {
                    len: own.len() as u64
                })
            ),
            None,
            "the plugin took its own state"
        );
        assert!(s.staged.is_none(), "and the load consumed it");

        let mut s = Session::new();
        assert_state_loaded(stage_state_bytes(&mut s, &[1, 2, 3]));
        assert!(
            load_reply(&mut s, Some(StagedState { len: 3 })).is_some(),
            "bytes the plugin cannot parse are refused in the reply"
        );
        assert!(s.plugin.is_some(), "and the plugin is loaded all the same");

        let mut s = Session::new();
        assert_eq!(
            load_reply(&mut s, None),
            None,
            "nothing staged, nothing refused"
        );
    }

    /// **A stateful load says it is restoring before it does, and a stateless
    /// one never does**: the interim sink hears `RestoringState` once, ahead of the
    /// reply.
    ///
    /// Mutation (run): the loader not calling `before_restore` leaves the
    /// stateful load silent and fails the first assertion.
    #[test]
    #[cfg(feature = "clap")]
    fn a_stateful_load_announces_the_restore() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut source, _shm) = load_clap("announce_source", SampleFormat::Float32);
        let own = save_state_bytes(&mut source);
        let load = |s: &mut Session, declared: Option<StagedState>| {
            let mut heard = Vec::new();
            let reply = s
                .handle_with(
                    &main_thread(),
                    HostMessage::LoadPlugin {
                        path: std::path::PathBuf::from(clap_plugin()),
                        sample_rate: tutti_plugin_types::SampleRate(44100.0),
                        block_size: 512,
                        preferred_format: SampleFormat::Float32,
                        shm_name: "announce".into(),
                        state: declared,
                    },
                    &mut |m| heard.push(m),
                )
                .unwrap()
                .into_reply();
            (heard, reply)
        };
        let mut s = Session::new();
        assert_state_loaded(stage_state_bytes(&mut s, &own));
        let (heard, reply) = load(
            &mut s,
            Some(StagedState {
                len: own.len() as u64,
            }),
        );
        assert!(
            matches!(heard.as_slice(), [BridgeMessage::RestoringState]),
            "{heard:?}"
        );
        assert!(matches!(reply, BridgeMessage::PluginLoaded { .. }));

        let mut s = Session::new();
        let (heard, _) = load(&mut s, None);
        assert!(heard.is_empty(), "{heard:?}");
    }

    /// A staged sequence out of order is refused, and nothing is staged.
    ///
    /// Mutation (run): not clearing `staged` on the error leaves the earlier
    /// state waiting.
    #[test]
    #[cfg(feature = "clap")]
    fn a_broken_staging_sequence_stages_nothing() {
        let mut s = Session::new();
        assert_state_loaded(stage_state_bytes(&mut s, &[9, 9]));
        let reply = s
            .handle(
                &main_thread(),
                HostMessage::StageStateChunk {
                    seq: 5,
                    last: true,
                    bytes: vec![1],
                },
            )
            .unwrap()
            .into_reply();
        assert!(
            matches!(reply, BridgeMessage::StateLoaded { error: Some(_) }),
            "{reply:?}"
        );
        assert!(s.staged.is_none());
    }

    #[test]
    #[cfg(feature = "clap")]
    fn save_load_state() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("save_load_state_clap", SampleFormat::Float32);
        let data = save_state_bytes(&mut s);
        let r = load_state_bytes(&mut s, &data);
        assert_state_loaded(r);
    }

    /// Byte-exact round-trip: the bytes a plugin emits must survive the
    /// host<->subprocess IPC boundary and a load+re-save unchanged. Proves
    /// the state path carries the opaque blob verbatim (no UTF-8 assumption,
    /// truncation, or re-encoding) AND that the plugin's own state is stable.
    #[test]
    #[cfg(feature = "clap")]
    fn save_load_state_round_trips_byte_exact() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("rt_state_clap", SampleFormat::Float32);

        let first = save_state_bytes(&mut s);
        assert_state_loaded(load_state_bytes(&mut s, &first));
        let second = save_state_bytes(&mut s);

        assert_eq!(
            first,
            second,
            "state blob changed across save → load → save \
             (len {} vs {}) — the IPC path or plugin mutated it",
            first.len(),
            second.len()
        );
    }

    #[test]
    #[cfg(feature = "clap")]
    fn set_sample_rate_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("set_sr_clap", SampleFormat::Float32);
        let r = s
            .handle(
                &main_thread(),
                HostMessage::SetSampleRate {
                    rate: tutti_plugin_types::SampleRate(96000.0),
                },
            )
            .unwrap()
            .into_reply();
        assert!(matches!(
            r,
            BridgeMessage::SampleRateSet { error: None, .. }
        ));
        assert_eq!(s.clock.sample_rate, 96000.0);
    }

    #[test]
    #[cfg(feature = "clap")]
    fn unload_loaded_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("unload_clap", SampleFormat::Float32);
        assert!(s.plugin.is_some());
        assert!(s.shm.is_some());
        let r = s.handle(&main_thread(), HostMessage::UnloadPlugin).unwrap();
        assert_none(r);
        assert!(s.plugin.is_none());
        assert!(s.shm.is_none());
    }

    #[test]
    #[cfg(feature = "clap")]
    fn process_audio_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("proc_audio_clap", SampleFormat::Float32);
        let reply = s
            .handle(
                &main_thread(),
                HostMessage::ProcessAudio(tutti_plugin::server::ProcessAudioData {
                    seq: 0,
                    num_samples: 512,
                    midi_events: MidiEventVec::new(),
                    param_changes: ParameterChanges::new(),
                    note_expression: NoteExpressionChanges::new(),
                    transport: TransportInfo::default(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .into_reply();
        assert!(
            matches!(reply, BridgeMessage::AudioProcessed { .. }),
            "unexpected reply: {reply:?}"
        );
    }

    /// The probe bundle `Plugin::load` picks the format from, as a session
    /// loads it, with a slab sized to what the plugin declared.
    #[cfg(feature = "vst3")]
    fn load_vst3_probe(name: &str) -> (Session, SlabWriter) {
        let bundle = std::path::Path::new(crate::test_utils::vst3_probe_path())
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "vst3"))
            .expect("the probe module is inside its bundle")
            .to_path_buf();
        let mut s = Session::new();
        s.handle(
            &main_thread(),
            HostMessage::LoadPlugin {
                path: bundle,
                sample_rate: tutti_plugin_types::SampleRate(48_000.0),
                block_size: 512,
                preferred_format: SampleFormat::Float32,
                shm_name: name.to_string(),
                state: None,
            },
        )
        .unwrap();
        let loaded = s.plugin.as_ref().unwrap().instance().loaded().clone();
        let layout = tutti_plugin::server::SlabLayout {
            samples_per_channel: 8192,
            format: s.clock.format,
            slots: tutti_plugin::server::RING_SLOTS as u32,
            inputs: loaded.input_widths(),
            outputs: loaded.output_widths(),
        };
        let buffer_name = format!("tutti_vst_buffer_{}_{}", name, std::process::id());
        let guard = AudioSlab::create(buffer_name.clone(), layout.clone()).unwrap();
        s.shm = Some(AudioSlab::open(buffer_name, layout).unwrap());
        (s, guard)
    }

    /// One block of `frames` silent frames through `s`'s slab, and whether it
    /// was published.
    #[cfg(any(feature = "vst3", feature = "clap"))]
    fn run_silent_block(s: &mut Session, shm: &mut SlabWriter, seq: u64, frames: usize) -> bool {
        let layout = shm.layout_ref().clone();
        for ch in 0..layout.input_channels() {
            shm.write::<f32>(seq, ch, &vec![0.0; frames]).unwrap();
        }
        shm.publish(seq);
        let reply = s
            .handle(
                &main_thread(),
                HostMessage::ProcessAudio(tutti_plugin::server::ProcessAudioData {
                    seq,
                    num_samples: frames,
                    ..Default::default()
                }),
            )
            .expect("a failed block does not end the server")
            .into_reply();
        assert!(
            matches!(reply, BridgeMessage::AudioProcessed { seq: s, .. } if s == seq),
            "{reply:?}"
        );
        shm.has_output(seq)
    }

    /// **A block the plugin fails is silence and is counted; the server goes
    /// on.** The failure used to leave `handle` as an error, which ends the
    /// server loop, so one `kResultFalse` (a preset load, a licence hiccup)
    /// read as a plugin crash.
    ///
    /// Mutation (run): restoring the `?` on the plugin's `process` in
    /// `AudioPipeline::process` (the failure returned) → the first `handle`
    /// errors → fails.
    #[test]
    #[cfg(feature = "vst3")]
    fn a_vst3_block_the_plugin_fails_is_silence_and_the_server_goes_on() {
        let _lock = crate::test_utils::plugin_load_lock();
        let failing = {
            // `plugin_load_lock` is held, as every loader test that reads this
            // variable holds it. 4 is `kMisbehaveProcessFails`.
            let mut env = tutti_fixtures::ScopedEnv::new();
            env.set("TUTTI_PROBE_MISBEHAVIOUR", "4");
            let (mut s, mut shm) = load_vst3_probe("fails_vst3");
            drop(env);
            for seq in 1..=3 {
                assert!(
                    !run_silent_block(&mut s, &mut shm, seq, 128),
                    "a failed block publishes nothing, so the host reads silence"
                );
            }
            assert_eq!(s.pipeline.failed_blocks(), 3);
            assert!(s.drain_async_events(&main_thread()).is_empty());
            s.pipeline.failed_blocks()
        };
        assert_eq!(failing, 3);
        // The same server code with a plugin that behaves renders and publishes.
        let (mut s, mut shm) = load_vst3_probe("renders_vst3");
        assert!(run_silent_block(&mut s, &mut shm, 1, 128));
        assert_eq!(s.pipeline.failed_blocks(), 0);
    }

    /// The same for CLAP: a block the plugin fails (`CLAP_PROCESS_ERROR`) is
    /// silence and is counted, and the next block renders.
    ///
    /// The failure used to be provoked with a block longer than the activated
    /// maximum, which the CLAP host refused (`BlockTooLarge`). The server now
    /// refuses such a block before any plugin sees it (doc 027 D12,
    /// `a_block_of_a_length_the_plugin_cannot_take_never_reaches_it`), so the
    /// plugin fails here on its own account, through the probe's status.
    ///
    /// Mutation (run): drop `note_failed_block` in the pipeline → the count
    /// stays 0 → fails.
    #[test]
    #[cfg(feature = "clap")]
    fn a_clap_block_the_plugin_fails_is_silence_and_the_server_goes_on() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, mut shm) = load_clap("fails_clap", SampleFormat::Float32);
        {
            let _error = crate::test_utils::clap_probe_status_mode(
                tutti_clap_test_plugin::StatusMode::Error,
            );
            assert!(!run_silent_block(&mut s, &mut shm, 1, 64));
            assert_eq!(s.pipeline.failed_blocks(), 1);
        }
        assert!(
            run_silent_block(&mut s, &mut shm, 2, 64),
            "the next block renders"
        );
        assert_eq!(s.pipeline.failed_blocks(), 1);
    }

    /// **An idle dispatch allocates nothing**: the drain every message is
    /// followed by, with the plugin having asked for nothing. The lifecycle is
    /// not entered, so nothing is cloned or queued on the render thread.
    ///
    /// Mutation (run): the bridge cloning its `State` for each `drive` again
    /// together with `serve` driving a tick on every pass aborts inside the
    /// gate. Neither alone does: without the clone an idle tick allocates
    /// nothing either, so the quiet check is the second line of defence, not
    /// the first.
    #[test]
    #[cfg(feature = "vst3")]
    fn an_idle_drain_does_not_allocate_vst3() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_vst3_probe("idle_vst3");
        // Warm: the first pass sizes the bridge's request buffer.
        assert!(s.drain_async_events(&main_thread()).is_empty());
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..64 {
                assert!(s.drain_async_events(&main_thread()).is_empty());
            }
        });
    }

    /// As above, for CLAP.
    #[test]
    #[cfg(feature = "clap")]
    fn an_idle_drain_does_not_allocate_clap() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("idle_clap", SampleFormat::Float32);
        assert!(s.drain_async_events(&main_thread()).is_empty());
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..64 {
                assert!(s.drain_async_events(&main_thread()).is_empty());
            }
        });
    }

    /// As above, for VST2: the plugin asked for nothing and moved no parameter,
    /// so the drain (its request pass and the editor-knob queue) allocates
    /// nothing.
    ///
    /// Mutation (run): the staged probe named `.dylib` (the macOS cdylib
    /// extension) -> `Plugin::open` refuses the format -> fails.
    /// Mutation (run): the bridge cloning its `State` for each `drive` again
    /// together with `serve` driving a tick on every pass aborts inside the
    /// gate; and `poll` collecting the plugin's parameter moves into a
    /// `Vec::with_capacity(1)` aborts it too.
    #[test]
    #[cfg(feature = "vst2")]
    fn an_idle_drain_does_not_allocate_vst2() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut s = Session::new();
        s.handle(
            &main_thread(),
            HostMessage::LoadPlugin {
                path: crate::test_utils::vst2_probe_staged_path().to_path_buf(),
                sample_rate: tutti_plugin_types::SampleRate(48_000.0),
                block_size: 512,
                preferred_format: SampleFormat::Float32,
                shm_name: "idle_vst2".to_string(),
                state: None,
            },
        )
        .unwrap();
        assert!(s.drain_async_events(&main_thread()).is_empty());
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..64 {
                assert!(s.drain_async_events(&main_thread()).is_empty());
            }
        });
    }

    /// A plugin that says its parameter list changed gets its parameter
    /// output re-reserved by `drain_async_events`, at the plugin's count.
    ///
    /// The probe's list never really grows, so the output is first shrunk
    /// to one queue, standing in for a load-time count the plugin has since
    /// outgrown; the probe's `restartComponent(kParamTitlesChanged)` is raised
    /// through the host's own handler, as a plugin raises it.
    ///
    /// Mutations (run), each fails: deleting the `if events.iter().any(..)`
    /// block in `drain_async_events` → 1 queue; matching only `Reloaded`
    /// → 1 queue; refitting to a count of 0 → 1 queue.
    #[test]
    #[cfg(feature = "vst3")]
    fn a_param_titles_change_reserves_the_parameter_output_again() {
        // `RestartFlags_::kParamTitlesChanged` (`ivsteditcontroller.h`). The
        // `PluginParamTitlesChanged` assertion below pins that this is the bit.
        const K_PARAM_TITLES_CHANGED: i32 = 1 << 4;
        let _lock = crate::test_utils::plugin_load_lock();
        // `Plugin::load` picks the format by the `.vst3` bundle, not the
        // module inside it that `vst3_probe_path` names.
        let bundle = std::path::Path::new(crate::test_utils::vst3_probe_path())
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "vst3"))
            .expect("the probe module is inside its bundle")
            .to_path_buf();
        let mut s = Session::new();
        s.handle(
            &main_thread(),
            HostMessage::LoadPlugin {
                path: bundle,
                sample_rate: tutti_plugin_types::SampleRate(48_000.0),
                block_size: 512,
                preferred_format: SampleFormat::Float32,
                shm_name: "refit_vst3".to_string(),
                state: None,
            },
        )
        .unwrap();
        let params = s
            .plugin
            .as_ref()
            .unwrap()
            .instance()
            .parameter_count(&main_thread());
        assert!(params > 1, "the probe has parameters to refit to");
        assert_eq!(s.pipeline.param_queues(), params, "reserved at load");

        s.pipeline
            .reserve_params(1, ParameterQueue::chunk_room(512));
        assert!(
            s.drain_async_events(&main_thread()).is_empty(),
            "nothing raised yet"
        );
        assert_eq!(s.pipeline.param_queues(), 1);

        match s.plugin.as_ref().unwrap() {
            Plugin::Vst3(p) => p.plugin_restart_component(K_PARAM_TITLES_CHANGED),
            #[allow(unreachable_patterns)]
            _ => unreachable!("the probe is a VST3"),
        }
        let messages = s.drain_async_events(&main_thread());
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, BridgeMessage::PluginParamTitlesChanged)),
            "the restart reaches the session: {messages:?}"
        );
        assert_eq!(s.pipeline.param_queues(), params);
    }

    /// The format's count query, which the load and the refit reserve by,
    /// counts the parameters the list carries (the probes describe every
    /// one, so the two agree exactly).
    ///
    /// Mutations (run), each fails: the CLAP override returning 0; the
    /// VST3 override returning 0.
    #[test]
    #[cfg(all(feature = "clap", feature = "vst3"))]
    fn the_parameter_count_agrees_with_the_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (s, _shm) = load_clap("count_clap", SampleFormat::Float32);
        let clap = s.plugin.as_ref().unwrap().instance();
        assert!(clap.parameter_count(&main_thread()) > 0);
        assert_eq!(
            clap.parameter_count(&main_thread()),
            clap.get_parameter_list(&main_thread()).len()
        );

        let vst3 = crate::loaders::vst3::Vst3Instance::load(
            &main_thread(),
            std::path::Path::new(crate::test_utils::vst3_probe_path()),
            48_000.0,
            128,
            false,
        )
        .expect("the vendored VST3 probe loads");
        let vst3: &dyn tutti_plugin::server::PluginInstance = &vst3;
        assert!(vst3.parameter_count(&main_thread()) > 0);
        assert_eq!(
            vst3.parameter_count(&main_thread()),
            vst3.get_parameter_list(&main_thread()).len()
        );
    }
}
