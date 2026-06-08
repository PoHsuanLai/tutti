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
    to_process_context, AudioBuffer, BufferPtrs, MidiEvent, NoteExpressionValue, ParameterChanges,
    PluginInfo, ProcessOutputRef, TransportInfo, Vst3Sample,
};

use super::bus_buffers::{BusBuffers, MIN_PTR_COUNT};
use super::loaded::Vst3Loaded;
use super::midi_mapping::MidiCcMapping;
use super::{IComponentExt, K_INPUT, K_OUTPUT};

pub(super) const K_EVENT: i32 = kEvent as i32;
const K_REALTIME: i32 = kRealtime as i32;

/// Pre-reserve capacity for output param-change queues. One slot per
/// distinct param_id the plugin might emit in a single block; growing
/// beyond this allocates once and then sticks.
const OUTPUT_PARAM_QUEUE_RESERVE: usize = 32;

/// Scratch buffers + event lists the realtime `process()` loop needs.
/// Separated from [`Vst3Loaded`] so GUI-only hosting doesn't pay the cost.
///
/// The two `ComWrapper<ParameterChangesImpl>` slots are reused across
/// every `process()` call: we call `refill_from_changes` / `clear_in_place`
/// rather than re-building a fresh ComWrapper. That keeps the RT path
/// allocation- and lock-free.
struct AudioIO<T> {
    sample_rate: f64,
    block_size: usize,
    num_input_channels: usize,
    num_output_channels: usize,
    /// Typed pointer tables for the single committed sample format `T`.
    /// Sized to the per-direction channel total (main + sidechain/aux buses).
    ptrs: BufferPtrs<T>,
    /// Per-bus `AudioBusBuffers` scratch (one per direction; sample-type
    /// erased). Bus 0 is mapped onto the live caller buffer; extra input buses
    /// receive silence and extra output buses a discard sink.
    in_buses: BusBuffers,
    out_buses: BusBuffers,
    input_events: vst3::ComWrapper<EventList>,
    output_events: vst3::ComWrapper<EventList>,
    input_param_changes: vst3::ComWrapper<ParameterChangesImpl>,
    output_param_changes: vst3::ComWrapper<ParameterChangesImpl>,
    /// Pooled return-value buffers. `process` writes converted events
    /// into these so the call can return a borrowed view.
    out_midi: SmallVec<[MidiEvent; 64]>,
    out_param_changes: ParameterChanges,
    /// Pooled scratch for the `IMidiMapping` CC→param routing pass. When the
    /// plugin has a non-empty mapping, mapped CC/aftertouch/pitch-bend events
    /// are pulled out of `midi_events` into `cc_filtered_midi` (the events that
    /// still go to the plugin's event list) while their parameter points are
    /// merged into `cc_param_changes` alongside the host's automation. Both
    /// are reused across blocks to keep the routing pass allocation-free.
    cc_filtered_midi: SmallVec<[MidiEvent; 64]>,
    cc_param_changes: ParameterChanges,
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
    /// `IMidiMapping` CC→parameter routing table. Built at activation from the
    /// controller's `IMidiMapping` interface and re-built when the caller
    /// responds to [`RestartOutcome::midi_cc_assignment_changed`] by calling
    /// [`rebuild_midi_cc_mapping`](Self::rebuild_midi_cc_mapping).
    midi_cc_mapping: MidiCcMapping,
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
        // The flat `BufferPtrs` arrays carry every bus's channels in bus order
        // (main + sidechain/aux), so size them to the per-direction totals —
        // not just bus 0 — or the flat caller buffer would be truncated to the
        // main bus and the sidechain channels would never reach `prepare`.
        let total_input_channels = loaded.info.total_input_channels();
        let total_output_channels = loaded.info.total_output_channels();
        let input_ptr_count = total_input_channels.max(MIN_PTR_COUNT);
        let output_ptr_count = total_output_channels.max(MIN_PTR_COUNT);
        let input_bus_channels = loaded.info.input_bus_channels.clone();
        let output_bus_channels = loaded.info.output_bus_channels.clone();

        let mut out_param_changes = ParameterChanges::new();
        // SmallVec doesn't expose a sized constructor for inline capacity;
        // pre-reserve via grow_to_capacity-by-clear-after-push. Cheaper
        // approach: just call reserve to pump heap capacity once.
        out_param_changes.queues.reserve(OUTPUT_PARAM_QUEUE_RESERVE);

