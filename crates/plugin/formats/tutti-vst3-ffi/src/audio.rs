//! The audio-thread half of a plugin: `IAudioProcessor::process` and
//! `setProcessing`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, MutexGuard};
use vst3::Steinberg::Vst::{
    IAudioProcessor, IAudioProcessorTrait, ProcessContext, ProcessData, ProcessSetup,
};
use vst3::{ComPtr, ComWrapper};

use crate::bus_buffers::BoundBuses;
use crate::com::{EventList, ParameterChangesImpl, event_list_ptr, param_changes_ptr};
use crate::error::{Vst3Error, check};
use crate::module::Module;
use crate::types::ProcessMode;

/// The lock that keeps the audio half and the teardown of its plugin from
/// overlapping: `process` runs under it, and the plugin's `Drop` takes it for
/// good before it destroys anything.
#[derive(Default)]
pub(crate) struct AudioClaim {
    /// `true` once the plugin is torn down.
    retired: Mutex<bool>,
}

impl AudioClaim {
    /// The claim for one audio-side entry; `None` once the plugin is gone.
    pub(crate) fn enter(&self) -> Option<MutexGuard<'_, bool>> {
        let guard = self.retired.lock();
        (!*guard).then_some(guard)
    }

    /// Retires the claim: waits out an entry in flight, and refuses every later
    /// one. The guard keeps the audio half out until it drops.
    pub(crate) fn retire(&self) -> MutexGuard<'_, bool> {
        let mut guard = self.retired.lock();
        *guard = true;
        guard
    }
}

/// The audio-thread half of a plugin, taken once from
/// [`Plugin::audio_side`](crate::Plugin::audio_side).
///
/// It holds the processor and the module, and names no main-thread entry, so a
/// processor that owns it cannot make a main-thread call. Each entry runs under
/// the instance's claim: if the plugin has been torn down the call is refused
/// with [`Vst3Error::NotInitialized`] and nothing reaches the plugin.
pub struct AudioSide {
    // Fields drop in order: the processor reference goes before the module
    // whose code it points into.
    processor: ComPtr<IAudioProcessor>,
    claim: Arc<AudioClaim>,
    out: Arc<AtomicBool>,
    _module: Arc<Module>,
}

impl std::fmt::Debug for AudioSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSide").finish_non_exhaustive()
    }
}

// SAFETY: the half moves to the audio thread and is used by one thread at a
// time (`process` takes `&mut self`, and the type is not `Sync`-shared). The
// processor pointer is to an object the VST3 contract lets the audio thread
// call, and the claim keeps those calls from overlapping the plugin's teardown.
unsafe impl Send for AudioSide {}

impl Drop for AudioSide {
    fn drop(&mut self) {
        self.out.store(false, Ordering::Release);
    }
}

/// One block's worth of everything `process` is handed.
///
/// The buses are bound to the block's channel slices
/// ([`BusBuffers::bind_inputs_f32`](crate::BusBuffers::bind_inputs_f32)) and the lists
/// are the host's COM objects; the lifetime ties every pointer the plugin sees
/// to the borrows that produced it.
pub struct ProcessIo<'a> {
    /// `kRealtime`, `kPrefetch` or `kOffline` for this block.
    pub mode: ProcessMode,
    /// The block length in frames.
    pub num_samples: usize,
    /// The input buses.
    pub inputs: BoundBuses<'a>,
    /// The output buses.
    pub outputs: BoundBuses<'a>,
    /// The events the plugin reads.
    pub input_events: &'a ComWrapper<EventList>,
    /// The events the plugin writes.
    pub output_events: &'a ComWrapper<EventList>,
    /// The parameter changes the plugin reads; `None` for none.
    pub input_params: Option<&'a ComWrapper<ParameterChangesImpl>>,
    /// The parameter changes the plugin writes.
    pub output_params: &'a ComWrapper<ParameterChangesImpl>,
    /// The transport and time.
    pub context: &'a mut ProcessContext,
    /// The setup the plugin was given, which the conformance observer reports
    /// beside the data.
    pub setup: &'a ProcessSetup,
}

impl AudioSide {
    pub(crate) fn new(
        processor: ComPtr<IAudioProcessor>,
        claim: Arc<AudioClaim>,
        out: Arc<AtomicBool>,
        module: Arc<Module>,
    ) -> Self {
        Self {
            processor,
            claim,
            out,
            _module: module,
        }
    }

