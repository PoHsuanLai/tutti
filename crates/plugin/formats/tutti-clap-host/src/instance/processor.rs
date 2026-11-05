//! The realtime half of an activation: [`ClapProcessor<T>`] at a fixed sample
//! type, and [`ClapProcessorAny`], the trait's `Processor`, which picks the type
//! at activation.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_plugin_types::{
    AudioBufferMut, EditDrain, NoteExpressionChanges, PluginError, PluginIo, PluginResult,
    Processor, TransportInfo as SharedTransport,
};
use tutti_types::Reader;

use super::audio::ProcessOutputRef;
use super::config::{AudioScratch, PortLayout, RangeTable};
use super::shell::{RawPlugin, Shell};
use super::thread_pool::PoolCell;
use super::{ClapProcessContext, ClapSample};
use crate::host::HostState;
use crate::types::{AudioBuffer, ClapNoteExpression, TransportInfo};

/// Everything a Processor is built from, so the constructor takes one value.
pub(crate) struct ProcessorParts<T: ClapSample> {
    pub shell: Arc<Shell>,
    pub plugin: RawPlugin,
    pub host_state: Arc<HostState>,
    pub pool: Reader<PoolCell>,
    pub ports: PortLayout,
    pub max_frames: u32,
    pub scratch: AudioScratch<T>,
    pub ranges: Reader<RangeTable>,
    pub drain: EditDrain,
}

/// The realtime half of an activation at sample type `T`: it renders blocks and
/// nothing else.
///
/// `Send` to move to the realtime thread, not `Sync`. It reaches the plugin only
/// through its own copy of the plugin pointer and takes the `[audio-thread]`
/// role for every call, so it never touches the `ClapLoaded` its
/// [`ClapController`](super::ClapController) holds. Dropping it releases its
/// share of the activation and calls nothing in the plugin.
pub struct ClapProcessor<T: ClapSample> {
    // `scratch` is listed first so it drops before the rest: the RT buffers
    // release before the plugin can be destroyed.
    pub(crate) scratch: AudioScratch<T>,
    pub(crate) ranges: Reader<RangeTable>,
    pub(crate) drain: EditDrain,
    pub(crate) plugin: RawPlugin,
    pub(crate) host_state: Arc<HostState>,
    pub(crate) pool: Reader<PoolCell>,
    pub(crate) ports: PortLayout,
    pub(crate) max_frames: u32,
    pub(crate) shell: Arc<Shell>,
}

impl<T: ClapSample> std::fmt::Debug for ClapProcessor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapProcessor")
            .field("max_frames", &self.max_frames)
            .field("ports", &self.ports)
            .finish_non_exhaustive()
    }
}

// SAFETY: clause 2 of the invariant on `Shell`. The raw pointers in the scratch
// point into the scratch itself, and the plugin pointer is called only from the
// thread that holds the `[audio-thread]` role.
unsafe impl<T: ClapSample> Send for ClapProcessor<T> {}

impl<T: ClapSample> ClapProcessor<T> {
    pub(crate) fn new(parts: ProcessorParts<T>) -> Self {
        let ProcessorParts {
            shell,
            plugin,
            host_state,
            pool,
            ports,
            max_frames,
            scratch,
            ranges,
            drain,
        } = parts;
        Self {
            scratch,
            ranges,
            drain,
            plugin,
            host_state,
            pool,
            ports,
            max_frames,
            shell,
        }
    }

    /// The shell both halves of this activation hold; `deactivate` compares it.
    pub(crate) fn shell(&self) -> &Arc<Shell> {
        &self.shell
    }

    /// Whether `start_processing` has succeeded and no `stop_processing` has
    /// followed. Distinct from *active*: an activated instance is not
    /// processing until the first `process` call self-starts it.
    pub fn is_processing(&self) -> bool {
        self.shell.processing.load(Ordering::Relaxed)
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
        // Clone the Arc into a local so the claim borrows the local rather than
        // `self`. An Arc clone is an atomic increment, no allocation.
        let host_state = Arc::clone(&self.host_state);
        let _claim = host_state.claim_audio_thread();
        // Checked under the claim: the Controller retires before it claims.
        if self.shell.retired.load(Ordering::Acquire) {
            return;
        }

        // SAFETY: the plugin is alive (`retired` is clear) and the claim holds
        // the `[audio-thread]` role.
        let plugin_ref = unsafe { &*self.plugin.as_ptr() };
        if let Some(reset_fn) = plugin_ref.reset {
            unsafe { reset_fn(self.plugin.as_ptr()) };
        }
        self.scratch.steady_time = 0;
        // The plugin forgot its input: a SysEx open across the reset must not
        // complete on the far side of it.
        self.scratch.input_events.reset_sysex();
    }

