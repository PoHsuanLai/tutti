//! The realtime half of an activation: [`Vst3Processor<T>`] at a fixed sample
//! type, and [`ProcessorAny`], the trait's `Processor`, which picks the
//! type at activation.

use std::sync::Arc;
use tutti_graph::UmpAt;

use vst3::Steinberg::Vst::ProcessSetup;

use tutti_plugin_types::{
    AudioBufferMut, Edit, EditDrain, Normalized, ParamId, PluginError, PluginIo, PluginResult,
    Processor, RtMidiEvents,
};
use tutti_rt::DrainBeat;
use tutti_types::ChannelLayout;

use crate::events::{InputEvents, Stager, drain_midi};
use crate::types::{
    AudioBuffer, ParameterChanges, ProcessContextRequirements, ProcessMode, ProcessOutputRef,
    TransportInfo, Vst3Sample, to_process_context,
};
use tutti_vst3_ffi::{AudioSide, BusBuffers, Busy, EventList, ParameterChangesImpl, ProcessIo};

use super::midi_learn::MidiLearnProducer;
use super::midi_mapping::{CcRoute, midi_to_mapped_controller, stage_input_events};
use super::sequencer::{
    SeqScratch, convert_chords_into, convert_expr_texts_into, convert_scales_into, expr_ints_of,
};
use super::shell::{Hold, Shell};

/// How many of the Controller's writes the ring holds between two blocks.
pub(super) const EDIT_CAPACITY: usize = 256;

/// The plugin's `process` (or the call that readies it) failed: the status it
/// returned, or why the host did not make the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessFailed {
    /// What failed, by name; [`StatusError::code`](crate::StatusError::code) is
    /// the raw `tresult`.
    pub error: tutti_vst3_ffi::Vst3Error,
}

impl std::fmt::Display for ProcessFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the plugin's process failed: {}", self.error)
    }
}

impl std::error::Error for ProcessFailed {}

/// Sample rate / block size / channel counts captured at activation. Read by
/// `apply_process_setup` to fill `ProcessSetup` and by `process` to size
/// `ProcessData`. Channel counts are re-synced post-activation in
/// `activate_buses` (some plugins only finalise their arrangement once active).
pub(super) struct ProcessConfig {
    pub(super) sample_rate: f64,
    pub(super) block_size: usize,
    pub(super) num_input_channels: ChannelLayout,
    pub(super) num_output_channels: ChannelLayout,
    /// The mode last handed to `setupProcessing`. This is the value
    /// `ProcessData::processMode` must agree with, so both reads go through
    /// this one field rather than two constants that could drift apart.
    pub(super) setup_mode: ProcessMode,
    /// The mode written into each block's `ProcessData`. Equal to `setup_mode`
    /// except while a live realtime↔prefetch toggle is in effect — the one
    /// divergence `ProcessSetupCheck` permits without a fresh `setupProcessing`.
    pub(super) block_mode: ProcessMode,
}

/// The input half of `ProcessData`: per-bus audio scratch plus the COM-wrapped
/// event and parameter-change lists the host stages for the plugin to read.
/// All three are reused in place each block to keep the RT path allocation-free.
pub(super) struct InputStaging {
    /// Per-bus `AudioBusBuffers` scratch. Bus 0 is mapped onto the live caller
    /// buffer; extra input buses receive silence.
    pub(super) buses: BusBuffers,
    pub(super) events: vst3::ComWrapper<EventList>,
    /// Fills `events` from a block's sources; keeps the per-note bend ranges.
    pub(super) stager: Stager,
    pub(super) param_changes: vst3::ComWrapper<ParameterChangesImpl>,
}

