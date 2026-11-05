//! The realtime half of an activation: [`ClapProcessor`], whose sample format
//! is a value chosen at activation, and the trait's `Processor` impl on it.

use tutti_plugin_types::{
    AudioBufferMut, Edit, EditDrain, Normalized, NoteExpressionChanges, ParamId, PluginError,
    PluginIo, PluginResult, Processor, SampleFormat, TransportInfo as SharedTransport,
};
use tutti_types::Reader;

use tutti_clap_ffi::AudioSide;

use super::ClapProcessContext;
use super::audio::ProcessOutputRef;
use super::config::{AudioScratch, RangeTable};
use super::thread_pool::PoolCell;
use crate::types::{ClapNoteExpression, TransportInfo};

/// What activation hands [`ClapProcessor::new`].
pub(crate) struct ProcessorParts {
    pub audio_side: AudioSide,
    pub pool: Reader<PoolCell>,
    pub input_ports: Vec<u32>,
    pub output_ports: Vec<u32>,
    pub max_frames: u32,
    pub scratch: AudioScratch,
    pub ranges: Reader<RangeTable>,
    pub drain: EditDrain,
}

/// The realtime half of an activation: it renders blocks and nothing else. Its
/// sample [`format`](Self::format) is a value chosen at activation.
///
/// `Send` to move to the realtime thread, not `Sync`. It holds the audio half
/// of the plugin ([`AudioSide`]), which can call only the audio-thread entries
/// and takes the `[audio-thread]` claim for every call, so it never touches the
/// `ClapLoaded` its [`ClapController`](super::ClapController) holds. Dropping it
/// releases its share of the activation and calls nothing in the plugin.
pub struct ClapProcessor {
    pub(crate) scratch: AudioScratch,
    pub(crate) ranges: Reader<RangeTable>,
    pub(crate) drain: EditDrain,
    pub(crate) audio_side: AudioSide,
    pub(crate) pool: Reader<PoolCell>,
    pub(crate) input_ports: Vec<u32>,
    pub(crate) output_ports: Vec<u32>,
    pub(crate) max_frames: u32,
}

impl std::fmt::Debug for ClapProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapProcessor")
            .field("format", &self.format())
            .field("max_frames", &self.max_frames)
            .field("input_ports", &self.input_ports)
            .field("output_ports", &self.output_ports)
            .finish_non_exhaustive()
    }
}

impl ClapProcessor {
    pub(crate) fn new(parts: ProcessorParts) -> Self {
        let ProcessorParts {
            audio_side,
            pool,
            input_ports,
            output_ports,
            max_frames,
            scratch,
            ranges,
            drain,
        } = parts;
        Self {
            scratch,
            ranges,
            drain,
            audio_side,
            pool,
            input_ports,
            output_ports,
            max_frames,
        }
    }

    /// The audio half this activation holds; `deactivate` checks it belongs to
    /// the Controller's plugin.
    pub(crate) fn audio_side(&self) -> &AudioSide {
        &self.audio_side
    }

    /// Whether `start_processing` has succeeded and no `stop_processing` has
    /// followed. Distinct from *active*: an activated instance is not
    /// processing until the first `process` call self-starts it.
    pub fn is_processing(&self) -> bool {
        self.audio_side.is_processing()
    }

    /// Clears the plugin's processing state — buffers, filters, oscillators,
    /// envelopes, LFOs — and kills its voices. Parameter values are unchanged.
    ///
    /// Call this on any playback discontinuity the host creates: a locate, a
    /// loop wrap, a punch. Without it a reverb tail, a ringing filter or a hung
    /// voice bleeds across the jump into material it never belonged to.
    ///
    /// Also resets the `steady_time` counter this instance passes to
    /// `clap_process`. CLAP permits `steady_time` to jump backward **because**
    /// `reset` was called, so the two belong in one call; `stop_processing`
    /// zeroes it for the same reason, at the other discontinuity a host makes.
    ///
    /// # Threading
    /// CLAP marks `reset` `[audio-thread & active]`. The `[audio-thread]` half
    /// is taken here rather than asserted: the call runs under an
    /// [`AudioThreadClaim`](crate::host::AudioThreadClaim), so the calling OS
    /// thread becomes the audio thread for its duration (which the spec permits
    /// for any thread) and blocks until any in-flight `process` has returned.
    pub fn reset(&mut self) {
        // Under the audio claim, which the FFI half takes; a retired instance
        // (the Controller is gone) is left alone.
        if self.audio_side.is_retired() {
            return;
        }
        self.audio_side.reset();
        self.scratch.steady_time = 0;
        // The plugin forgot its input: a SysEx open across the reset must not
        // complete on the far side of it.
        self.scratch.input_events.reset_sysex();
    }