    /// Ends processing (`stop_processing`) if it started, under a fresh
    /// [`AudioThreadClaim`](crate::host::AudioThreadClaim). The next `process`
    /// starts it again.
    pub fn stop(&mut self) {
        let host_state = Arc::clone(&self.host_state);
        let claim = host_state.claim_audio_thread();
        self.stop_processing_claimed(&claim);
    }

    /// `stop_processing` for a caller that already holds the claim.
    pub(crate) fn stop_processing_claimed(&mut self, _claim: &crate::host::AudioThreadClaim<'_>) {
        if self.shell.retired.load(Ordering::Acquire)
            || !self.shell.processing.swap(false, Ordering::Relaxed)
        {
            return;
        }
        // SAFETY: the plugin is alive and the caller holds the claim.
        let plugin_ref = unsafe { &*self.plugin.as_ptr() };
        if let Some(stop_fn) = plugin_ref.stop_processing {
            unsafe { stop_fn(self.plugin.as_ptr()) };
        }
        // The CLAP steady_time counter is per start/stop cycle — reset it so
        // the next start_processing begins the monotonic sequence at 0.
        self.scratch.steady_time = 0;
    }
}

/// The trait's Processor: the sample type is chosen at activation, so the two
/// typed processors sit in one enum and `process` matches the buffer's width to
/// the arm.
#[derive(Debug)]
pub enum ClapProcessorAny {
    /// Activated at 32-bit.
    F32(ClapProcessor<f32>),
    /// Activated at 64-bit; every port of the plugin advertises it.
    F64(ClapProcessor<f64>),
}

impl ClapProcessorAny {
    pub(crate) fn shell(&self) -> &Arc<Shell> {
        match self {
            Self::F32(p) => p.shell(),
            Self::F64(p) => p.shell(),
        }
    }

    /// Whether `start_processing` has run and `stop_processing` has not.
    pub fn is_processing(&self) -> bool {
        match self {
            Self::F32(p) => p.is_processing(),
            Self::F64(p) => p.is_processing(),
        }
    }

    /// [`ClapProcessor::reset`].
    pub fn reset(&mut self) {
        match self {
            Self::F32(p) => p.reset(),
            Self::F64(p) => p.reset(),
        }
    }

    /// Whether this processor renders 64-bit audio.
    #[must_use]
    pub fn is_f64(&self) -> bool {
        matches!(self, Self::F64(_))
    }
}

impl ClapProcessorAny {
    /// Renders one block, with the buffers, the context and the output taken
    /// apart. [`Processor::process`] is this with a [`PluginIo`]; a caller whose
    /// channel tables have distinct lifetimes, as `AudioBufferMut` allows, calls
    /// this one.
    ///
    /// # Errors
    ///
    /// [`PluginError::Process`] when the plugin failed the block or the buffer's
    /// sample format is not the one the processor was activated with.
    pub fn render(
        &mut self,
        buffer: &mut AudioBufferMut<'_, '_>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        match (buffer, self) {
            (AudioBufferMut::F32(buf), Self::F32(p)) => {
                let sample_rate = buf.sample_rate;
                let mut clap_buffer = crate::AudioBuffer32 {
                    inputs: buf.inputs,
                    outputs: buf.outputs,
                    num_samples: buf.num_samples,
                    sample_rate,
                };
                p.process_shared(&mut clap_buffer, ctx, sample_rate, out)
            }
            (AudioBufferMut::F64(buf), Self::F64(p)) => {
                let sample_rate = buf.sample_rate;
                let mut clap_buffer = crate::AudioBuffer64 {
                    inputs: buf.inputs,
                    outputs: buf.outputs,
                    num_samples: buf.num_samples,
                    sample_rate,
                };
                p.process_shared(&mut clap_buffer, ctx, sample_rate, out)
            }
            // The message is built only on this path, which is a negotiation bug
            // upstream and not steady state.
            (AudioBufferMut::F32(_), Self::F64(_)) | (AudioBufferMut::F64(_), Self::F32(_)) => {
                Err(PluginError::Process(
                    "audio buffer sample format does not match the format the CLAP \
                     plugin was activated with"
                        .to_string(),
                ))
            }
        }
    }
}

impl Processor for ClapProcessorAny {
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
}

impl<T: ClapSample> ClapProcessor<T> {
    /// One block from the shared vocabulary: converts the context into CLAP's
    /// (note expressions into scratch, transport on the stack), processes, and
    /// copies what the plugin emitted into `out`.
    fn process_shared(
        &mut self,
        buffer: &mut AudioBuffer<'_, '_, T>,
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
            .map_err(|e| PluginError::Process(e.to_string()));
        self.scratch.in_note_expressions = expressions;
        result
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