/// The output half of `ProcessData`: per-bus audio scratch, the COM-wrapped
/// lists the plugin writes into, plus the pooled buffers `process` drains those
/// emitted events into so it can return a borrowed [`ProcessOutputRef`].
pub(super) struct OutputStaging {
    /// Per-bus `AudioBusBuffers` scratch. Bus 0 is mapped onto the live caller
    /// buffer; extra output buses get a discard sink.
    pub(super) buses: BusBuffers,
    pub(super) events: vst3::ComWrapper<EventList>,
    pub(super) param_changes: vst3::ComWrapper<ParameterChangesImpl>,
    /// Pooled return-value buffers. `process` drains the plugin's emitted
    /// events into these so the call can return a borrowed view.
    ///
    /// Capped: the plugin decides how many events it emits, so an unbounded
    /// pool would let it provoke a `malloc` in the audio callback.
    pub(super) emitted_midi: RtMidiEvents,
    pub(super) emitted_param_changes: ParameterChanges,
}

impl OutputStaging {
    /// Resets the emitted-event return pools so a borrow into them reads empty.
    /// Called at the top of every `process` block: the bail-out paths return
    /// `emitted_ref()` directly, and the steady-state path then drains the
    /// plugin's fresh events in. Clears in place, keeping heap capacity: the
    /// param queues go back to the set's spares with their point storage.
    pub(super) fn clear_emitted(&mut self) {
        self.emitted_midi.clear();
        self.emitted_param_changes.clear();
    }

    /// Borrows the emitted-event return pools as a [`ProcessOutputRef`].
    pub(super) fn emitted_ref(&self) -> ProcessOutputRef<'_> {
        ProcessOutputRef {
            midi_events: &self.emitted_midi,
            parameter_changes: &self.emitted_param_changes,
        }
    }

    /// Drains the plugin's emitted events from the COM output lists into the
    /// return pools. Both `fill_*` clear their destination first and reuse its
    /// heap capacity, so this is allocation-free after warmup.
    pub(super) fn drain_emitted(&mut self) {
        drain_midi(&self.events, &mut self.emitted_midi);
        // A refusal is counted in `lists_busy` and leaves the pool empty.
        if !self
            .param_changes
            .try_fill_changes(&mut self.emitted_param_changes)
        {
            self.emitted_param_changes.clear();
        }
    }
}

/// All the per-block scratch the realtime `process()` loop needs, grouped to
/// mirror `ProcessData`'s own input/output split. Separated from [`Vst3Loaded`](super::Vst3Loaded)
/// so GUI-only hosting doesn't pay the allocation cost.
///
/// `ptrs` is shared: its flat per-channel pointers feed both `input.buses` and
/// `output.buses` each block.
pub(super) struct AudioIO {
    pub(super) config: ProcessConfig,
    /// Typed flat pointer tables for the single committed sample format `T`.
    /// Sized to the per-direction channel total (main + sidechain/aux buses).
    pub(super) input: InputStaging,
    pub(super) output: OutputStaging,
    pub(super) cc: CcRoute,
    /// RT-side MIDI-learn CC capture, paired with the
    /// [`Vst3Loaded`](super::Vst3Loaded)'s consumer. Inert (a single
    /// relaxed atomic load) unless learn is armed.
    pub(super) midi_learn: MidiLearnProducer,
}

impl ProcessConfig {
    /// The `ProcessSetup` describing this activation.
    ///
    /// The single source of the setup values, so what `setupProcessing`
    /// receives and what the conformance observer is told cannot drift: a
    /// second, hand-built copy for the observer would agree with `ProcessData`
    /// by construction and hide exactly the mismatch the check exists to find.
    pub(super) fn process_setup(&self, symbolic_sample_size: i32) -> ProcessSetup {
        ProcessSetup {
            processMode: self.setup_mode.to_vst3(),
            symbolicSampleSize: symbolic_sample_size,
            maxSamplesPerBlock: self.block_size as i32,
            sampleRate: self.sample_rate,
        }
    }
}