        let audio = AudioIO {
            sample_rate,
            block_size,
            num_input_channels,
            num_output_channels,
            ptrs: BufferPtrs::new(input_ptr_count, output_ptr_count),
            in_buses: BusBuffers::new(&input_bus_channels, num_input_channels, block_size),
            out_buses: BusBuffers::new(&output_bus_channels, num_output_channels, block_size),
            input_events: EventList::new(),
            output_events: EventList::new(),
            input_param_changes: ParameterChangesImpl::new_empty(),
            output_param_changes: ParameterChangesImpl::new_empty(),
            out_midi: SmallVec::new(),
            out_param_changes,
            cc_filtered_midi: SmallVec::new(),
            cc_param_changes: ParameterChanges::new(),
        };

        let midi_cc_mapping = MidiCcMapping::query(loaded.interfaces.controller.as_ref());
        let mut instance = Self { loaded, audio, midi_cc_mapping };
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

    /// Sample rate in Hz that was applied to `setupProcessing`.
    pub fn sample_rate(&self) -> f64 {
        self.audio.sample_rate
    }

    /// Change the sample rate and re-run `setupProcessing`. Must be called
    /// only when not inside [`process`](Self::process).
    pub fn set_sample_rate(&mut self, rate: f64) -> &mut Self {
        self.audio.sample_rate = rate;
        let _ = self.apply_process_setup();
        self
    }

    /// Maximum block size (samples per channel) that was applied to
    /// `setupProcessing`.
    pub fn block_size(&self) -> usize {
        self.audio.block_size
    }

    /// Change the maximum block size and re-run `setupProcessing`. Must be
    /// called only when not inside [`process`](Self::process).
    pub fn set_block_size(&mut self, size: usize) -> &mut Self {
        self.audio.block_size = size;
        let _ = self.apply_process_setup();
        self
    }

    /// Input channels the plugin will read on bus 0.
    pub fn num_input_channels(&self) -> usize {
        self.audio.num_input_channels
    }

    /// Output channels the plugin will write on bus 0.
    pub fn num_output_channels(&self) -> usize {
        self.audio.num_output_channels
    }

