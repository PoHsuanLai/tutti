//! `IComponent` and `IAudioProcessor` calls: buses, the processor's setup, and state.

use super::*;

impl Plugin {
    // ── Buses and the processor's setup ──────────────────────────────────────

    /// `IComponent::getBusCount`.
    #[must_use]
    pub fn bus_count(&self, main: &MainThread, media: MediaType, direction: BusDirection) -> i32 {
        let _scope = self.scoped(main);
        // SAFETY: a plain getter on a live component.
        unsafe { self.component.getBusCount(media.raw(), direction.raw()) }
    }

    /// `IComponent::getBusInfo`.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn bus_info(
        &self,
        main: &MainThread,
        media: MediaType,
        direction: BusDirection,
        index: i32,
    ) -> Result<BusInfo, Vst3Error> {
        let _scope = self.scoped(main);
        let mut bus = BusInfo::default();
        // SAFETY: the out-parameter is a valid `BusInfo` for the call.
        check(unsafe {
            self.component
                .getBusInfo(media.raw(), direction.raw(), index, bus.as_mut_inner())
        })?;
        Ok(bus)
    }

    /// `IComponent::activateBus`.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists, or the plugin's
    /// refusal (a plugin may decline an optional bus).
    pub fn activate_bus(
        &self,
        main: &MainThread,
        media: MediaType,
        direction: BusDirection,
        index: i32,
    ) -> Result<(), Vst3Error> {
        let _scope = self.scoped(main);
        let _state = self.shared.lock_for_control()?;
        // SAFETY: a plain call on a live component, under the claim, with no
        // audio half out.
        check(unsafe {
            self.component
                .activateBus(media.raw(), direction.raw(), index, 1)
        })
    }

    /// Channel layout of **bus 0** of the audio buses in `direction`, exactly
    /// as the plugin reports it.
    ///
    /// `None` when the plugin reports no buses or `getBusInfo` fails — the two
    /// cases where there is no answer to carry. A plugin that answers with zero
    /// channels is reported as zero: that is a bus the plugin declared and then
    /// said is empty, which is a different fact from "no bus".
    ///
    /// A negative `channelCount` is floored to 0, and one past `u16::MAX`
    /// saturates there (a layout holds no more, and a wrapped count would turn
    /// 65 538 channels into a stereo bus). The ABI types it `int32` with no
    /// negative meaning, so these are the only out-of-contract clamps here;
    /// there is no error path to refuse through (a bus query answers `Option`
    /// or a `Vec`), and the saturated width is one no arrangement names, so the
    /// host proposes none and the plugin keeps its own.
    #[must_use]
    pub fn audio_bus_channel_count(
        &self,
        main: &MainThread,
        direction: BusDirection,
    ) -> Option<ChannelLayout> {
        let _scope = self.scoped(main);
        if self.bus_count(main, MediaType::Audio, direction) <= 0 {
            return None;
        }
        let bus = self.bus_info(main, MediaType::Audio, direction, 0).ok()?;
        Some(ChannelLayout::from(bus_width(bus.channel_count())))
    }

    /// Channel count of every audio bus in `direction`, in bus-index order,
    /// each floored and saturated as [`audio_bus_channel_count`](Self::audio_bus_channel_count)
    /// describes. A
    /// bus whose query fails contributes 0 so the vec length always equals the
    /// bus count.
    #[must_use]
    pub fn audio_bus_channels(&self, main: &MainThread, direction: BusDirection) -> Vec<usize> {
        let _scope = self.scoped(main);
        let num_buses = self.bus_count(main, MediaType::Audio, direction);
        if num_buses <= 0 {
            return Vec::new();
        }
        (0..num_buses)
            .map(|i| {
                self.bus_info(main, MediaType::Audio, direction, i)
                    .map_or(0, |bus| usize::from(bus_width(bus.channel_count())))
            })
            .collect()
    }

    /// The first audio bus in `direction` whose raw `channelCount` no layout
    /// holds (negative, or above `u16::MAX`), as `(bus index, channelCount)`.
    ///
    /// The accessors above saturate; the load path asks this to refuse such a
    /// plugin instead, naming the bus.
    #[must_use]
    pub fn first_unrepresentable_audio_bus(
        &self,
        main: &MainThread,
        direction: BusDirection,
    ) -> Option<(i32, i32)> {
        let _scope = self.scoped(main);
        let num_buses = self.bus_count(main, MediaType::Audio, direction);
        let counts = (0..num_buses.max(0)).map(|i| {
            self.bus_info(main, MediaType::Audio, direction, i)
                .map_or(0, |bus| bus.channel_count())
        });
        first_unrepresentable(counts)
    }

    /// `IAudioProcessor::setBusArrangements`.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists; otherwise the plugin's
    /// refusal. A new arrangement invalidates the setup: the plugin must be set
    /// up again before an audio half is handed out.
    pub fn set_bus_arrangements(
        &self,
        main: &MainThread,
        inputs: &mut [SpeakerArrangement],
        outputs: &mut [SpeakerArrangement],
    ) -> Result<(), Vst3Error> {
        let _scope = self.scoped(main);
        let mut state = self.shared.lock_for_control()?;
        state.setup = None;
        state.buses = None;
        // SAFETY: each pointer is valid for the length passed beside it, for
        // the call, and the claim is held with no audio half out.
        check(unsafe {
            self.processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as i32,
                outputs.as_mut_ptr(),
                outputs.len() as i32,
            )
        })
    }

    /// `IAudioProcessor::getBusArrangement`.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn bus_arrangement(
        &self,
        main: &MainThread,
        direction: BusDirection,
        index: i32,
    ) -> Result<SpeakerArrangement, Vst3Error> {
        let _scope = self.scoped(main);
        let mut arrangement: SpeakerArrangement = 0;
        // SAFETY: `arrangement` is a valid out-parameter.
        check(unsafe {
            self.processor
                .getBusArrangement(direction.raw(), index, &mut arrangement)
        })?;
        Ok(arrangement)
    }

    /// Whether the processor accepts 64-bit samples.
    #[must_use]
    pub fn can_process_f64(&self, main: &MainThread) -> bool {
        let _scope = self.scoped(main);
        // SAFETY: a plain query on a live processor.
        is_ok(unsafe { self.processor.canProcessSampleSize(K_SAMPLE_64_INT) })
    }

    /// `IAudioProcessor::setupProcessing`. `Ok(false)` is `kResultFalse`, which
    /// the SDK lets a plugin answer without failing.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists; any other refusal.
    ///
    /// What was set up (the sample size and the longest block) is remembered
    /// when the plugin does not fail the call (`Ok(true)` or `Ok(false)`: a
    /// `kResultFalse` setup is tolerated, as the host's activation expects): the
    /// audio half refuses a block that does not fit it. A call that errors
    /// leaves no setup, and the previous one is forgotten when the call is made.
    pub fn setup_processing(
        &self,
        main: &MainThread,
        mut setup: ProcessSetup,
    ) -> Result<bool, Vst3Error> {
        let _scope = self.scoped(main);
        let mut state = self.shared.lock_for_control()?;
        state.setup = None;
        // SAFETY: `setup` is a valid in-parameter for the call, and the claim is
        // held with no audio half out.
        let result = check_bool(unsafe { self.processor.setupProcessing(&mut setup) });
        if result.is_ok() {
            state.setup = Some(Setup {
                symbolic_size: setup.symbolicSampleSize,
                max_block: usize::try_from(setup.maxSamplesPerBlock).unwrap_or(0),
            });
        }
        result
    }

    /// `IAudioProcessor::getLatencySamples`.
    ///
    /// Takes no `&MainThread`, deliberately: a plain getter that the host reads
    /// beside the processing figures. The SDK annotates both this and
    /// `getTailSamples` `[UI-thread & Setup Done]`, so every caller in the host
    /// crate holds a token anyway; the FFI signature leaves them tokenless.
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        // SAFETY: a plain getter on a live processor.
        unsafe { self.processor.getLatencySamples() }
    }

    /// `IAudioProcessor::getTailSamples`.
    #[must_use]
    pub fn tail_samples(&self) -> u32 {
        // SAFETY: a plain getter on a live processor.
        unsafe { self.processor.getTailSamples() }
    }

    /// `IPrefetchableSupport::getPrefetchableSupport`, if the processor has the
    /// interface and answers.
    #[must_use]
    pub fn prefetchable_support(&self, main: &MainThread) -> Option<crate::PrefetchSupport> {
        let _scope = self.scoped(main);
        let proc = self.ext.prefetchable_support.as_ref()?;
        let mut support: u32 = 0;
        // SAFETY: `support` is a valid out-parameter.
        check(unsafe { proc.getPrefetchableSupport(&mut support) }).ok()?;
        Some(crate::PrefetchSupport::from_raw(support))
    }

    /// `IAudioPresentationLatency`, if the processor has it. `false` without.
    pub fn set_audio_presentation_latency(
        &self,
        main: &MainThread,
        direction: BusDirection,
        bus_index: i32,
        latency_samples: u32,
    ) -> bool {
        let _scope = self.scoped(main);
        match &self.ext.audio_presentation_latency {
            Some(p) => {
                // SAFETY: a plain call on a live interface.
                unsafe {
                    p.setAudioPresentationLatencySamples(
                        direction.raw(),
                        bus_index,
                        latency_samples,
                    );
                }
                true
            }
            None => false,
        }
    }

    // ── State ────────────────────────────────────────────────────────────────

    /// The component's saved state.
    ///
    /// `kNotImplemented` is "this plugin keeps no state here", not a failure:
    /// the bytes written so far (usually none) are returned.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn component_state(&self, main: &MainThread) -> Result<Vec<u8>, Vst3Error> {
        let _scope = self.scoped(main);
        let stream = BStream::new();
        let stream_ptr = stream
            .as_com_ref::<IBStream>()
            .ok_or(Vst3Error::InternalError)?;
        // SAFETY: the stream is a live host object that `stream` keeps alive
        // across the call.
        state_ok(unsafe { self.component.getState(stream_ptr.as_ptr()) })?;
        Ok(stream.data())
    }

    /// Restores the component's state.
    ///
    /// # Errors
    ///
    /// The plugin's refusal (`kNotImplemented` is not one).
    pub fn set_component_state(&self, main: &MainThread, data: &[u8]) -> Result<(), Vst3Error> {
        let _scope = self.scoped(main);
        let stream = BStream::from_data(data.to_vec());
        let stream_ptr = stream
            .as_com_ref::<IBStream>()
            .ok_or(Vst3Error::InternalError)?;
        // SAFETY: the stream is a live host object that `stream` keeps alive
        // across the call.
        state_ok(unsafe { self.component.setState(stream_ptr.as_ptr()) })
    }

    /// The controller's own saved state, `None` for a plugin without a
    /// controller, a refusal or an empty state.
    #[must_use]
    pub fn controller_state(&self, main: &MainThread) -> Option<Vec<u8>> {
        let _scope = self.scoped(main);
        let ctrl = self.controller.get()?;
        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        // SAFETY: the stream is live for the call, kept by `stream`.
        state_ok(unsafe { ctrl.getState(stream_ptr.as_ptr()) }).ok()?;
        let data = stream.data();
        (!data.is_empty()).then_some(data)
    }

    /// Syncs the controller with the component's state
    /// (`IEditController::setComponentState`); the answer is ignored, as the
    /// SDK's own hosts do.
    pub fn sync_controller_to_component_state(&self, main: &MainThread, data: &[u8]) {
        let _scope = self.scoped(main);
        if data.is_empty() {
            return;
        }
        let Some(ctrl) = self.controller.get() else {
            return;
        };
        let stream = BStream::from_data(data.to_vec());
        if let Some(stream_ref) = stream.as_com_ref::<IBStream>() {
            // SAFETY: the stream is live for the call, kept by `stream`.
            unsafe {
                let _ = ctrl.setComponentState(stream_ref.as_ptr());
            }
        }
    }

    /// Restores the controller's own state; the answer is ignored.
    pub fn set_controller_state(&self, main: &MainThread, data: &[u8]) {
        let _scope = self.scoped(main);
        let Some(ctrl) = self.controller.get() else {
            return;
        };
        let stream = BStream::from_data(data.to_vec());
        if let Some(stream_ref) = stream.as_com_ref::<IBStream>() {
            // SAFETY: the stream is live for the call, kept by `stream`.
            unsafe {
                let _ = ctrl.setState(stream_ref.as_ptr());
            }
        }
    }
}