/// The realtime half of an activation at sample type `T`: it renders blocks and
/// nothing else.
///
/// `Send` to move to the realtime thread, not `Sync`. It reaches the plugin only
/// through its own reference to `IAudioProcessor`, so it never touches the
/// [`Vst3Loaded`](super::Vst3Loaded) its [`Vst3Controller`](super::Vst3Controller)
/// holds. Dropping it ends its share of the activation. The plugin is destroyed
/// when the last half goes, on the control side: a Processor that is the last
/// hands the plugin to a retirement queue that
/// [`Vst3Loaded::reap_retired`](super::Vst3Loaded::reap_retired) empties, so
/// dropping it on the realtime thread does not tear a plugin down there.
pub struct Vst3Processor<T: Vst3Sample> {
    _format: std::marker::PhantomData<fn() -> T>,
    // The scratch is listed first so it drops before the rest.
    pub(super) audio: AudioIO,
    seq: SeqScratch,
    audio_side: AudioSide,
    /// What `getProcessContextRequirements` answered at activation; `None` when
    /// the plugin does not implement the interface.
    context_requirements: Option<ProcessContextRequirements>,
    drain: EditDrain,
    /// Counts the drains, for the Controller's record of what a block has taken.
    beat: DrainBeat,
    /// The writes taken for the block in hand, latest per parameter, and what
    /// a rejoin carried over from the last activation before the first block.
    /// Reserved for the ring's room, so no block grows it.
    pending: Vec<(ParamId, Normalized)>,
    /// Edits the block could not hold, since activation.
    edits_dropped: u64,
    /// Whether `setProcessing(1)` is the plugin's last word.
    processing: bool,
    /// Last: the plugin outlives everything above, and a last holder retires
    /// it instead of destroying it here.
    shell: Hold,
}

impl<T: Vst3Sample> std::fmt::Debug for Vst3Processor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Processor")
            .field("block_size", &self.audio.config.block_size)
            .field("processing", &self.processing)
            .finish_non_exhaustive()
    }
}

/// Everything a Processor is built from.
pub(super) struct ProcessorParts {
    pub shell: Arc<Shell>,
    pub audio: AudioIO,
    pub audio_side: AudioSide,
    pub context_requirements: Option<ProcessContextRequirements>,
    pub drain: EditDrain,
    pub beat: DrainBeat,
    pub carried: Vec<(ParamId, Normalized)>,
}

impl<T: Vst3Sample> Vst3Processor<T> {
    pub(super) fn new(parts: ProcessorParts) -> Self {
        let ProcessorParts {
            shell,
            audio,
            audio_side,
            context_requirements,
            drain,
            beat,
            mut carried,
        } = parts;
        carried.reserve(EDIT_CAPACITY);
        Self {
            _format: std::marker::PhantomData,
            audio,
            seq: SeqScratch::default(),
            audio_side,
            context_requirements,
            drain,
            beat,
            pending: carried,
            edits_dropped: 0,
            processing: true,
            shell: Hold::new(shell),
        }
    }

    /// The shell both halves of this activation hold; `deactivate` compares it.
    pub(super) fn shell(&self) -> &Arc<Shell> {
        self.shell.shell()
    }

    /// Whether `setProcessing(1)` is the plugin's last word: true from
    /// activation, false after [`stop`](Self::stop) until the next block.
    pub fn is_processing(&self) -> bool {
        self.processing
    }

    /// Ends processing (`setProcessing(0)`) if it is on. The next
    /// [`process`](Self::process) starts it again.
    pub fn stop(&mut self) {
        if std::mem::take(&mut self.processing) {
            // A refusal (the plugin is gone or deactivated) leaves nothing to stop.
            let _ = self.audio_side.set_processing(false);
            // The plugin forgets its input across the stop: a SysEx open in the
            // input list must not complete on the far side of it.
            self.audio.input.events.reset_sysex();
        }
    }

    /// Takes the parameter writes the Controller queued that no block has
    /// drained, and those a rejoin carried in, oldest first, into `out`. Main
    /// thread, with the Processor held (nothing is rendering): for a caller
    /// that must not lose them, as a rejoin must not.
    pub fn take_edits(&mut self, out: &mut Vec<(ParamId, Normalized)>) {
        self.drain_ring();
        out.append(&mut self.pending);
    }

    /// Writes the ring held that no block took, since activation, for want of
    /// room in the block in hand.
    #[must_use]
    pub fn edits_dropped(&self) -> u64 {
        self.edits_dropped
    }