    /// `IAudioProcessor::setProcessing`.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::NotInitialized`] if the plugin is gone, or the plugin's
    /// refusal.
    pub fn set_processing(&mut self, on: bool) -> Result<(), Vst3Error> {
        let _claim = self.claim.enter().ok_or(Vst3Error::NotInitialized)?;
        // SAFETY: a plain call on a live processor, under the claim.
        check(unsafe { self.processor.setProcessing(u8::from(on)) })
    }

    /// Renders one block through `IAudioProcessor::process`.
    ///
    /// Allocation-free, and takes the instance's claim for the call.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::NotInitialized`] if the plugin is gone;
    /// [`Vst3Error::InvalidArgument`] if the block is longer than a bound
    /// channel or than the scratch was sized for, or does not fit the ABI's
    /// `int32`; otherwise the plugin's refusal.
    pub fn process(&mut self, mut io: ProcessIo<'_>) -> Result<(), Vst3Error> {
        let _claim = self.claim.enter().ok_or(Vst3Error::NotInitialized)?;
        if io.inputs.width() != io.outputs.width() {
            return Err(Vst3Error::InvalidArgument);
        }
        if io.num_samples > io.inputs.capacity() || io.num_samples > io.outputs.capacity() {
            return Err(Vst3Error::InvalidArgument);
        }
        let num_samples = i32::try_from(io.num_samples).map_err(|_| Vst3Error::InvalidArgument)?;
        let num_inputs =
            i32::try_from(io.inputs.num_buses()).map_err(|_| Vst3Error::InvalidArgument)?;
        let num_outputs =
            i32::try_from(io.outputs.num_buses()).map_err(|_| Vst3Error::InvalidArgument)?;

        let mut data = ProcessData {
            processMode: io.mode.to_vst3(),
            symbolicSampleSize: io.inputs.width().symbolic_size(),
            numSamples: num_samples,
            numInputs: num_inputs,
            numOutputs: num_outputs,
            inputs: io.inputs.finish(),
            outputs: io.outputs.finish(),
            inputParameterChanges: io
                .input_params
                .map_or(std::ptr::null_mut(), param_changes_ptr),
            outputParameterChanges: param_changes_ptr(io.output_params),
            inputEvents: event_list_ptr(io.input_events),
            outputEvents: event_list_ptr(io.output_events),
            processContext: io.context,
        };

        #[cfg(feature = "conformance")]
        crate::conformance::observe(&data, io.setup);
        #[cfg(not(feature = "conformance"))]
        let _ = io.setup;

        // SAFETY: every pointer in `data` is to memory that outlives the call
        // and is valid for what the SDK says the field means: the bus arrays
        // and their channel tables are the scratch `io` borrows and point at
        // slices of at least `numSamples` samples (checked above, the aux block
        // included), the lists are the live host objects `io` borrows, and the
        // context is `io`'s exclusive borrow. The claim is held, so the plugin
        // is not being torn down.
        check(unsafe { self.processor.process(&mut data) })
    }
}

impl std::fmt::Debug for ProcessIo<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessIo")
            .field("mode", &self.mode)
            .field("num_samples", &self.num_samples)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live claim is entered; a retired one is refused for good, and
    /// retiring waits for an entry in flight.
    ///
    /// Mutation: have `retire` skip setting the flag and the refusal assert
    /// fails; make `enter` ignore the flag and the same assert fails.
    #[test]
    fn a_retired_claim_refuses_every_later_entry() {
        let claim = AudioClaim::default();
        assert!(claim.enter().is_some());
        drop(claim.retire());
        assert!(claim.enter().is_none());
        assert!(claim.enter().is_none());
    }

    /// The claim is exclusive: while an entry is in flight a second one cannot
    /// start, so teardown cannot overlap `process`.
    ///
    /// Mutation: replace the mutex with a bare flag and the `try_lock` below
    /// is no longer refused.
    #[test]
    fn an_entry_in_flight_excludes_another() {
        let claim = AudioClaim::default();
        let _held = claim.enter().expect("live");
        assert!(claim.retired.try_lock().is_none());
    }
}
