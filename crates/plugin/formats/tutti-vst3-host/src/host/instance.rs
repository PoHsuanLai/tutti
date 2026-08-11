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
//!
//! What this type adds over [`Vst3Loaded`] is exactly the state that only
//! exists between `setActive(1)` and `setActive(0)`: the scratch buffers sized
//! to the negotiated arrangements, the input/output staging, and the sample
//! width `T`. [`deactivate`](Vst3Instance::deactivate) takes `self` by value
//! and returns the embedded [`Vst3Loaded`], which is what makes a handle to a
//! deactivated plugin unrepresentable — the caller cannot keep the old value to
//! call `process` on, because it was moved. A `&mut self` deactivation would
//! leave one behind, and `process` would then have to answer it with a runtime
//! error on the audio path.
//!
//! The reverse edge is total, which is the precondition a consuming type-state
//! needs. `deactivate` returns a [`Vst3Loaded`] rather than a `Result`: a
//! refusal from `setActive(0)` is dropped, because it leaves no state this host
//! could act on — the COM interfaces are still valid and every loaded-state
//! method is still legal, so the value handed back describes the plugin either
//! way. (That is the asymmetry with `setActive(1)`, where a refusal is a real
//! failure and is reported: a plugin that declined to activate must not be
//! processed.) Contrast `tutti-au-host`, whose transitions can fail in *both*
//! directions and therefore cannot use a consuming pair — a failed transition
//! belongs to neither type, so it carries an internal state enum instead.

use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use vst3::Steinberg::{
    kResultFalse, kResultOk,
    Vst::{
        IAudioProcessorTrait, IComponentTrait, MediaTypes_::kEvent, ProcessSetup, SpeakerArr,
        SpeakerArrangement,
    },
};

use tutti_plugin_types::RtMidiEvents;
use tutti_types::{ChannelLayout, ChannelTopology};

use crate::com::{event_list_ptr, param_changes_ptr, EventList, ParameterChangesImpl};
use crate::error::{LoadStage, Result, Vst3Error};
use crate::types::{
    to_process_context, AudioBuffer, BufferPtrs, BusInfo as BusInfoWrap, MidiEvent,
    ParameterChanges, PluginInfo, ProcessMode, ProcessOutputRef, TransportInfo, Vst3InputEvents,
    Vst3Sample,
};

use super::bus_buffers::{BusBuffers, DirectionScratch};
use super::loaded::Vst3Loaded;
use super::midi_learn::MidiLearnProducer;
use super::midi_mapping::{midi_to_mapped_controller, CcRoute, MidiCcMapping};
use super::{speakers, IComponentExt, K_INPUT, K_OUTPUT};

pub(super) const K_EVENT: i32 = kEvent as i32;

/// Pre-reserve capacity for output param-change queues. One slot per
/// distinct param_id the plugin might emit in a single block; growing
/// beyond this allocates once and then sticks.
const OUTPUT_PARAM_QUEUE_RESERVE: usize = 32;

/// The arrangement to propose for a bus of `layout` channels, when the host has
/// no topology for it.
///
/// Named layouts, not a synthesized mask. The previous version built
/// `(1u64 << n) - 1` — the low `n` bits — which has the right *popcount* and the
/// wrong *speakers*: at width 4 it asks for `L R C Lfe` where a four-channel bus
/// means a surround pair, and at width 8 it asks for a front-of-centre pair
/// rather than the extra surround pair. Only 5.1 came out right, and only
/// because bits 0–5 happen to be contiguous and in the engine's own order.
///
/// `None` for a width with no canonical arrangement, which is a real answer:
/// `setBusArrangements` is a *proposal*, so proposing nothing and reading back
/// what the plugin chose beats proposing a layout that names the wrong
/// speakers.
///
/// Prefers [`speakers::to_arrangement`] once a caller has a real
/// [`ChannelTopology`] to offer; this is the width-only fallback for the path
/// that still enumerates counts.
fn default_arrangement_for(layout: ChannelLayout) -> Option<SpeakerArrangement> {
    Some(match layout.count() {
        0 => SpeakerArr::kEmpty,
        1 => SpeakerArr::kMono,
        2 => SpeakerArr::kStereo,
        // `k40Music` (`L R Ls Rs`), not `k40Cine` (`L R C Cs`): a four-channel
        // bus in this engine is a front pair plus a surround pair.
        4 => SpeakerArr::k40Music,
        6 => SpeakerArr::k51,
        // `k71Music`, whose extra pair sits beyond the 5.1 core — the same
        // shape as the engine's own 7.1 order. `k71CineFullRear` is a different
        // eight-channel layout.
        8 => SpeakerArr::k71Music,
        _ => return None,
    })
}