    /// Moves what the ring holds into `pending`, the latest write to each
    /// parameter winning. Allocation-free: `pending` is reserved for the ring.
    fn drain_ring(&mut self) {
        let Self {
            drain,
            pending,
            edits_dropped,
            ..
        } = self;
        drain.drain(|edit| {
            let (id, value) = match edit {
                Edit::Param { id, value } => (id, value),
                // What came before is stale; what came after is not.
                Edit::Reset => {
                    pending.clear();
                    return;
                }
                _ => return,
            };
            if let Some(slot) = pending.iter_mut().find(|(waiting, _)| *waiting == id) {
                slot.1 = value;
            } else if pending.len() < pending.capacity() {
                pending.push((id, value));
            } else {
                *edits_dropped += 1;
            }
        });
    }

    /// Runs one realtime processing block.
    ///
    /// `events` bundles the MIDI and per-note-expressive streams staged into the
    /// plugin's input event list (sorted by `sample_offset`); chord/scale/text
    /// strings are interned into the event list's arena for the duration of the
    /// call. `param_changes` is forwarded as `inputParameterChanges`;
    /// `transport` populates `ProcessContext`. The returned
    /// [`ProcessOutput`](crate::types::ProcessOutput)
    /// carries any MIDI / parameter-change events the plugin emitted
    /// (plugin-emitted legacy-MIDI-CC-out is decoded to MIDI).
    ///
    /// An empty output if `buffer.num_samples == 0`.
    ///
    /// # Errors
    ///
    /// [`ProcessFailed`] when the plugin returns a non-OK `tresult`; the block's
    /// outputs are cleared and what the plugin emitted is dropped.
    ///
    /// Call from the audio thread. Buffers and event lists are sized at
    /// activation, so the steady-state path does not allocate.
    pub fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        events: &InputEvents,
        param_changes: Option<&ParameterChanges>,
        transport: &TransportInfo,
    ) -> std::result::Result<ProcessOutputRef<'_>, ProcessFailed> {
        // Reset the return pools up front so the bail-out paths below return a
        // borrow into known-empty state, and the steady-state path drains the
        // plugin's fresh events into them.
        self.audio.output.clear_emitted();

        if buffer.num_samples == 0 {
            return Ok(self.audio.output.emitted_ref());
        }
        // Processing resumes lazily after a `stop`. (Activation starts it.)
        if !self.processing {
            // `kNotImplemented` is a plugin that has no use for the call, not a
            // failure; any other refusal fails the block (outputs cleared), and
            // `processing` stays off so the next block asks again.
            match self.audio_side.set_processing(true) {
                Ok(()) | Err(tutti_vst3_ffi::Vst3Error::NotImplemented) => self.processing = true,
                Err(error) => {
                    buffer.clear_outputs();
                    return Err(ProcessFailed { error });
                }
            }
        }
        // The Controller's writes since the last block, put ahead of this
        // block's automation below, so where both set one parameter the
        // automation has the last word.
        self.beat.begin();
        self.drain_ring();

        // MIDI learn (IMidiLearn): when armed, capture each incoming controller
        // from the *raw* MIDI — before CC routing can divert mapped CCs into
        // parameter changes — so the plugin can learn even controllers that have
        // no mapping yet. A single relaxed atomic load when disarmed (the norm).
        self.capture_midi_learn(events.midi);

        // Route IMidiMapping-mapped CCs into parameter changes (when the
        // plugin exposes a non-empty CC→param table) and stage the filtered
        // MIDI plus every other input event source into the input list.
        let Some(effective_params) = stage_input_events(
            &mut self.audio.cc,
            &mut self.audio.input.stager,
            &self.audio.input.events,
            events,
            param_changes,
        ) else {
            // A plugin thread is inside the input list, which therefore still
            // holds the last block's events: processing now would replay them.
            // The block is silence, nothing is emitted, and the skip is counted
            // (`lists_busy`). The controller's writes wait in `pending`.
            buffer.clear_outputs();
            return Ok(self.audio.output.emitted_ref());
        };
        if !self.audio.output.events.clear() {
            // The output list is busy the same way: the plugin would append to
            // the last block's output, and the host would read it twice.
            buffer.clear_outputs();
            return Ok(self.audio.output.emitted_ref());
        }

        // Refill the pooled input/output ParameterChanges wrappers in
        // place instead of building fresh ComWrappers every block. A list a
        // plugin thread still holds is skipped like a busy event list: the
        // block is silence, nothing is emitted, and the skip is counted
        // (`lists_busy`). The controller's writes wait in `pending`.
        let input_params = effective_params.filter(|pc| !pc.is_empty());
        let have_input_params = input_params.is_some() || !self.pending.is_empty();
        let staged = (|| {
            let mut staging = self.audio.input.param_changes.try_begin().ok_or(Busy)?;
            if let Some(changes) = input_params {
                staging.refill(changes)?;
            }
            for &(id, value) in &self.pending {
                staging.prepend(id, value)?;
            }
            // The output list must be empty before the plugin appends to it.
            drop(staging);
            let _ = self.audio.output.param_changes.try_begin().ok_or(Busy)?;
            Ok::<(), Busy>(())
        })();
        if staged.is_err() {
            buffer.clear_outputs();
            return Ok(self.audio.output.emitted_ref());
        }
        self.pending.clear();

        let mut process_context = to_process_context(
            transport,
            self.context_requirements
                .unwrap_or(ProcessContextRequirements::all()),
        );
        process_context.sampleRate = buffer.sample_rate;

        // The setup comes from `process_setup()`, the same accessor
        // `apply_process_setup` hands the plugin, so a conformance observer
        // comparing the two sees the real negotiated value.
        let setup = self.audio.config.process_setup(T::VST3_SYMBOLIC_SIZE);

        // Bind each direction to this block's channel slices: bus 0 maps onto
        // the live channels, extra input buses get silence, extra output buses
        // a sink. Channels beyond the scratch's flat width are dropped.
        let bound = T::bind_inputs(&mut self.audio.input.buses, buffer.inputs).and_then(|inputs| {
            T::bind_outputs(&mut self.audio.output.buses, buffer.outputs)
                .map(|outputs| (inputs, outputs))
        });
        let (inputs, outputs) = match bound {
            Ok(buses) => buses,
            Err(error) => {
                buffer.clear_outputs();
                return Err(ProcessFailed { error });
            }
        };
        let result = self.audio_side.process(ProcessIo {
            mode: self.audio.config.block_mode,
            num_samples: buffer.num_samples,
            inputs,
            outputs,
            input_events: &self.audio.input.events,
            output_events: &self.audio.output.events,
            input_params: have_input_params.then_some(&self.audio.input.param_changes),
            output_params: &self.audio.output.param_changes,
            context: &mut process_context,
            setup: &setup,
        });

        if let Err(error) = result {
            buffer.clear_outputs();
            return Err(ProcessFailed { error });
        }

        self.audio.output.drain_emitted();
        Ok(self.audio.output.emitted_ref())
    }

    /// Feed incoming MIDI CC controllers to the MIDI-learn capture (for
    /// `IMidiLearn`). When learn is disarmed — the common case — this is a single
    /// relaxed atomic load and an immediate return, so the hot path pays almost
    /// nothing. When armed, each mappable controller (CC / channel-pressure /
    /// pitch-bend) is decoded to its VST3 `(channel, controller)` and captured
    /// allocation-free; the value is irrelevant to learning and dropped.
    #[inline]
    fn capture_midi_learn(&self, midi_events: &[UmpAt]) {
        // Cheap bail when disarmed (the norm): avoid even decoding the MIDI.
        if !self.audio.midi_learn.is_armed() {
            return;
        }
        for event in midi_events {
            if let Some((channel, controller, _value)) = midi_to_mapped_controller(&event.1) {
                self.audio.midi_learn.capture(channel, controller);
            }
        }
    }

    /// Blocks skipped (rendered as silence, nothing emitted) or lists left
    /// unread since activation because a plugin thread was inside a call on
    /// the input or output event list (`getEventCount`, `getEvent`,
    /// `addEvent`) or parameter list or one of its queues (`getParameterData`,
    /// `getPoint`, `addPoint`, ...) when the audio thread needed it. Zero for
    /// a plugin that keeps to the threading rules; the host never waits on
    /// the plugin.
    pub fn lists_busy(&self) -> u64 {
        self.audio.input.events.busy()
            + self.audio.output.events.busy()
            + self.audio.input.param_changes.busy()
            + self.audio.output.param_changes.busy()
    }

    /// The input parameter list, shared: a conformance test holds one of its
    /// queues
    /// ([`ParameterChangesImpl::hold_queue_as_plugin`](tutti_vst3_ffi::ParameterChangesImpl::hold_queue_as_plugin))
    /// as a plugin worker thread inside `getPoint` does, while the processor
    /// renders.
    #[cfg(feature = "conformance")]
    pub fn input_param_list(&self) -> vst3::ComWrapper<tutti_vst3_ffi::ParameterChangesImpl> {
        self.audio.input.param_changes.clone()
    }

    /// The input event list, shared: a conformance test holds it
    /// ([`EventList::hold_as_plugin`](tutti_vst3_ffi::EventList::hold_as_plugin))
    /// as a plugin thread inside `getEvent` does, while the processor renders.
    #[cfg(feature = "conformance")]
    pub fn input_event_list(&self) -> vst3::ComWrapper<tutti_vst3_ffi::EventList> {
        self.audio.input.events.clone()
    }

    /// Texts (chord, scale or note-expression text) dropped since activation
    /// because the block's event-list text arena was full, input and output
    /// lists together. Each list reserves its arena once and never grows it,
    /// because the plugin keeps pointers into it for the whole `process`
    /// call; a text past the room is staged as an empty text and counted here.
    pub fn event_text_dropped(&self) -> usize {
        self.audio.input.events.text_dropped() + self.audio.output.events.text_dropped()
    }

    /// Input events not handed to the plugin since activation because the
    /// block staged more than the input event list's reserved room (MIDI,
    /// note expressions, chords, scales and per-note texts together). The
    /// list never grows on the audio thread; past its room a note-off still
    /// takes the place of an earlier event, so no note is left hanging
    /// unless the list is full of note-offs, and every other event is
    /// dropped. Per-note expression values past their own budget (a MIDI
    /// block's worth) are dropped and counted here too. Each lost event is counted once. Zero
    /// while a block stays within the room. Read on this instance only, like
    /// [`param_points_dropped`](Self::param_points_dropped).
    pub fn events_dropped(&self) -> u64 {
        self.audio.input.events.dropped()
    }

    /// SysEx messages not handed to the plugin since this Processor was built
    /// (an activation builds a new one, so a re-activation starts at zero),
    /// each dropped whole rather than truncated: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY),
    /// left without their `END` by a later `START`, open when the plugin was
    /// stopped ([`stop`](Self::stop)), or cut by more than a block's MIDI cap routed through the
    /// plugin's CC mapping. The input list
    /// reassembles a multi-packet SysEx (across blocks if it spans them) and
    /// hands the plugin one complete `0xF0 … 0xF7` message at its `END`.
    /// Read on this instance only, like
    /// [`events_dropped`](Self::events_dropped).
    pub fn sysex_dropped(&self) -> u64 {
        self.audio.input.events.sysex_dropped()
    }

    /// Events the plugin emitted through `addEvent` that its output list
    /// refused (with `kResultFalse`) or that gave way to a later note-off,
    /// since activation: past the room the list reserved, as for
    /// [`events_dropped`](Self::events_dropped).
    pub fn output_events_dropped(&self) -> u64 {
        self.audio.output.events.dropped()
    }

    /// Host parameter points not handed to the plugin whole since
    /// activation: the input list holds a chunk's points as the host's chunk
    /// does (a few per parameter and a shared pool of a chunk's room), and a
    /// point past a queue's room, or past its own few with the pool spent,
    /// gives way to its parameter's latest value and is counted here, as is
    /// one the `IMidiMapping` route's merge of host automation and mapped
    /// controllers could not hold. Zero while the host stays within the
    /// room the instance was activated for. Read on this instance only: the
    /// plugin-server does not report it (as it does not report the CLAP
    /// host's `param_points_dropped`).
    pub fn param_points_dropped(&self) -> u64 {
        self.audio.input.param_changes.points_dropped() + self.audio.cc.points_overflowed()
    }

    /// Points the plugin emitted that the host could not hold whole since
    /// activation: past an output queue's `MIN_POINT_ROOM` as the plugin
    /// added them (`addPoint`), or past it in the drained output set with
    /// the set's pool spent. Each time the parameter kept its latest value.
    /// Read on this instance only, like
    /// [`param_points_dropped`](Self::param_points_dropped).
    pub fn output_param_points_dropped(&self) -> u64 {
        self.audio.output.emitted_param_changes.points_overflowed()
            + self.audio.output.param_changes.points_dropped()
    }

    /// Toggles between [`Realtime`](ProcessMode::Realtime) and
    /// [`Prefetch`](ProcessMode::Prefetch) on a live instance, without
    /// re-running `setupProcessing`.
    ///
    /// This pair is the sole per-block mode change the VST3 spec allows: the
    /// `ProcessSetup`/`ProcessData` agreement rule in
    /// `ProcessSetupCheck::check` names it as an explicit exception, so only
    /// `ProcessData::processMode` moves here and the negotiated setup is left
    /// alone. Returns `false` — changing nothing — when the instance was
    /// activated [`Offline`](ProcessMode::Offline), because reaching it from
    /// either of these needs a fresh setup and the resulting disagreement would
    /// be a spec violation. Activate again with another
    /// [`Prepare::mode`](tutti_plugin_types::Prepare::mode) to change an
    /// offline instance's mode.
    ///
    /// Must not be called from inside [`process`](Self::process).
    pub fn set_prefetch(&mut self, prefetch: bool) -> bool {
        let requested = if prefetch {
            ProcessMode::Prefetch
        } else {
            ProcessMode::Realtime
        };
        // Gate on the *negotiated setup* mode, not the current block mode: it
        // is the setup value every block is checked against, and it is what an
        // offline activation pinned.
        if !self.audio.config.setup_mode.switchable_to(requested) {
            return false;
        }
        self.audio.config.block_mode = requested;
        true
    }

    /// The mode this instance's blocks are currently processed in.
    pub fn process_mode(&self) -> ProcessMode {
        self.audio.config.block_mode
    }
}

