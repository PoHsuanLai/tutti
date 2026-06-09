//! Active processing state: a [`Vst3Loaded`] with scratch buffers and the
//! `setActive(1)` + `setProcessing(1)` lifecycle.
//!
//! All non-processing methods (parameters, editor, state, latency) are
//! inherited from [`Vst3Loaded`] via [`Deref`] / [`DerefMut`] — see
//! [`crate::host::loaded`] for that surface.
//!
//! To create one: `Vst3Instance::load(path, rate, block)` or
//! `Vst3Loaded::load(path)?.activate(rate, block)?`. To drop back to
//! non-processing state: [`Vst3Instance::deactivate`].

use std::ops::{Deref, DerefMut};
use std::path::Path;

use smallvec::SmallVec;
use vst3::Steinberg::{
    kResultFalse, kResultOk,
    Vst::{
        IAudioProcessorTrait, IComponentTrait, MediaTypes_::kEvent, ProcessModes_::kRealtime,
        ProcessSetup,
    },
};

use crate::com::{event_list_ptr, param_changes_ptr, EventList, ParameterChangesImpl};
use crate::error::{LoadStage, Result, Vst3Error};
use crate::types::{
    to_process_context, AudioBuffer, BufferPtrs, ChordValue, MidiEvent, NoteExpressionIntValue,
    NoteExpressionText, NoteExpressionValue, ParameterChanges, PluginInfo, ProcessOutputRef,
    ScaleValue, TransportInfo, Vst3Sample,
};

use super::bus_buffers::{BusBuffers, DirectionScratch};
use super::loaded::Vst3Loaded;
use super::midi_mapping::{CcRoute, MidiCcMapping};
use super::{IComponentExt, K_INPUT, K_OUTPUT};

pub(super) const K_EVENT: i32 = kEvent as i32;
const K_REALTIME: i32 = kRealtime as i32;

/// Pre-reserve capacity for output param-change queues. One slot per
/// distinct param_id the plugin might emit in a single block; growing
/// beyond this allocates once and then sticks.
const OUTPUT_PARAM_QUEUE_RESERVE: usize = 32;

/// Sample rate / block size / channel counts captured at activation. Read by
/// `apply_process_setup` to fill `ProcessSetup` and by `process` to size
/// `ProcessData`. Channel counts are re-synced post-activation in
/// `activate_buses` (some plugins only finalise their arrangement once active).
struct ProcessConfig {
    sample_rate: f64,
    block_size: usize,
    num_input_channels: usize,
    num_output_channels: usize,
}

/// The input half of `ProcessData`: per-bus audio scratch plus the COM-wrapped
/// event and parameter-change lists the host stages for the plugin to read.
/// All three are reused in place each block to keep the RT path allocation-free.
struct InputStaging<T: Vst3Sample> {
    /// Per-bus `AudioBusBuffers` scratch. Bus 0 is mapped onto the live caller
    /// buffer; extra input buses receive silence.
    buses: BusBuffers<T>,
    events: vst3::ComWrapper<EventList>,
    param_changes: vst3::ComWrapper<ParameterChangesImpl>,
}

/// The output half of `ProcessData`: per-bus audio scratch, the COM-wrapped
/// lists the plugin writes into, plus the pooled buffers `process` drains those
/// emitted events into so it can return a borrowed [`ProcessOutputRef`].
struct OutputStaging<T: Vst3Sample> {
    /// Per-bus `AudioBusBuffers` scratch. Bus 0 is mapped onto the live caller
    /// buffer; extra output buses get a discard sink.
    buses: BusBuffers<T>,
    events: vst3::ComWrapper<EventList>,
    param_changes: vst3::ComWrapper<ParameterChangesImpl>,
    /// Pooled return-value buffers. `process` drains the plugin's emitted
    /// events into these so the call can return a borrowed view.
    emitted_midi: SmallVec<[MidiEvent; 64]>,
    emitted_param_changes: ParameterChanges,
}

impl<T: Vst3Sample> OutputStaging<T> {
    /// Reset the emitted-event return pools so a borrow into them reads empty.
    /// Called at the top of every `process` block: the bail-out paths return
    /// `emitted_ref()` directly, and the steady-state path then drains the
    /// plugin's fresh events in. Clears in place, keeping heap capacity.
    fn clear_emitted(&mut self) {
        self.emitted_midi.clear();
        for queue in self.emitted_param_changes.queues.iter_mut() {
            queue.points.clear();
        }
        self.emitted_param_changes.queues.clear();
    }