/// A read-back topology, or `None` when there is nothing usable to say about
/// this bus.
///
/// Split out of `bus_topologies` so the decision is testable: that method needs
/// a live COM plugin, so the rule inside it could otherwise only be exercised
/// by loading one — and a rule no test can reach is a rule that quietly stops
/// being true.
///
/// Three ways to have nothing to say, all of them `None`:
///
/// - **Not fully named** — the arrangement uses a speaker this vocabulary does
///   not have, so the bus cannot be routed by speaker even though its width is
///   known.
/// - **Width disagrees** with the count reported beside it. The count sizes the
///   buffers, so a topology describing a different number of channels describes
///   a different bus.
/// - **Zero width.** A failed `getBusArrangement` yields an empty topology,
///   which is vacuously "fully named" and would otherwise be reported as a real
///   bus that happens to have no channels.
fn usable_topology(topology: ChannelTopology, width: usize) -> Option<ChannelTopology> {
    let usable =
        topology.is_fully_named() && topology.layout().count() as usize == width && width > 0;
    usable.then_some(topology)
}

/// Sample rate / block size / channel counts captured at activation. Read by
/// `apply_process_setup` to fill `ProcessSetup` and by `process` to size
/// `ProcessData`. Channel counts are re-synced post-activation in
/// `activate_buses` (some plugins only finalise their arrangement once active).
struct ProcessConfig {
    sample_rate: f64,
    block_size: usize,
    num_input_channels: ChannelLayout,
    num_output_channels: ChannelLayout,
    /// The mode last handed to `setupProcessing`. This is the value
    /// `ProcessData::processMode` must agree with, so both reads go through
    /// this one field rather than two constants that could drift apart.
    setup_mode: ProcessMode,
    /// The mode written into each block's `ProcessData`. Equal to `setup_mode`
    /// except while a live realtime↔prefetch toggle is in effect — the one
    /// divergence `ProcessSetupCheck` permits without a fresh `setupProcessing`.
    block_mode: ProcessMode,
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
    ///
    /// Capped: the plugin decides how many events it emits, so an unbounded
    /// pool would let it provoke a `malloc` in the audio callback.
    emitted_midi: RtMidiEvents,
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
        Self::load_with_mode(path, sample_rate, block_size, ProcessMode::Realtime)
    }

    /// Load a VST3 plugin and activate it for a specific [`ProcessMode`].
    ///
    /// Use this — with [`ProcessMode::Offline`] — for a bounce or export.
    /// [`load`](Self::load) is the [`Realtime`](ProcessMode::Realtime) case.
    ///
    /// # Errors
    ///
    /// As [`load`](Self::load).
    pub fn load_with_mode(
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        mode: ProcessMode,
    ) -> Result<Self> {
        let loaded = Vst3Loaded::load(path)?;
        Self::from_loaded(loaded, sample_rate, block_size, mode)
    }

    /// Load and activate one named audio class from a multi-plugin bundle.
    ///
    /// See [`Vst3Loaded::load_class`] for why a bundle may hold many: `load`
    /// takes the first audio class, which cannot address the other 33 in a
    /// suite like `mda-vst3`.
    ///
    /// # Errors
    ///
    /// As [`load`](Self::load), plus a
    /// [`LoadFailed`](crate::Vst3Error::LoadFailed) listing the bundle's actual
    /// class names when `class_name` matches none of them.
    pub fn load_class(
        path: &Path,
        class_name: &str,
        sample_rate: f64,
        block_size: usize,
    ) -> Result<Self> {
        let loaded = Vst3Loaded::load_class(path, Some(class_name))?;
        Self::from_loaded(loaded, sample_rate, block_size, ProcessMode::Realtime)
    }

    /// Called by [`Vst3Loaded::activate`]. Runs `setupProcessing`, activates
    /// buses, calls `setActive(1)` + `setProcessing(1)`.
    pub(super) fn from_loaded(
        loaded: Vst3Loaded,
        sample_rate: f64,
        block_size: usize,
        mode: ProcessMode,
    ) -> Result<Self> {
        if T::VST3_SYMBOLIC_SIZE == crate::types::K_SAMPLE_64_INT && !loaded.info.supports_f64 {
            return Err(Vst3Error::NotSupported(
                "Plugin does not support 64-bit processing".to_string(),
            ));
        }

        let num_input_channels = ChannelLayout::from(loaded.info.num_inputs);
        let num_output_channels = ChannelLayout::from(loaded.info.num_outputs);
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
                // Both start at the requested mode; `apply_process_setup` is
                // what actually delivers `setup_mode` to the plugin, and
                // `set_prefetch` is the only thing that may move `block_mode`
                // away from it.
                setup_mode: mode,
                block_mode: mode,
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
                emitted_midi: RtMidiEvents::new(),
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
        // plugin has decided its channel layout before the scratch is sized and
        // processing is set up.
        instance.negotiate_bus_arrangements()?;
        instance.apply_process_setup()?;
        instance.activate_buses()?;
        instance.set_active(true)?;
        // The spec requires reading latency after each `setActive(true)`: the
        // value is only valid once the plugin is active, and it is what a host
        // feeds into delay compensation. Skipping it means a latency-reporting
        // plugin runs uncompensated — audibly out of time against every other
        // track. The read is also what tells a plugin the host implements PDC
        // at all (HostChecker flags the omission as "Missing Call:
        // getLatencySamples ()").
        //
        // The value is returned to the caller through `read_latency_samples`
        // rather than stored here: the compensation machinery lives in
        // `tutti-core` (`LatencyGraph` / `Compensation` / `PdcDelay`), which
        // needs it per graph rather than per instance. What matters at this
        // layer is that the call happens, in the right place, every activation
        // — the figure PDC compensates by is only as good as this read.
        let _ = instance.loaded.read_latency_samples();
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

    /// The sample rate this instance is currently activated at.
    ///
    /// Worth reading after a refused [`set_sample_rate`](Self::set_sample_rate):
    /// the instance keeps running at the rate the plugin already accepted, and
    /// this is what says which one that is.
    pub fn sample_rate(&self) -> f64 {
        self.audio.config.sample_rate
    }

    /// Change the sample rate, re-running `setupProcessing` across a
    /// deactivate/reactivate cycle.
    ///
    /// `setupProcessing` is `[UI-thread & (Initialized | Connected)]` and
    /// "called in disable state (setActive not called with true)"
    /// (`ivstaudioprocessor.h:328-330`), so it cannot be delivered in place:
    /// this type is active by construction. Setup-time only — never call from
    /// the audio thread or from inside [`process`](Self::process), since it
    /// deactivates the plugin and re-sizes its buffers.
    ///
    /// # Errors
    /// [`Vst3Error::PluginError`] if the plugin refuses the new rate. The
    /// instance is **rolled back** to the rate it was already running at and
    /// left active there. A rollback that itself fails leaves the instance
    /// inactive and reports the second failure, not the first.
    pub fn set_sample_rate(&mut self, rate: f64) -> Result<()> {
        if rate == self.audio.config.sample_rate {
            return Ok(());
        }
        self.reconfigure(rate)
    }

    /// Deactivate → `setupProcessing` at `rate` → reactivate, restoring the
    /// previous rate if the plugin refuses the new one.
    ///
    /// Distinct from [`restart_bus_configuration`](Self::restart_bus_configuration),
    /// which brackets the same way but for a different reason: that one exists
    /// to re-ask the plugin for a bus layout it has just announced changed, so
    /// it renegotiates arrangements and re-reads counts inside the cycle. A
    /// rate change announces nothing about the layout, and renegotiating one
    /// the plugin never said had moved would let a `setBusArrangements`
    /// refusal re-resolve scratch behind an unrelated call.
    ///
    /// Rolling back is the one recovery available — the plugin accepted the
    /// previous rate once — and it restores an active instance the caller can
    /// keep processing. A refused rollback is a worse fact than the refusal
    /// that caused it, so it propagates as its own `Err` rather than folding
    /// into the one below; the instance is then genuinely inactive, and `Drop`
    /// still tears it down safely.
    fn reconfigure(&mut self, rate: f64) -> Result<()> {
        let previous = self.audio.config.sample_rate;

        self.stop_processing();
        self.set_active(false)?;

        self.audio.config.sample_rate = rate;
        if self.apply_process_setup().is_ok() && self.set_active(true).is_ok() {
            // Latency is only valid once active, and a rate change moves it for
            // any plugin whose group delay is a duration rather than a sample
            // count. Same read as `from_loaded`, for the same reason.
            let _ = self.loaded.read_latency_samples();
            return Ok(());
        }

        // Refused. Put back what the plugin already accepted once.
        self.audio.config.sample_rate = previous;
        self.apply_process_setup()?;
        self.set_active(true)?;
        let _ = self.loaded.read_latency_samples();

        Err(Vst3Error::PluginError {
            stage: LoadStage::Setup,
            code: kResultFalse,
        })
    }

    /// The mode this instance's blocks are currently processed in.
    pub fn process_mode(&self) -> ProcessMode {
        self.audio.config.block_mode
    }

    /// Toggle between [`Realtime`](ProcessMode::Realtime) and
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
    /// be a spec violation. Re-activate with
    /// [`Vst3Loaded::activate_with_mode`](super::loaded::Vst3Loaded::activate_with_mode)
    /// to change an offline instance's mode.
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

    /// Run one realtime processing block.
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
            processMode: self.audio.config.block_mode.to_vst3(),
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

        // Test-only seam: hand the fully-built ProcessData to an installed
        // observer before the plugin sees it. Compiled out by default. The
        // setup comes from `process_setup()`, the same accessor
        // `apply_process_setup` hands the plugin, so an observer comparing the
        // two sees the real negotiated value.
        #[cfg(feature = "conformance")]
        {
            let setup = self.process_setup();
            super::conformance::observe(&process_data, &setup);
        }

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
    /// Call this when
    /// [`RestartOutcome::midi_cc_assignment_changed`](crate::RestartOutcome::midi_cc_assignment_changed)
    /// is set.
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
    /// One arrangement per bus is proposed, derived from the channel counts the
    /// component already enumerated (1 → mono, 2 → stereo, N → an N-bit low
    /// mask). Multichannel / surround / sidechain plugins need this: without it
    /// they fall back to a default layout that may not match the buses the host
    /// wired.
    ///
    /// A `kResultFalse` return means the plugin **kept its own layout** rather
    /// than accepting the host's — not an error. In that case the host reads
    /// back the plugin's chosen arrangement per bus with `getBusArrangement`,
    /// re-derives the channel counts, and re-resolves the audio scratch so
    /// `process` stages the right number of channels. (`activate_buses` re-resolves again from
    /// the live component after activation, covering plugins that only finalise
    /// their layout once active.)
    ///
    /// **Either way this ends by reconciling `PluginInfo`.** The scratch and
    /// the reported layout are two separate copies of the same fact, and only
    /// the scratch was being updated: `resolve_scratch_from_counts` fixes what
    /// this instance renders through, while `PluginInfo` is what every caller
    /// above reads — `PluginClient::new` sizes its fundsp node from it. A
    /// plugin that refused therefore had its *proposed* width reported while it
    /// ran another. Reconciling on the accepting branch too is not belt-and-
    /// braces: a plugin may accept the arrangement and still restructure its
    /// buses in the same call, and one exit path is one thing to keep true.
    fn negotiate_bus_arrangements(&mut self) -> Result<()> {
        let processor = self.loaded.interfaces.processor.clone();
        let component = &self.loaded.interfaces.component;

        // A bus whose width has no canonical arrangement makes the whole
        // proposal unsendable: `setBusArrangements` takes one arrangement per
        // bus, so there is no way to say "no opinion" about a single entry.
        // Skipping the call entirely is the honest move — the plugin keeps the
        // layout it already has, and the reconcile below reports it. Proposing
        // a made-up mask for that bus is what this change exists to stop.
        let proposals = component
            .audio_bus_channels(K_INPUT)
            .into_iter()
            .map(|c| default_arrangement_for(ChannelLayout::from(c)))
            .collect::<Option<Vec<_>>>()
            .zip(
                component
                    .audio_bus_channels(K_OUTPUT)
                    .into_iter()
                    .map(|c| default_arrangement_for(ChannelLayout::from(c)))
                    .collect::<Option<Vec<_>>>(),
            );

        let Some((mut inputs, mut outputs)) = proposals else {
            self.loaded.reconcile_bus_counts();
            return Ok(());
        };

        let result = unsafe {
            processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as i32,
                outputs.as_mut_ptr(),
                outputs.len() as i32,
            )
        };

        // Anything but `kResultTrue`/`kResultOk` (typically `kResultFalse`):
        // the plugin kept its own layout. Read it back and re-resolve scratch
        // to match. Not an error.
        if result != kResultOk && result != vst3::Steinberg::kResultTrue {
            let in_layouts = self.read_back_arrangements(&processor, K_INPUT, inputs.len());
            let out_layouts = self.read_back_arrangements(&processor, K_OUTPUT, outputs.len());
            let widths = |ls: &[ChannelTopology]| -> Vec<usize> {
                ls.iter().map(|t| t.layout().count() as usize).collect()
            };
            self.resolve_scratch_from_counts(&widths(&in_layouts), &widths(&out_layouts));
        }

        // Re-enumerated from the component rather than from `in_counts` above,
        // so there is one answer to "what is the layout" and it is the same
        // enumeration `activate_buses` and the restart path use.
        // `getBusArrangement` reports only the buses that existed at proposal
        // time, and a refusal is exactly when a plugin may have restructured
        // them.
        self.loaded.reconcile_bus_counts();
        Ok(())
    }

    /// The channel topology the plugin is running on each bus in `direction`.
    ///
    /// Queried live from `getBusArrangement` rather than cached from
    /// negotiation, because the two branches of `negotiate_bus_arrangements`
    /// leave different amounts behind — the accepting branch never reads back
    /// at all — and a plugin may restructure its buses afterwards. Asking is
    /// cheap and cannot go stale.
    ///
    /// An entry is `None` when the plugin's arrangement names a speaker this
    /// vocabulary cannot, or when the query failed: both mean "cannot route
    /// this bus by speaker", which is what a caller acts on. A bus whose
    /// arrangement is fully named yields `Some`, and its width always equals
    /// the count reported beside it.
    /// The channel topology of each **input** bus. See
    /// `bus_topologies`.
    ///
    /// Two named methods rather than one taking a direction, so a caller does
    /// not need the crate's private `kInput`/`kOutput` constants — and cannot
    /// pass an `i32` that is neither.
    pub fn input_bus_topologies(&self) -> Vec<Option<ChannelTopology>> {
        self.bus_topologies(K_INPUT)
    }

    /// The channel topology of each **output** bus. See
    /// `bus_topologies`.
    pub fn output_bus_topologies(&self) -> Vec<Option<ChannelTopology>> {
        self.bus_topologies(K_OUTPUT)
    }

    fn bus_topologies(&self, direction: i32) -> Vec<Option<ChannelTopology>> {
        let processor = self.loaded.interfaces.processor.clone();
        let num_buses = self
            .loaded
            .interfaces
            .component
            .audio_bus_channels(direction)
            .len();
        let widths = self
            .loaded
            .interfaces
            .component
            .audio_bus_channels(direction);
        self.read_back_arrangements(&processor, direction, num_buses)
            .into_iter()
            .zip(widths)
            .map(|(topology, width)| usable_topology(topology, width))
            .collect()
    }

    /// Read the plugin's chosen layout for each of `num_buses` buses in
    /// `direction`. A bus whose query fails contributes an empty topology, so
    /// the length always equals `num_buses`.
    ///
    /// Decoded into a [`ChannelTopology`] rather than straight to a count: the
    /// mask that arrives here carries *which speaker each channel is*, and
    /// `count_ones()` discarded it on the line after it arrived. The width is
    /// still available from [`ChannelTopology::layout`] and is identical to the
    /// popcount, so every existing caller reads the same number it did before.
    fn read_back_arrangements(
        &self,
        processor: &vst3::ComPtr<vst3::Steinberg::Vst::IAudioProcessor>,
        direction: i32,
        num_buses: usize,
    ) -> Vec<ChannelTopology> {
        (0..num_buses)
            .map(|i| {
                let mut arr: SpeakerArrangement = 0;
                let res = unsafe { processor.getBusArrangement(direction, i as i32, &mut arr) };
                if res == kResultOk {
                    speakers::from_arrangement(arr)
                } else {
                    ChannelTopology::new([])
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
        // Bus 0's count, or 0 when the plugin named no bus in that direction.
        // The scratch below is sized from the full per-bus slices, not from
        // these, so an empty direction stays empty rather than being rounded up
        // to a channel the plugin never declared.
        let num_in = ChannelLayout::from(in_counts.first().copied().unwrap_or(0));
        let num_out = ChannelLayout::from(out_counts.first().copied().unwrap_or(0));
        let in_scratch = DirectionScratch::<T>::resolve(in_counts, num_in, block_size);
        let out_scratch = DirectionScratch::<T>::resolve(out_counts, num_out, block_size);

        self.audio.config.num_input_channels = num_in;
        self.audio.config.num_output_channels = num_out;
        self.audio.ptrs.resize_inputs(in_scratch.ptr_count);
        self.audio.ptrs.resize_outputs(out_scratch.ptr_count);
        self.audio.input.buses = in_scratch.buses;
        self.audio.output.buses = out_scratch.buses;
    }

    /// The `ProcessSetup` describing this instance's negotiated configuration.
    ///
    /// The single source of the setup values, so what `setupProcessing`
    /// receives and what the conformance observer is told cannot drift — a
    /// second, hand-built copy for the observer would agree with `ProcessData`
    /// by construction and hide exactly the mismatch the check exists to find.
    fn process_setup(&self) -> ProcessSetup {
        ProcessSetup {
            processMode: self.audio.config.setup_mode.to_vst3(),
            symbolicSampleSize: T::VST3_SYMBOLIC_SIZE,
            maxSamplesPerBlock: self.audio.config.block_size as i32,
            sampleRate: self.audio.config.sample_rate,
        }
    }

    fn apply_process_setup(&mut self) -> Result<()> {
        let mut setup = self.process_setup();
        let result = unsafe { self.loaded.interfaces.processor.setupProcessing(&mut setup) };
        if result != kResultOk && result != kResultFalse {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Setup,
                code: result,
            });
        }
        Ok(())
    }

    /// Activate the buses this host wants live, per
    /// [`wants_activation`](super::wants_activation).
    ///
    /// Event buses are activated on the same terms as audio ones: the spec
    /// starts every bus inactive regardless of media type, and `activateBus`
    /// takes the type as a parameter precisely because it is not audio-only.
    /// Reading only the counts — to decide whether the plugin speaks MIDI at
    /// all — leaves event buses inactive: a plugin that honours the inactive
    /// default then receives no events while one that ignores it works, which
    /// presents as a plugin quirk rather than a host bug.
    fn activate_buses(&mut self) -> Result<()> {
        const K_AUDIO: i32 = super::K_AUDIO;
        let component = &self.loaded.interfaces.component;
        for (media, direction) in [
            (K_AUDIO, K_INPUT),
            (K_AUDIO, K_OUTPUT),
            (K_EVENT, K_INPUT),
            (K_EVENT, K_OUTPUT),
        ] {
            for i in 0..unsafe { component.getBusCount(media, direction) } {
                let mut bus = BusInfoWrap::default();
                let read = unsafe { component.getBusInfo(media, direction, i, bus.as_mut_inner()) };
                // A bus whose info cannot be read is activated anyway. That
                // restores the pre-policy behaviour for exactly the plugins
                // that cannot answer the question the policy asks, rather than
                // silently dropping a bus over a failed query.
                let wants =
                    read != kResultOk || super::wants_activation(bus.bus_type(), bus.flags());
                if wants {
                    unsafe { component.activateBus(media, direction, i, 1) };
                }
            }
        }

        // Re-resolve the per-bus scratch from the live component — some plugins
        // only finalise their bus arrangement once activated, so the layout can
        // differ from the PluginInfo snapshot used in `from_loaded`. Setup-time
        // only; never on the audio thread.
        //
        // Both the flat count and the scratch come from this one enumeration.
        // Querying them separately lets the flat one fall back to the
        // pre-activation value when its query fails — reinstating the stale
        // layout this re-resolve exists to replace, at exactly the moment the
        // live read said it could not be trusted.
        let in_counts = component.audio_bus_channels(K_INPUT);
        let out_counts = component.audio_bus_channels(K_OUTPUT);
        let num_in = ChannelLayout::from(in_counts.first().copied().unwrap_or(0));
        let num_out = ChannelLayout::from(out_counts.first().copied().unwrap_or(0));
        let block_size = self.audio.config.block_size;
        let in_scratch = DirectionScratch::<T>::resolve(&in_counts, num_in, block_size);
        let out_scratch = DirectionScratch::<T>::resolve(&out_counts, num_out, block_size);

        self.audio.config.num_input_channels = num_in;
        self.audio.config.num_output_channels = num_out;
        self.audio.ptrs.resize_inputs(in_scratch.ptr_count);
        self.audio.ptrs.resize_outputs(out_scratch.ptr_count);
        self.audio.input.buses = in_scratch.buses;
        self.audio.output.buses = out_scratch.buses;
        Ok(())
    }

    /// Run the deactivate → re-enumerate → reactivate cycle two `RestartFlags`
    /// require, and report whether the plugin came back up.
    ///
    /// `ivsteditcontroller.h:125-127` for `kIoChanged`: *"The host has to
    /// deactivate the plug-in, asks the plug-in for its wanted new bus
    /// configurations, adapts its processing graph and reactivate the
    /// plug-in."* `kLatencyChanged` (`:137-138`) states the same cycle and adds
    /// that `getLatencySamples` should be read *after* `setActive(true)`.
    ///
    /// This lives on `Vst3Instance` rather than `Vst3Loaded` because only this
    /// type knows whether the plugin is active. Reaching the re-enumeration
    /// through `DerefMut` on a live instance skips the cycle entirely — the bus
    /// layout is re-read while the plugin is still active, which is the one
    /// ordering the spec rules out, and a plugin that only recomputes its layout
    /// or its group delay inside `setActive(true)` answers with the old figures.
    ///
    /// A refused reactivation is reported, not swallowed: `set_active` treats
    /// `kResultFalse` as the refusal it is, and a caller that ignored this
    /// would go on to `process` a plugin that is no longer active.
    pub fn restart_bus_configuration(&mut self) -> Result<()> {
        self.stop_processing();
        self.set_active(false)?;

        // Re-ask the plugin for its layout while it is down. Arrangements are
        // renegotiated before the counts are re-read, matching the activation
        // order in `from_loaded` — a plugin decides its channel layout in
        // `setBusArrangements`, so reading counts first would cache the layout
        // it is about to replace. The re-read is `negotiate_bus_arrangements`'s
        // own last step, so it is not repeated here.
        self.negotiate_bus_arrangements()?;
        self.apply_process_setup()?;
        self.activate_buses()?;

        self.set_active(true)?;
        // Latency is only valid once active, and both flags that reach here can
        // change it. Same read as `from_loaded`, for the same reason.
        let _ = self.loaded.read_latency_samples();
        Ok(())
    }

    fn set_active(&mut self, active: bool) -> Result<()> {
        let flag: vst3::Steinberg::TBool = if active { 1 } else { 0 };
        let result = unsafe { self.loaded.interfaces.component.setActive(flag) };
        // `kResultFalse` is a *refusal*, not a "didn't implement it". A plugin
        // whose licence check, dongle, or device claim fails reports it here,
        // and it is the only way it can. Accepting it as success left the host
        // believing an inactive plugin was live and calling `process` on it —
        // which is undefined, and which the plugin has no way to prevent.
        //
        // Steinberg's own suite agrees: `validstatetransition.cpp` fails the
        // plugin unless `setActive` returns exactly `kResultTrue`.
        //
        // This is the opposite of the `setProcessing` call below, where a
        // non-OK result genuinely does mean "not implemented" — see there.
        if result != kResultOk {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Activation,
                code: result,
            });
        }
        if active {
            // `setProcessing` only *informs* the plugin that processing is about
            // to start; the SDK's own `AudioEffect` base returns
            // `kNotImplemented` for it, so every plugin that doesn't need the
            // notification reports failure here. Treating that as fatal rejects
            // valid plugins — 7 of Steinberg's 11 reference plugins among them.
            // Nothing downstream depends on the result, so ignore it.
            unsafe {
                self.loaded.interfaces.processor.setProcessing(1);
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

#[cfg(test)]
mod default_arrangement_tests {
    use super::*;

    /// Each width proposes the arrangement whose speakers the engine means.
    ///
    /// The table is asserted against `SpeakerArr`'s named constants rather than
    /// against literals: a literal would agree with a wrong mask that happened
    /// to have the right popcount, which is precisely how the previous
    /// `(1 << n) - 1` survived. Widths 4 and 8 are the two it got wrong.
    #[test]
    fn each_width_proposes_its_named_arrangement() {
        for (width, expected, name) in [
            (0u16, SpeakerArr::kEmpty, "kEmpty"),
            (1, SpeakerArr::kMono, "kMono"),
            (2, SpeakerArr::kStereo, "kStereo"),
            (4, SpeakerArr::k40Music, "k40Music"),
            (6, SpeakerArr::k51, "k51"),
            (8, SpeakerArr::k71Music, "k71Music"),
        ] {
            assert_eq!(
                default_arrangement_for(ChannelLayout::from(width)),
                Some(expected),
                "width {width} must propose {name}"
            );
        }
    }

    /// The proposal a width makes decodes back to that many channels.
    ///
    /// Guards the pairing rather than the mask: an entry naming a real
    /// arrangement of the *wrong width* — `k51` for width 4, say — would pass
    /// the table test above if the expectation were edited to match, but a
    /// plugin would then be handed a bus of a different size than the host
    /// allocated.
    #[test]
    fn a_proposed_arrangement_has_the_width_it_was_asked_for() {
        for width in [0u16, 1, 2, 4, 6, 8] {
            let arr = default_arrangement_for(ChannelLayout::from(width)).expect("width is named");
            assert_eq!(
                speakers::from_arrangement(arr).layout().count(),
                width,
                "the arrangement proposed for width {width} is not {width} channels"
            );
        }
    }

    /// The 4- and 8-channel proposals are not the low-bit masks they replaced.
    ///
    /// Named explicitly because those two are the defect: the old masks had the
    /// right channel count and the wrong speakers, so every width-based check
    /// passed while a quad bus was negotiated as a centre-plus-LFE 3.1.
    #[test]
    fn the_four_and_eight_channel_proposals_are_not_the_low_bit_masks() {
        for width in [4u16, 8] {
            let proposed = default_arrangement_for(ChannelLayout::from(width)).expect("named");
            let old_mask: SpeakerArrangement = (1u64 << width) - 1;
            assert_ne!(
                proposed, old_mask,
                "width {width} is still proposing the low-{width}-bits mask"
            );
        }
    }

    /// A fully-named topology whose width matches its bus is reported.
    #[test]
    fn a_named_topology_matching_its_bus_width_is_usable() {
        let stereo = speakers::from_arrangement(SpeakerArr::kStereo);
        assert_eq!(usable_topology(stereo.clone(), 2), Some(stereo));
    }

    /// A topology naming a speaker this vocabulary lacks is not usable.
    ///
    /// The width is right and the bus is real — but nothing can route it by
    /// speaker, so reporting it would promise more than is known.
    #[test]
    fn a_topology_with_an_unnamed_speaker_is_not_usable() {
        // `kSpeakerLfe2` is a real VST3 speaker with no name here.
        let arr = SpeakerArr::kStereo | vst3::Steinberg::Vst::kSpeakerLfe2;
        let topology = speakers::from_arrangement(arr);
        assert_eq!(topology.layout().count(), 3, "the width is still known");
        assert_eq!(usable_topology(topology, 3), None);
    }

    /// A topology whose width disagrees with its bus is not usable.
    ///
    /// The count sizes the buffers, so a topology describing a different number
    /// of channels describes a different bus — there is no way to adjudicate
    /// which is right, and acting on the wrong one misroutes audio.
    #[test]
    fn a_topology_that_disagrees_with_its_bus_width_is_not_usable() {
        let stereo = speakers::from_arrangement(SpeakerArr::kStereo);
        assert_eq!(usable_topology(stereo, 6), None);
    }

    /// An empty topology on a zero-width bus is not reported as a real answer.
    ///
    /// This is the one a naive check misses: a failed `getBusArrangement`
    /// yields an empty topology, which is *vacuously* fully named and whose
    /// width *does* equal the zero count beside it. Both other guards pass, so
    /// only the explicit `width > 0` stops "the plugin never answered" from
    /// being reported as "this bus has no channels".
    #[test]
    fn an_empty_topology_is_not_a_usable_answer() {
        let empty = speakers::from_arrangement(SpeakerArr::kEmpty);
        assert!(empty.is_fully_named(), "vacuously true, which is the trap");
        assert_eq!(empty.layout().count(), 0);
        assert_eq!(usable_topology(empty, 0), None);
    }

    /// A width with no canonical arrangement declines rather than inventing one.
    ///
    /// `None` is what makes the caller skip `setBusArrangements` entirely, so a
    /// plugin keeps the layout it already had instead of being handed a
    /// fabricated one.
    #[test]
    fn an_unnamed_width_has_no_proposal() {
        for width in [3u16, 5, 7, 9, 12] {
            assert_eq!(
                default_arrangement_for(ChannelLayout::from(width)),
                None,
                "width {width} should have no canonical arrangement"
            );
        }
    }
}