/// The trait's Processor: the sample type is chosen at activation, so the two
/// typed processors sit in one enum and `process` matches the buffer's width to
/// the arm.
#[derive(Debug)]
pub enum ProcessorAny {
    /// Activated at 32-bit.
    F32(Vst3Processor<f32>),
    /// Activated at 64-bit; the plugin said it can process it.
    F64(Vst3Processor<f64>),
}

impl ProcessorAny {
    pub(super) fn shell(&self) -> &Arc<Shell> {
        match self {
            Self::F32(p) => p.shell(),
            Self::F64(p) => p.shell(),
        }
    }

    /// [`Vst3Processor::is_processing`].
    pub fn is_processing(&self) -> bool {
        match self {
            Self::F32(p) => p.is_processing(),
            Self::F64(p) => p.is_processing(),
        }
    }

    /// [`Vst3Processor::take_edits`].
    pub fn take_edits(&mut self, out: &mut Vec<(ParamId, Normalized)>) {
        match self {
            Self::F32(p) => p.take_edits(out),
            Self::F64(p) => p.take_edits(out),
        }
    }

    /// Whether this processor renders 64-bit audio.
    #[must_use]
    pub fn is_f64(&self) -> bool {
        matches!(self, Self::F64(_))
    }

    /// Renders one block, with the buffers, the context and the output taken
    /// apart. [`Processor::process`] is this with a [`PluginIo`].
    ///
    /// # Errors
    ///
    /// [`PluginError::Block`] when the plugin failed the block or the buffer's
    /// sample format is not the one the processor was activated with. Neither
    /// allocates.
    pub fn render(
        &mut self,
        buffer: &mut AudioBufferMut<'_, '_>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        match (buffer, self) {
            (AudioBufferMut::F32(buf), Self::F32(p)) => p.process_shared(buf, ctx, out),
            (AudioBufferMut::F64(buf), Self::F64(p)) => p.process_shared(buf, ctx, out),
            (AudioBufferMut::F32(_), Self::F64(_)) | (AudioBufferMut::F64(_), Self::F32(_)) => {
                Err(PluginError::Block(
                    "the block's sample format is not the one the plugin was activated with",
                ))
            }
        }
    }
}