    /// Borrow the emitted-event return pools as a [`ProcessOutputRef`].
    fn emitted_ref(&self) -> ProcessOutputRef<'_> {
        ProcessOutputRef {
            midi_events: &self.emitted_midi,
            parameter_changes: &self.emitted_param_changes,
        }
    }

    /// Drain the plugin's emitted events from the COM output lists into the
    /// return pools. Both `fill_*` clear their destination first and reuse its
    /// heap capacity, so this is allocation-free after warmup.
    fn drain_emitted(&mut self) {
        self.events.fill_midi_events(&mut self.emitted_midi);
        self.param_changes
            .fill_changes(&mut self.emitted_param_changes);
    }
}

/// All the per-block scratch the realtime `process()` loop needs, grouped to
/// mirror `ProcessData`'s own input/output split. Separated from [`Vst3Loaded`]
/// so GUI-only hosting doesn't pay the allocation cost.
///
/// `ptrs` is shared: its flat per-channel pointers feed both `input.buses` and
/// `output.buses` each block.
struct AudioIO<T: Vst3Sample> {
    config: ProcessConfig,
    /// Typed flat pointer tables for the single committed sample format `T`.
    /// Sized to the per-direction channel total (main + sidechain/aux buses).
    ptrs: BufferPtrs<T>,
    input: InputStaging<T>,
    output: OutputStaging<T>,
    cc: CcRoute,
}

/// Fully-active VST3 plugin ready to process audio.
///
/// The type parameter `T` fixes the sample format at activation time:
/// `Vst3Instance<f32>` (the default) always calls `setupProcessing` with
/// `kSample32`; `Vst3Instance<f64>` uses `kSample64` and returns an error from
/// [`Vst3Loaded::activate`] if the plugin does not advertise 64-bit support.
///
/// Embeds a [`Vst3Loaded`]; all parameter, editor, state, and metadata methods
/// are inherited via [`Deref`]. Obtain via [`Vst3Instance::load`] or
/// [`Vst3Loaded::activate`], and drop back to a non-processing
/// [`Vst3Loaded`] with [`Vst3Instance::deactivate`].
pub struct Vst3Instance<T: Vst3Sample = f32> {
    loaded: Vst3Loaded,
    audio: AudioIO<T>,
}

impl<T: Vst3Sample> Vst3Instance<T> {
    /// Lightweight metadata read: load the library, read factory and bus info,
    /// return without calling `initialize()` or `setActive()`. Safe for plugins
    /// that would otherwise pop license dialogs or hit the network during full
    /// load.
    pub fn probe(path: &Path) -> Result<PluginInfo> {
        Vst3Loaded::probe(path)
    }