    /// Takes the parameter edits the Controller queued that no block has
    /// drained, oldest first, into `out`. Main thread, with the Processor held
    /// (nothing is rendering): for a caller that must not lose them, as a
    /// rejoin must not.
    pub fn take_edits(&mut self, out: &mut Vec<(ParamId, Normalized)>) {
        self.drain.drain(|edit| match edit {
            Edit::Param { id, value } => out.push((id, value)),
            Edit::Reset => out.clear(),
            _ => {}
        });
    }

    /// Ends processing (`stop_processing`) if it started, under the audio
    /// claim. The next `process` starts it again.
    pub fn stop(&mut self) {
        let was_processing = self.audio_side.is_processing();
        self.audio_side.stop();
        // The CLAP steady_time counter is per start/stop cycle — reset it so
        // the next start_processing begins the monotonic sequence at 0.
        if was_processing && !self.audio_side.is_retired() {
            self.scratch.steady_time = 0;
        }
    }
}

impl ClapProcessor {
    /// The format this processor was activated at: a value, fixed at
    /// activation, that every block's buffers are matched against.
    #[must_use]
    pub fn format(&self) -> SampleFormat {
        self.scratch.process.format()
    }

    /// Whether this processor renders 64-bit audio.
    #[must_use]
    pub fn is_f64(&self) -> bool {
        self.format() == SampleFormat::Float64
    }

    /// Renders one block, with the buffers, the context and the output taken
    /// apart. [`Processor::process`] is this with a [`PluginIo`]; a caller whose
    /// channel tables have distinct lifetimes, as `AudioBufferMut` allows, calls
    /// this one.
    ///
    /// # Errors
    ///
    /// [`PluginError::Process`] when the buffer's sample format is not the one
    /// the processor was activated with; [`PluginError::Block`] when the plugin
    /// failed the block.
    pub fn render(
        &mut self,
        buffer: &mut AudioBufferMut<'_, '_>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        let sample_rate = match buffer {
            AudioBufferMut::F32(b) => b.sample_rate,
            AudioBufferMut::F64(b) => b.sample_rate,
        };
        let width = match buffer {
            AudioBufferMut::F32(_) => SampleFormat::Float32,
            AudioBufferMut::F64(_) => SampleFormat::Float64,
        };
        if width != self.format() {
            // The message is built only on this path, which is a negotiation bug
            // upstream and not steady state.
            return Err(PluginError::Process(
                "audio buffer sample format does not match the format the CLAP \
                 plugin was activated with"
                    .to_string(),
            ));
        }
        self.process_shared(buffer, ctx, sample_rate, out)
    }

    /// One block from the shared vocabulary: converts the context into CLAP's
    /// (note expressions into scratch, transport on the stack), processes, and
    /// copies what the plugin emitted into `out`.
    fn process_shared(
        &mut self,
        buffer: &mut AudioBufferMut<'_, '_>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        sample_rate: f64,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        self.scratch.in_note_expressions.clear();
        if let Some(changes) = ctx.note_expression {
            let scratch = &mut self.scratch;
            convert_note_expressions(
                changes,
                &mut scratch.in_note_expressions,
                &mut scratch.in_note_expressions_dropped,
            );
        }
        let transport = ctx.transport.map(|t| convert_transport(t, sample_rate));
        // `process` borrows `self` for the output it returns, so the expressions
        // are moved out of the scratch for the call and put back after. A move of
        // a `Vec`'s three words, no allocation.
        let expressions = std::mem::take(&mut self.scratch.in_note_expressions);
        let clap_ctx = ClapProcessContext {
            midi: ctx.midi_events,
            // An empty list means "no automation this block"; the plugin reads
            // `None` and `Some(empty)` differently, so the mapping is kept.
            params: ctx.param_changes.filter(|p| !p.is_empty()),
            expressions: &expressions,
            transport: transport.as_ref(),
        };
        let result = self.process(buffer, &clap_ctx);
        let result = result
            .map(|output| copy_process_output(output, out))
            .map_err(|e| PluginError::Block(block_reason(&e)));
        self.scratch.in_note_expressions = expressions;
        result
    }
}

