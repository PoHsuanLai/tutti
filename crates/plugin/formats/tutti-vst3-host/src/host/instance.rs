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

use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use smallvec::SmallVec;
use vst3::Steinberg::{
    kResultFalse, kResultOk,
    Vst::{
        IAudioProcessorTrait, IComponentTrait, MediaTypes_::kEvent, ProcessModes_::kRealtime,
        ProcessSetup, SpeakerArr, SpeakerArrangement,
    },
};

use crate::com::{event_list_ptr, param_changes_ptr, EventList, ParameterChangesImpl};
use crate::error::{LoadStage, Result, Vst3Error};
use crate::types::{
    to_process_context, AudioBuffer, BufferPtrs, MidiEvent, ParameterChanges, PluginInfo,
    ProcessOutputRef, TransportInfo, Vst3InputEvents, Vst3Sample,
};

use super::bus_buffers::{BusBuffers, DirectionScratch};
use super::loaded::Vst3Loaded;
use super::midi_learn::MidiLearnProducer;
use super::midi_mapping::{midi_to_mapped_controller, CcRoute, MidiCcMapping};
use super::{IComponentExt, K_INPUT, K_OUTPUT};

pub(super) const K_EVENT: i32 = kEvent as i32;
const K_REALTIME: i32 = kRealtime as i32;

/// Pre-reserve capacity for output param-change queues. One slot per
/// distinct param_id the plugin might emit in a single block; growing
/// beyond this allocates once and then sticks.
const OUTPUT_PARAM_QUEUE_RESERVE: usize = 32;