impl Processor for ProcessorAny {
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
        let PluginIo { buffer, ctx, out } = io;
        self.render(buffer, ctx, out)
    }

    fn stop(&mut self) {
        match self {
            Self::F32(p) => p.stop(),
            Self::F64(p) => p.stop(),
        }
    }

    /// `setProcessing(false)`, started again by the next block: what the SDK
    /// offers for a reset (`ivstaudioprocessor.h`: it "could be used to reset
    /// some buffers (like Delay line or Reverb)"), with the SysEx reassembly
    /// dropped as [`stop`](Vst3Processor::stop) drops it.
    fn reset(&mut self) {
        Processor::stop(self);
    }
}

impl<T: Vst3Sample> Vst3Processor<T> {
    /// One block from the shared vocabulary: converts the context into VST3's
    /// (the sequencer streams into scratch, transport on the stack), processes,
    /// and copies what the plugin emitted into `out`.
    fn process_shared(
        &mut self,
        buffer: &mut AudioBuffer<'_, '_, T>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        let transport = ctx.transport.cloned().unwrap_or_default();
        // The shared and VST3 note-expression values are one type, so this is a
        // borrow, not a conversion.
        let note_expressions = ctx
            .note_expression
            .map(|n| n.changes.as_slice())
            .unwrap_or_default();
        // Refilled into `seq` rather than rebuilt: each entry carries a UTF-16
        // `text` buffer, so building them fresh allocated once per event. An
        // absent stream truncates its buffer to empty, which is what the plugin
        // should see.
        let expressive = ctx.expressive.as_ref();
        let mut seq = std::mem::take(&mut self.seq);
        match expressive.and_then(|e| e.chords) {
            Some(c) => convert_chords_into(c, &mut seq.chords),
            None => seq.chords.clear(),
        }
        match expressive.and_then(|e| e.scales) {
            Some(s) => convert_scales_into(s, &mut seq.scales),
            None => seq.scales.clear(),
        }
        match expressive.and_then(|e| e.expr_texts) {
            Some(t) => convert_expr_texts_into(t, &mut seq.expr_texts),
            None => seq.expr_texts.clear(),
        }
        let events = InputEvents {
            midi: ctx.midi_events,
            note_expressions,
            chords: &seq.chords,
            scales: &seq.scales,
            expr_texts: &seq.expr_texts,
            expr_ints: expr_ints_of(expressive),
        };
        let result = match self.process(buffer, &events, ctx.param_changes, &transport) {
            Ok(output) => {
                copy_process_output(output, out);
                Ok(())
            }
            Err(_) => Err(PluginError::Block("the plugin's process returned an error")),
        };
        self.seq = seq;
        result
    }
}

/// Copies one block's emitted events out of the host's pooled buffers into the
/// caller's reusable [`ProcessOutput`](tutti_plugin_types::ProcessOutput).
///
/// `out` arrives cleared, with its heap capacity intact, so every copy below
/// writes into storage that already exists: past its cap the MIDI is dropped
/// and counted. VST3 note-expression output is not read back (the input side
/// carries it inbound only), so `out`'s stays empty.
fn copy_process_output(output: ProcessOutputRef<'_>, out: &mut tutti_plugin_types::ProcessOutput) {
    out.emit_midi(output.midi_events.iter().copied());
    for queue in &output.parameter_changes.queues {
        for point in &queue.points {
            // `emit_param_point`, not `ParameterChanges::add_change`: the former
            // draws its queues from `out`'s own pool, so a warm block reuses the
            // `points` buffers rather than allocating one per automated
            // parameter.
            out.emit_param_point(queue.param_id, point.sample_offset, point.value.get());
        }
    }
}