/// Whether a `getState` / `setState` result counts as success.
///
/// **`kNotImplemented`, not `kResultFalse`** — and getting this backwards is not
/// a hypothetical. The SDK's own `Component` base returns `kNotImplemented` from
/// both methods (`vstcomponent.cpp:159,165`), so *every* plugin that does not
/// override state answers that way. Accepting `kResultOk || kResultFalse`
/// instead rejects exactly those plugins: saving a project containing one fails
/// with a `PluginError`.
///
/// The list matches `verify` in the SDK's own preset writer
/// (`vstpresetfile.cpp:53`), which is the closest thing to a reference host for
/// this call and accepts `kResultOk || kNotImplemented`.
///
/// `kResultFalse` is deliberately *not* here. Unlike the state methods, where
/// "I have none" is the common honest answer, a plugin that actively fails a
/// state round-trip is reporting the blob is bad — and silently returning an
/// empty one would persist a project that cannot be restored.
pub(super) fn state_ok(code: vst3::Steinberg::tresult) -> Result<(), Vst3Error> {
    match check(code) {
        Err(Vst3Error::NotImplemented) => Ok(()),
        other => other,
    }
}

/// A bus's `channelCount` as a width: negative is 0 and one past `u16::MAX`
/// saturates, never wraps.
fn bus_width(count: i32) -> u16 {
    u16::try_from(count.max(0)).unwrap_or(u16::MAX)
}

/// `(index, count)` of the first count in `counts` no layout holds.
fn first_unrepresentable(counts: impl IntoIterator<Item = i32>) -> Option<(i32, i32)> {
    counts
        .into_iter()
        .zip(0..)
        .find(|&(count, _)| u16::try_from(count).is_err())
        .map(|(count, index)| (index, count))
}

#[cfg(test)]
mod width_tests {
    use super::*;

    /// A bus given a width past `u16::MAX`, or a negative one, is found with
    /// its index; the widest a layout holds is not.
    ///
    /// Mutation (run): `first_unrepresentable` returning `None` always fails
    /// the refusals; testing only `count > 65_535` fails the negative one.
    #[test]
    fn the_first_bus_no_layout_holds_is_found_with_its_index() {
        assert_eq!(first_unrepresentable([2, 65_535, 0]), None);
        assert_eq!(
            first_unrepresentable([2, 65_536, 70_000]),
            Some((1, 65_536))
        );
        assert_eq!(first_unrepresentable([2, 2, -1]), Some((2, -1)));
        assert_eq!(first_unrepresentable([i32::MAX]), Some((0, i32::MAX)));
    }
}