    /// Run one realtime processing block.
    ///
    /// `midi_events` and `note_expressions` are staged into the plugin's input
    /// event list (sorted by `sample_offset`). `param_changes` is forwarded as
    /// `inputParameterChanges`; `transport` populates `ProcessContext`. The
    /// returned [`ProcessOutput`] carries any MIDI / parameter-change events
    /// the plugin emitted.
    ///
    /// Falls back to an empty output if `buffer.num_samples == 0` or if the
    /// plugin returns a non-OK `tresult` (in which case `buffer.outputs` is
    /// also cleared).
    pub fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        midi_events: &[MidiEvent],
        param_changes: Option<&ParameterChanges>,
        note_expressions: &[NoteExpressionValue],
        transport: &TransportInfo,
    ) -> ProcessOutputRef<'_> {
        // Clear pooled return buffers up front so:
        // 1. the bail-out paths below return a borrow into known-empty
        //    state without separately constructing an empty owned output,
        // 2. the steady-state path can `fill_*` into them with the
        //    plugin's freshly-pushed events.
        self.audio.out_midi.clear();
        // Clear inline points without dropping heap capacity.
        for queue in self.audio.out_param_changes.queues.iter_mut() {
            queue.points.clear();
        }
        self.audio.out_param_changes.queues.clear();

        if buffer.num_samples == 0 {
            return ProcessOutputRef {
                midi_events: &self.audio.out_midi,
                parameter_changes: &self.audio.out_param_changes,
            };
        }
        let processor = self.loaded.interfaces.processor.clone();

        // Fill bus-0 channel pointers from the (flat) caller buffer, then build
        // the per-bus `AudioBusBuffers` arrays: bus 0 maps onto the live
        // channels, extra input buses get silence, extra output buses a sink.
        let (input_ptrs, output_ptrs) =
            self.audio.ptrs.prepare(buffer.inputs, buffer.outputs);
        let num_input_buses = self.audio.in_buses.num_buses();
        let num_output_buses = self.audio.out_buses.num_buses();
        // SAFETY: `input_ptrs`/`output_ptrs` point at the just-filled per-channel
        // pointer arrays (length = buffer.inputs/outputs.len()), valid until the
        // `processor.process` call below; the scratch tables are pre-sized.
        let inputs_ptr = unsafe {
            self.audio
                .in_buses
                .prepare(input_ptrs as *const *mut std::ffi::c_void, buffer.inputs.len(), true)
        };
        let outputs_ptr = unsafe {
            self.audio.out_buses.prepare(
                output_ptrs as *const *mut std::ffi::c_void,
                buffer.outputs.len(),
                false,
            )
        };

        // Route IMidiMapping-mapped CCs into parameter changes. When the
        // plugin exposes a non-empty CC→param table, mapped CC/aftertouch/
        // pitch-bend events are pulled out of the MIDI stream and merged into
        // a scratch ParameterChanges alongside the host's automation; the
        // remaining MIDI events still go to the event list. Returns whether
        // the routing pass took ownership of the staged events / params.
        let cc_routed = self.route_midi_cc(midi_events, param_changes);
        let (effective_midi, effective_params): (&[MidiEvent], Option<&ParameterChanges>) =
            if cc_routed {
                (&self.audio.cc_filtered_midi, Some(&self.audio.cc_param_changes))
            } else {
                (midi_events, param_changes)
            };

        // Stage the (possibly CC-filtered) MIDI plus note expressions into the
        // input event list, or clear it when there's nothing to send.
        if effective_midi.is_empty() && note_expressions.is_empty() {
            self.audio.input_events.clear();
        } else {
            self.audio
                .input_events
                .update_from_midi_and_expression(effective_midi, note_expressions);
        }
        self.audio.output_events.clear();
        let input_events_ptr = event_list_ptr(&self.audio.input_events);
        let output_events_ptr = event_list_ptr(&self.audio.output_events);

        // Refill the pooled input/output ParameterChanges wrappers in
        // place instead of building fresh ComWrappers every block.
        let have_input_params = effective_params.map(|pc| !pc.is_empty()).unwrap_or(false);
        if have_input_params {
            self.audio
                .input_param_changes
                .refill_from_changes(effective_params.unwrap());
        } else {
            self.audio.input_param_changes.clear_in_place();
        }
        self.audio.output_param_changes.clear_in_place();

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
                param_changes_ptr(&self.audio.input_param_changes)
            } else {
                std::ptr::null_mut()
            },
            outputParameterChanges: param_changes_ptr(&self.audio.output_param_changes),
            inputEvents: input_events_ptr,
            outputEvents: output_events_ptr,
            processContext: &mut process_context,
        };

        let result = unsafe { processor.process(&mut process_data) };

        if result != kResultOk {
            buffer.clear_outputs();
            return ProcessOutputRef {
                midi_events: &self.audio.out_midi,
                parameter_changes: &self.audio.out_param_changes,
            };
        }

        // Drain the plugin's emitted events into the pooled return
        // buffers. Both `fill_*` clear their destination first and
        // reuse the destination's heap capacity.
        self.audio
            .output_events
            .fill_midi_events(&mut self.audio.out_midi);
        self.audio
            .output_param_changes
            .fill_changes(&mut self.audio.out_param_changes);

        ProcessOutputRef {
            midi_events: &self.audio.out_midi,
            parameter_changes: &self.audio.out_param_changes,
        }
    }

    /// Route `IMidiMapping`-mapped controllers into parameter changes.
    ///
    /// When the plugin exposes a non-empty CC→param table, this walks
    /// `midi_events` and, for each mapped CC / channel-pressure / pitch-bend
    /// message, appends a normalized parameter point to `cc_param_changes`
    /// (seeded with the host's `param_changes`) and *omits* that event from
    /// `cc_filtered_midi`. Unmapped events (notes, unmapped CCs, …) pass
    /// through to `cc_filtered_midi` unchanged.
    ///
    /// Returns `true` when routing happened and the caller should use the
    /// scratch buffers; `false` (no work) when the plugin has no mapping —
    /// the common case for effects and simple instruments, so the hot path
    /// pays nothing.
    ///
    /// Allocation-free after warmup: both scratch buffers are cleared in place
    /// and reuse their heap capacity. Decoding goes through MIDI-1 bytes, the
    /// same lossless path `vst3_event_from_midi` already uses for events.
    fn route_midi_cc(
        &mut self,
        midi_events: &[MidiEvent],
        param_changes: Option<&ParameterChanges>,
    ) -> bool {
        if self.midi_cc_mapping.is_empty() {
            return false;
        }

        // Clear scratch in place (keep heap capacity).
        self.audio.cc_filtered_midi.clear();
        for queue in self.audio.cc_param_changes.queues.iter_mut() {
            queue.points.clear();
        }
        self.audio.cc_param_changes.queues.clear();

        // Seed the merged param changes with the host's automation.
        if let Some(pc) = param_changes {
            for queue in &pc.queues {
                for point in &queue.points {
                    self.audio.cc_param_changes.add_change(
                        queue.param_id,
                        point.sample_offset,
                        point.value,
                    );
                }
            }
        }

        super::midi_mapping::route_cc_events(
            &self.midi_cc_mapping,
            midi_events,
            &mut self.audio.cc_filtered_midi,
            &mut self.audio.cc_param_changes,
        );

        // VST3 requires each IParamValueQueue's points in ascending
        // sampleOffset order; seeding + appending can leave them unsorted.
        super::midi_mapping::sort_param_points(&mut self.audio.cc_param_changes);

        true
    }

    /// Re-query the `IMidiMapping` CC→parameter table from the controller.
    /// Call this when [`RestartOutcome::midi_cc_assignment_changed`] is set.
    pub fn rebuild_midi_cc_mapping(&mut self) {
        tutti_plugin_types::assert_main_thread();
        self.midi_cc_mapping = MidiCcMapping::query(self.loaded.interfaces.controller.as_ref());
    }

    /// Tell the plugin's audio processor to idle. Safe to call repeatedly;
    /// `Drop` calls this automatically.
    pub fn stop_processing(&mut self) {
        unsafe {
            self.loaded.interfaces.processor.setProcessing(0);
        }
    }

    fn apply_process_setup(&mut self) -> Result<()> {
        let mut setup = ProcessSetup {
            processMode: K_REALTIME,
            symbolicSampleSize: T::VST3_SYMBOLIC_SIZE,
            maxSamplesPerBlock: self.audio.block_size as i32,
            sampleRate: self.audio.sample_rate,
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
        unsafe {
            let component = &self.loaded.interfaces.component;
            for i in 0..component.getBusCount(K_AUDIO, K_INPUT) {
                component.activateBus(K_AUDIO, K_INPUT, i, 1);
            }
            for i in 0..component.getBusCount(K_AUDIO, K_OUTPUT) {
                component.activateBus(K_AUDIO, K_OUTPUT, i, 1);
            }

            // Re-sync channel counts after bus activation — some plugins only
            // finalise their bus arrangement once activated.
            if let Some(ch) = component.audio_bus_channel_count(K_INPUT, 0) {
                if ch != self.audio.num_input_channels {
                    self.audio.num_input_channels = ch;
                    self.audio.ptrs.resize_inputs(ch.max(MIN_PTR_COUNT));
                }
            }
            if let Some(ch) = component.audio_bus_channel_count(K_OUTPUT, 1) {
                if ch != self.audio.num_output_channels {
                    self.audio.num_output_channels = ch;
                    self.audio.ptrs.resize_outputs(ch.max(MIN_PTR_COUNT));
                }
            }

            // Some plugins only finalise their full bus arrangement once
            // activated; re-enumerate every bus and rebuild the per-bus
            // scratch so the RT `process` path sees the live layout. Setup-time
            // only — never on the audio thread.
            let input_bus_channels = component.audio_bus_channels(K_INPUT);
            let output_bus_channels = component.audio_bus_channels(K_OUTPUT);
            let block_size = self.audio.block_size;
            // Re-size the flat pointer arrays to the live per-direction totals
            // (main + sidechain/aux) so a multi-bus flat caller buffer isn't
            // truncated to the main bus. No-op for single-bus plugins.
            let total_in: usize = input_bus_channels.iter().sum();
            let total_out: usize = output_bus_channels.iter().sum();
            if total_in > self.audio.num_input_channels {
                self.audio.ptrs.resize_inputs(total_in.max(MIN_PTR_COUNT));
            }
            if total_out > self.audio.num_output_channels {
                self.audio.ptrs.resize_outputs(total_out.max(MIN_PTR_COUNT));
            }
            self.audio.in_buses =
                BusBuffers::new(&input_bus_channels, self.audio.num_input_channels, block_size);
            self.audio.out_buses =
                BusBuffers::new(&output_bus_channels, self.audio.num_output_channels, block_size);
        }
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