/// Why a block failed, as a constant: the audio thread builds this for every
/// errored block, so it must not allocate (`ClapError`'s own text is formatted).
fn block_reason(error: &crate::ClapError) -> &'static str {
    use crate::ClapError;
    match error {
        ClapError::PluginReturnedError => "the plugin returned CLAP_PROCESS_ERROR",
        ClapError::BlockTooLarge { .. } => "the block is longer than the activated max_frames",
        ClapError::StartProcessingFailed => "the plugin refused to start processing",
        ClapError::NotActive => "the plugin is no longer active",
        ClapError::BufferTooShort => "a channel buffer is shorter than the block",
        ClapError::FormatMismatch => "the block's sample format is not the activated one",
        ClapError::LayoutMismatch => "the port layout is larger than the activation sized for",
        _ => "the plugin failed the block",
    }
}

/// The block's note expressions, narrowed to CLAP's voice-addressed form.
///
/// The expression dimension is the shared `NoteExpressionType` on both sides;
/// only CLAP's wrapper differs (it adds port/channel/key), which `from_shared`
/// fills with defaults. Past the inline room the rest are dropped.
fn convert_note_expressions(
    changes: &NoteExpressionChanges,
    into: &mut Vec<ClapNoteExpression>,
    dropped: &mut u64,
) {
    for change in &changes.changes {
        if into.len() == into.capacity() {
            *dropped += 1;
        } else {
            into.push(ClapNoteExpression::from_shared(change));
        }
    }
}

/// CLAP's transport for the block. A non-finite or non-positive rate falls back
/// to 44.1 kHz for the seconds position.
fn convert_transport(transport: &SharedTransport, sample_rate: f64) -> TransportInfo {
    let sr = if sample_rate.is_finite() && sample_rate > 0.0 {
        sample_rate
    } else {
        44100.0
    };
    // CLAP's `song_pos_seconds`. Prefer the wire's own seconds field (the
    // plugin node's project time, `transport_source::ProjectTime`); fall back
    // to the project-time sample position for a producer that reports only
    // that.
    let seconds = if transport.position.seconds != 0.0 {
        transport.position.seconds
    } else {
        transport.position.samples.unwrap_or(0) as f64 / sr
    };
    TransportInfo::default()
        .with_playing(transport.state.playing)
        .with_recording(transport.state.recording)
        .with_tempo(transport.timing.tempo)
        .with_time_signature(transport.timing.signature)
        .with_position_beats(transport.position.quarters, seconds)
        .with_loop(
            transport.state.cycle_active,
            transport.loop_region.start_quarters,
            transport.loop_region.end_quarters,
        )
        // The bar number arrives over the wire; pass it through rather than
        // hardcoding 0, which would put every plugin at bar one.
        .with_bar(transport.bar.position_quarters, transport.bar.number)
}

/// Copy one block's emitted events out of the CLAP host's pooled buffers into
/// the caller's reusable [`ProcessOutput`](tutti_plugin_types::ProcessOutput).
///
/// `out` arrives cleared, with its heap capacity intact, so every copy and
/// `add_change` below writes into storage that already exists.
fn copy_process_output(output: ProcessOutputRef<'_>, out: &mut tutti_plugin_types::ProcessOutput) {
    out.emit_midi(output.midi_events.iter().copied());

    for queue in &output.param_changes.queues {
        for point in &queue.points {
            // Pooled queues: not `ParameterChanges::add_change`, which grows.
            out.emit_param_point(queue.param_id, point.sample_offset, point.value.get());
        }
    }

    // CLAP's note expression uses the shared `NoteExpressionType` (Pressure /
    // Expression included), so this narrows CLAP's voice-addressed value down
    // to the protocol shape without losing the dimension.
    for expr in output.note_expressions {
        out.note_expression
            .add_change(tutti_plugin_types::NoteExpressionValue {
                sample_offset: expr.sample_offset,
                note_id: expr.note_id,
                expression_type: expr.expression_type,
                value: expr.value,
            });
    }
}

impl Processor for ClapProcessor {
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
        let PluginIo { buffer, ctx, out } = io;
        self.render(buffer, ctx, out)
    }

    fn stop(&mut self) {
        ClapProcessor::stop(self);
    }
}