    /// Load a VST3 plugin and bring it to the active processing state.
    ///
    /// For GUI-only hosting, prefer [`Vst3Loaded::load`] — it skips the
    /// `setActive(1) + setProcessing(1)` cost.
    ///
    /// # Errors
    ///
    /// See [`Vst3Loaded::load`] for load-time errors, plus
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) with
    /// [`LoadStage::Setup`] or [`LoadStage::Activation`] if the plugin rejects
    /// the requested sample rate / block size or refuses to activate.
    /// Returns [`Vst3Error::NotSupported`] if `T = f64` and the plugin does
    /// not advertise 64-bit support.
    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        let loaded = Vst3Loaded::load(path)?;
        Self::from_loaded(loaded, sample_rate, block_size)
    }

    /// Called by [`Vst3Loaded::activate`]. Runs `setupProcessing`, activates
    /// buses, calls `setActive(1)` + `setProcessing(1)`.
    pub(super) fn from_loaded(
        loaded: Vst3Loaded,
        sample_rate: f64,
        block_size: usize,
    ) -> Result<Self> {
        if T::VST3_SYMBOLIC_SIZE == crate::types::K_SAMPLE_64_INT && !loaded.info.supports_f64 {
            return Err(Vst3Error::NotSupported(
                "Plugin does not support 64-bit processing".to_string(),
            ));
        }

        let num_input_channels = loaded.info.num_inputs;
        let num_output_channels = loaded.info.num_outputs;
        // Resolve the per-direction scratch from the initial bus layout in the
        // PluginInfo snapshot. `activate_buses` re-resolves the same way from
        // the live component afterward, since some plugins only finalise their
        // arrangement once active.
        let in_scratch = DirectionScratch::<T>::resolve(
            &loaded.info.input_bus_channels,
            num_input_channels,
            block_size,
        );
        let out_scratch = DirectionScratch::<T>::resolve(
            &loaded.info.output_bus_channels,
            num_output_channels,
            block_size,
        );

        let mut emitted_param_changes = ParameterChanges::new();
        // SmallVec doesn't expose a sized constructor for inline capacity;
        // pre-reserve via grow_to_capacity-by-clear-after-push. Cheaper
        // approach: just call reserve to pump heap capacity once.
        emitted_param_changes.queues.reserve(OUTPUT_PARAM_QUEUE_RESERVE);

        let audio = AudioIO {
            config: ProcessConfig {
                sample_rate,
                block_size,
                num_input_channels,
                num_output_channels,
            },
            ptrs: BufferPtrs::new(in_scratch.ptr_count, out_scratch.ptr_count),
            input: InputStaging {
                buses: in_scratch.buses,
                events: EventList::new(),
                param_changes: ParameterChangesImpl::new_empty(),
            },
            output: OutputStaging {
                buses: out_scratch.buses,
                events: EventList::new(),
                param_changes: ParameterChangesImpl::new_empty(),
                emitted_midi: SmallVec::new(),
                emitted_param_changes,
            },
            cc: CcRoute::new(MidiCcMapping::query(loaded.interfaces.controller.as_ref())),
        };

        let mut instance = Self { loaded, audio };
        instance.apply_process_setup()?;
        instance.activate_buses()?;
        instance.set_active(true)?;
        Ok(instance)
    }

    /// Drop back to the non-processing [`Vst3Loaded`] state, reversing
    /// `setProcessing(1)` + `setActive(1)`.
    pub fn deactivate(mut self) -> Vst3Loaded {
        self.stop_processing();
        let _ = self.set_active(false);
        // Move `loaded` out without running `Vst3Instance::Drop` (which would
        // deactivate a second time).
        let loaded = unsafe { std::ptr::read(&self.loaded as *const Vst3Loaded) };
        std::mem::forget(self);
        loaded
    }

    /// Change the sample rate and re-run `setupProcessing`. Must be called
    /// only when not inside [`process`](Self::process).
    pub fn set_sample_rate(&mut self, rate: f64) -> &mut Self {
        self.audio.config.sample_rate = rate;
        let _ = self.apply_process_setup();
        self
    }

    /// Run one realtime processing block.
    ///
    /// `midi_events`, `note_expressions`, `chords`, `scales`, `expr_texts`, and
    /// `expr_ints` are staged into the plugin's input event list (sorted by
    /// `sample_offset`); chord/scale/text strings are interned into the event
    /// list's arena for the duration of the call. `param_changes` is forwarded
    /// as `inputParameterChanges`; `transport` populates `ProcessContext`. The
    /// returned [`ProcessOutput`] carries any MIDI / parameter-change events the
    /// plugin emitted (plugin-emitted legacy-MIDI-CC-out is decoded to MIDI).
    ///
    /// Falls back to an empty output if `buffer.num_samples == 0` or if the
    /// plugin returns a non-OK `tresult` (in which case `buffer.outputs` is
    /// also cleared).
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        midi_events: &[MidiEvent],
        param_changes: Option<&ParameterChanges>,
        note_expressions: &[NoteExpressionValue],
        chords: &[ChordValue],
        scales: &[ScaleValue],
        expr_texts: &[NoteExpressionText],
        expr_ints: &[NoteExpressionIntValue],
        transport: &TransportInfo,
    ) -> ProcessOutputRef<'_> {
        // Reset the return pools up front so the bail-out paths below return a
        // borrow into known-empty state, and the steady-state path drains the
        // plugin's fresh events into them.
        self.audio.output.clear_emitted();

        if buffer.num_samples == 0 {
            return self.audio.output.emitted_ref();
        }
        let processor = self.loaded.interfaces.processor.clone();

        // Fill bus-0 channel pointers from the (flat) caller buffer, then build
        // the per-bus `AudioBusBuffers` arrays: bus 0 maps onto the live
        // channels, extra input buses get silence, extra output buses a sink.
        let (input_ptrs, output_ptrs) =
            self.audio.ptrs.prepare(buffer.inputs, buffer.outputs);
        let num_input_buses = self.audio.input.buses.num_buses();
        let num_output_buses = self.audio.output.buses.num_buses();
        // SAFETY: `input_ptrs`/`output_ptrs` point at the just-filled per-channel
        // pointer arrays (length = buffer.inputs/outputs.len()), valid until the
        // `processor.process` call below; the scratch tables are pre-sized.
        let inputs_ptr = unsafe {
            self.audio.input.buses.prepare(
                input_ptrs as *const *mut std::ffi::c_void,
                buffer.inputs.len(),
                true,
            )
        };
        let outputs_ptr = unsafe {
            self.audio.output.buses.prepare(
                output_ptrs as *const *mut std::ffi::c_void,
                buffer.outputs.len(),
                false,
            )
        };

        // Route IMidiMapping-mapped CCs into parameter changes. When the plugin
        // exposes a non-empty CC→param table, `CcRoute::route` pulls mapped
        // CC/aftertouch/pitch-bend events out of the MIDI stream and merges them
        // into its scratch ParameterChanges alongside the host's automation,
        // returning the filtered MIDI + merged params to forward; otherwise the
        // caller's own inputs are forwarded untouched.
        let (effective_midi, effective_params): (&[MidiEvent], Option<&ParameterChanges>) =
            match self.audio.cc.route(midi_events, param_changes) {
                Some((midi, params)) => (midi, Some(params)),
                None => (midi_events, param_changes),
            };

        // Stage the (possibly CC-filtered) MIDI plus every other input event
        // source into the input event list, or clear it when there's nothing.
        let have_events = !effective_midi.is_empty()
            || !note_expressions.is_empty()
            || !chords.is_empty()
            || !scales.is_empty()
            || !expr_texts.is_empty()
            || !expr_ints.is_empty();
        if have_events {
            self.audio.input.events.update_from_sources(
                effective_midi,
                note_expressions,
                chords,
                scales,
                expr_texts,
                expr_ints,
            );
        } else {
            self.audio.input.events.clear();
        }
        self.audio.output.events.clear();
        let input_events_ptr = event_list_ptr(&self.audio.input.events);
        let output_events_ptr = event_list_ptr(&self.audio.output.events);

        // Refill the pooled input/output ParameterChanges wrappers in
        // place instead of building fresh ComWrappers every block.
        let have_input_params = effective_params.map(|pc| !pc.is_empty()).unwrap_or(false);
        if have_input_params {
            self.audio
                .input
                .param_changes
                .refill_from_changes(effective_params.unwrap());
        } else {
            self.audio.input.param_changes.clear_in_place();
        }
        self.audio.output.param_changes.clear_in_place();

        let mut process_context = to_process_context(transport);
        process_context.sampleRate = buffer.sample_rate;

        let mut process_data = vst3::Steinberg::Vst::ProcessData {
            processMode: K_REALTIME,
            symbolicSampleSize: T::VST3_SYMBOLIC_SIZE,
            numSamples: buffer.num_samples as i32,
            numInputs: num_input_buses as i32,
            numOutputs: num_output_buses as i32,
            inputs: inputs_ptr,
            outputs: outputs_ptr,
            inputParameterChanges: if have_input_params {
                param_changes_ptr(&self.audio.input.param_changes)
            } else {
                std::ptr::null_mut()
            },
            outputParameterChanges: param_changes_ptr(&self.audio.output.param_changes),
            inputEvents: input_events_ptr,
            outputEvents: output_events_ptr,
            processContext: &mut process_context,
        };

        let result = unsafe { processor.process(&mut process_data) };

        if result != kResultOk {
            buffer.clear_outputs();
            return self.audio.output.emitted_ref();
        }

        self.audio.output.drain_emitted();
        self.audio.output.emitted_ref()
    }

    /// Re-query the `IMidiMapping` CC→parameter table from the controller.
    /// Call this when [`RestartOutcome::midi_cc_assignment_changed`] is set.
    pub fn rebuild_midi_cc_mapping(&mut self) {
        tutti_plugin_types::assert_main_thread();
        self.audio.cc.mapping = MidiCcMapping::query(self.loaded.interfaces.controller.as_ref());
    }

    /// Tell the plugin's audio processor to idle. Safe to call repeatedly;
    /// `deactivate` and `Drop` call it during teardown.
    fn stop_processing(&mut self) {
        unsafe {
            self.loaded.interfaces.processor.setProcessing(0);
        }
    }

    fn apply_process_setup(&mut self) -> Result<()> {
        let mut setup = ProcessSetup {
            processMode: K_REALTIME,
            symbolicSampleSize: T::VST3_SYMBOLIC_SIZE,
            maxSamplesPerBlock: self.audio.config.block_size as i32,
            sampleRate: self.audio.config.sample_rate,
        };
        let result = unsafe { self.loaded.interfaces.processor.setupProcessing(&mut setup) };
        if result != kResultOk && result != kResultFalse {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Setup,
                code: result,
            });
        }
        Ok(())
    }

    fn activate_buses(&mut self) -> Result<()> {
        const K_AUDIO: i32 = super::K_AUDIO;
        let component = &self.loaded.interfaces.component;
        unsafe {
            for i in 0..component.getBusCount(K_AUDIO, K_INPUT) {
                component.activateBus(K_AUDIO, K_INPUT, i, 1);
            }
            for i in 0..component.getBusCount(K_AUDIO, K_OUTPUT) {
                component.activateBus(K_AUDIO, K_OUTPUT, i, 1);
            }
        }

        // Re-resolve the per-bus scratch from the live component — some plugins
        // only finalise their bus arrangement once activated, so the layout can
        // differ from the PluginInfo snapshot used in `from_loaded`. Setup-time
        // only; never on the audio thread.
        let num_in = component
            .audio_bus_channel_count(K_INPUT, 0)
            .unwrap_or(self.audio.config.num_input_channels);
        let num_out = component
            .audio_bus_channel_count(K_OUTPUT, 1)
            .unwrap_or(self.audio.config.num_output_channels);
        let block_size = self.audio.config.block_size;
        let in_scratch = DirectionScratch::<T>::resolve(
            &component.audio_bus_channels(K_INPUT),
            num_in,
            block_size,
        );
        let out_scratch = DirectionScratch::<T>::resolve(
            &component.audio_bus_channels(K_OUTPUT),
            num_out,
            block_size,
        );

        self.audio.config.num_input_channels = num_in;
        self.audio.config.num_output_channels = num_out;
        self.audio.ptrs.resize_inputs(in_scratch.ptr_count);
        self.audio.ptrs.resize_outputs(out_scratch.ptr_count);
        self.audio.input.buses = in_scratch.buses;
        self.audio.output.buses = out_scratch.buses;
        Ok(())
    }

    fn set_active(&mut self, active: bool) -> Result<()> {
        let flag: vst3::Steinberg::TBool = if active { 1 } else { 0 };
        let result = unsafe { self.loaded.interfaces.component.setActive(flag) };
        if result != kResultOk && result != kResultFalse {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Activation,
                code: result,
            });
        }
        if active {
            let result = unsafe { self.loaded.interfaces.processor.setProcessing(1) };
            if result != kResultOk && result != kResultFalse {
                return Err(Vst3Error::PluginError {
                    stage: LoadStage::Activation,
                    code: result,
                });
            }
        }
        Ok(())
    }
}

impl<T: Vst3Sample> Deref for Vst3Instance<T> {
    type Target = Vst3Loaded;
    fn deref(&self) -> &Vst3Loaded {
        &self.loaded
    }
}

impl<T: Vst3Sample> DerefMut for Vst3Instance<T> {
    fn deref_mut(&mut self) -> &mut Vst3Loaded {
        &mut self.loaded
    }
}

impl<T: Vst3Sample> Drop for Vst3Instance<T> {
    fn drop(&mut self) {
        self.stop_processing();
        let _ = self.set_active(false);
        // Drop order then terminates via Vst3Loaded's Drop.
    }
}