/// Derive a VST3 `SpeakerArrangement` (a speaker bit mask) from a plain channel
/// count for `setBusArrangements`.
///
/// `0` → empty (a disabled bus), `1` → mono, `2` → stereo (the two overwhelming
/// common cases get their canonical named arrangements). For higher counts we
/// fall back to an N-bit low mask — a well-formed arrangement with the right
/// `getChannelCount`, sufficient to propose "give me N channels on this bus"
/// even when we don't know the exact surround topology. A plugin that wants a
/// specific named layout refuses via `kResultFalse`, and the caller reads its
/// choice back with `getBusArrangement`.
fn arrangement_for_channel_count(count: usize) -> SpeakerArrangement {
    match count {
        0 => SpeakerArr::kEmpty,
        1 => SpeakerArr::kMono,
        2 => SpeakerArr::kStereo,
        // `count` is a channel total (≤ 64 in practice); a low-bit mask of that
        // width is a valid arrangement whose popcount equals `count`.
        n if n < 64 => (1u64 << n) - 1,
        _ => u64::MAX,
    }
}

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
    /// RT-side MIDI-learn CC capture, paired with the
    /// [`Vst3Loaded`](super::loaded::Vst3Loaded)'s consumer. Inert (a single
    /// relaxed atomic load) unless learn is armed.
    midi_learn: MidiLearnProducer,
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
    /// The embedded loaded state. Wrapped in [`ManuallyDrop`] so
    /// [`deactivate`](Vst3Instance::deactivate) can move it out by value
    /// without triggering this type's `Drop` (which would deactivate a second
    /// time). `deactivated` records whether that move happened, so `Drop` knows
    /// whether the field is still live and must be dropped.
    loaded: ManuallyDrop<Vst3Loaded>,
    audio: AudioIO<T>,
    /// Set by [`deactivate`](Vst3Instance::deactivate) once it has taken
    /// `loaded` out. When true, `Drop` neither re-runs the deactivation
    /// sequence nor drops `loaded` (already moved out).
    deactivated: bool,
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
        emitted_param_changes
            .queues
            .reserve(OUTPUT_PARAM_QUEUE_RESERVE);

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
            midi_learn: loaded.midi_learn.producer(),
        };

        let mut instance = Self {
            loaded: ManuallyDrop::new(loaded),
            audio,
            deactivated: false,
        };
        // VST3 activation order: setBusArrangements → setupProcessing →
        // activateBus → setActive. Arrangements must be negotiated first so the
        // plugin has decided its channel layout before we size scratch and set
        // up processing.
        instance.negotiate_bus_arrangements()?;
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
        // Take `loaded` out by value. Marking `deactivated` first means the
        // `Drop` that runs when `self` falls out of scope neither re-runs the
        // deactivation sequence nor drops the (now moved-out) `loaded`.
        self.deactivated = true;
        // SAFETY: `loaded` is live here (only `deactivate` ever takes it, and it
        // consumes `self`), and `deactivated` is now set so `Drop` will not
        // touch it again.
        unsafe { ManuallyDrop::take(&mut self.loaded) }
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
    /// `events` bundles the MIDI and per-note-expressive streams staged into the
    /// plugin's input event list (sorted by `sample_offset`); chord/scale/text
    /// strings are interned into the event list's arena for the duration of the
    /// call. `param_changes` is forwarded as `inputParameterChanges`;
    /// `transport` populates `ProcessContext`. The returned [`ProcessOutput`]
    /// carries any MIDI / parameter-change events the plugin emitted
    /// (plugin-emitted legacy-MIDI-CC-out is decoded to MIDI).
    ///
    /// Falls back to an empty output if `buffer.num_samples == 0` or if the
    /// plugin returns a non-OK `tresult` (in which case `buffer.outputs` is
    /// also cleared).
    pub fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        events: &Vst3InputEvents,
        param_changes: Option<&ParameterChanges>,
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
        let (input_ptrs, output_ptrs) = self.audio.ptrs.prepare(buffer.inputs, buffer.outputs);
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

        // MIDI learn (IMidiLearn): when armed, capture each incoming controller
        // from the *raw* MIDI — before CC routing can divert mapped CCs into
        // parameter changes — so the plugin can learn even controllers that have
        // no mapping yet. A single relaxed atomic load when disarmed (the norm).
        self.capture_midi_learn(events.midi);

        // Route IMidiMapping-mapped CCs into parameter changes. When the plugin
        // exposes a non-empty CC→param table, `CcRoute::route` pulls mapped
        // CC/aftertouch/pitch-bend events out of the MIDI stream and merges them
        // into its scratch ParameterChanges alongside the host's automation,
        // returning the filtered MIDI + merged params to forward; otherwise the
        // caller's own inputs are forwarded untouched.
        let (effective_midi, effective_params): (&[MidiEvent], Option<&ParameterChanges>) =
            match self.audio.cc.route(events.midi, param_changes) {
                Some((midi, params)) => (midi, Some(params)),
                None => (events.midi, param_changes),
            };

        // Stage the (possibly CC-filtered) MIDI plus every other input event
        // source into the input event list, or clear it when there's nothing.
        let effective = Vst3InputEvents {
            midi: effective_midi,
            ..*events
        };
        if !effective.is_empty() {
            self.audio.input.events.update_from_sources(&effective);
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

        let mut process_context = to_process_context(
            transport,
            self.loaded.interfaces.process_context_requirements,
        );
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

    /// Feed incoming MIDI CC controllers to the MIDI-learn capture (for
    /// `IMidiLearn`). When learn is disarmed — the common case — this is a single
    /// relaxed atomic load and an immediate return, so the hot path pays almost
    /// nothing. When armed, each mappable controller (CC / channel-pressure /
    /// pitch-bend) is decoded to its VST3 `(channel, controller)` and captured
    /// allocation-free; the value is irrelevant to learning and dropped.
    #[inline]
    fn capture_midi_learn(&self, midi_events: &[MidiEvent]) {
        // Cheap bail when disarmed (the norm): avoid even decoding the MIDI.
        if !self.audio.midi_learn.is_armed() {
            return;
        }
        for event in midi_events {
            if let Some((channel, controller, _value)) = midi_to_mapped_controller(event) {
                self.audio.midi_learn.capture(channel, controller);
            }
        }
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

    /// Negotiate per-bus speaker arrangements with the plugin via
    /// `IAudioProcessor::setBusArrangements`, before `setupProcessing`.
    ///
    /// We propose one arrangement per bus derived from the channel counts the
    /// component already enumerated (1 → mono, 2 → stereo, N → an N-bit low
    /// mask). Multichannel / surround / sidechain plugins need this: without it
    /// they fall back to a default layout that may not match the buses the host
    /// wired.
    ///
    /// A `kResultFalse` return means the plugin **kept its own layout** rather
    /// than accepting ours — not an error. In that case we read back the
    /// plugin's chosen arrangement per bus with `getBusArrangement`, re-derive
    /// the channel counts, and re-resolve the audio scratch so `process` stages
    /// the right number of channels. (`activate_buses` re-resolves again from
    /// the live component after activation, covering plugins that only finalise
    /// their layout once active.)
    fn negotiate_bus_arrangements(&mut self) -> Result<()> {
        let processor = self.loaded.interfaces.processor.clone();
        let component = &self.loaded.interfaces.component;

        let mut inputs: Vec<SpeakerArrangement> = component
            .audio_bus_channels(K_INPUT)
            .into_iter()
            .map(arrangement_for_channel_count)
            .collect();
        let mut outputs: Vec<SpeakerArrangement> = component
            .audio_bus_channels(K_OUTPUT)
            .into_iter()
            .map(arrangement_for_channel_count)
            .collect();

        let result = unsafe {
            processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as i32,
                outputs.as_mut_ptr(),
                outputs.len() as i32,
            )
        };

        // `kResultTrue`/`kResultOk`: plugin accepted our proposal — the counts
        // we derived it from are already correct.
        if result == kResultOk || result == vst3::Steinberg::kResultTrue {
            return Ok(());
        }

        // Anything else (typically `kResultFalse`): the plugin kept its own
        // layout. Read it back and re-resolve scratch to match. Not an error.
        let in_counts = self.read_back_arrangement_counts(&processor, K_INPUT, inputs.len());
        let out_counts = self.read_back_arrangement_counts(&processor, K_OUTPUT, outputs.len());
        self.resolve_scratch_from_counts(&in_counts, &out_counts);
        Ok(())
    }

    /// Read the plugin's chosen `SpeakerArrangement` for each of `num_buses`
    /// buses in `direction` and translate each to a channel count (popcount of
    /// the speaker mask). A bus whose query fails contributes 0, so the length
    /// always equals `num_buses`.
    fn read_back_arrangement_counts(
        &self,
        processor: &vst3::ComPtr<vst3::Steinberg::Vst::IAudioProcessor>,
        direction: i32,
        num_buses: usize,
    ) -> Vec<usize> {
        (0..num_buses)
            .map(|i| {
                let mut arr: SpeakerArrangement = 0;
                let res = unsafe { processor.getBusArrangement(direction, i as i32, &mut arr) };
                if res == kResultOk {
                    arr.count_ones() as usize
                } else {
                    0
                }
            })
            .collect()
    }

    /// Re-resolve the per-direction audio scratch and pointer tables from
    /// explicit per-bus channel counts (the plugin's chosen layout after a
    /// `setBusArrangements` refusal). Mirrors the resolve in `activate_buses`,
    /// but from counts rather than a fresh component enumeration.
    fn resolve_scratch_from_counts(&mut self, in_counts: &[usize], out_counts: &[usize]) {
        let block_size = self.audio.config.block_size;
        let num_in: usize = in_counts.first().copied().unwrap_or(0);
        let num_out: usize = out_counts.first().copied().unwrap_or(0).max(1);
        let in_scratch = DirectionScratch::<T>::resolve(in_counts, num_in, block_size);
        let out_scratch = DirectionScratch::<T>::resolve(out_counts, num_out, block_size);

        self.audio.config.num_input_channels = num_in;
        self.audio.config.num_output_channels = num_out;
        self.audio.ptrs.resize_inputs(in_scratch.ptr_count);
        self.audio.ptrs.resize_outputs(out_scratch.ptr_count);
        self.audio.input.buses = in_scratch.buses;
        self.audio.output.buses = out_scratch.buses;
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
        // If `deactivate` already ran, `loaded` was moved out and the plugin
        // was deactivated there — nothing to do, and dropping the (empty)
        // `ManuallyDrop` would be a double-free.
        if self.deactivated {
            return;
        }
        self.stop_processing();
        let _ = self.set_active(false);
        // Drop the embedded loaded state, which terminates via Vst3Loaded's Drop.
        // SAFETY: not deactivated, so `loaded` is still live and dropped exactly
        // once here.
        unsafe {
            ManuallyDrop::drop(&mut self.loaded);
        }
    }
}
