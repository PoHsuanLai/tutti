//! Bus configuration: the layout read back from the component, topologies, latency and tail.

use super::*;

impl Vst3Loaded {
    /// Reads the current processing latency directly from
    /// `IAudioProcessor::getLatencySamples`. Call this at load time and
    /// whenever [`RestartOutcome::latency_changed`] is set to get the fresh
    /// value for PDC.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn latency_samples(&self) -> u32 {
        tutti_plugin_types::assert_main_thread();
        self.plugin.latency_samples()
    }

    /// Reads the plugin's tail length from `IAudioProcessor::getTailSamples` —
    /// how long it keeps sounding after its input goes silent.
    ///
    /// The raw count, so `0` (no tail) and the saturating `u32::MAX` (an
    /// effectively unbounded one) both reach the caller as the plugin stated
    /// them. `PluginTail::from_vst3_samples` is what turns those into the shared
    /// vocabulary; this stays at the ABI's own type so nothing is decided here.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn tail_samples(&self) -> u32 {
        tutti_plugin_types::assert_main_thread();
        self.plugin.tail_samples()
    }

    /// Queries the plugin's offline/prefetch processing support via
    /// `IPrefetchableSupport`. `None` if the plugin doesn't implement the
    /// interface. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn prefetchable_support(&self) -> Option<PrefetchSupport> {
        tutti_plugin_types::assert_main_thread();
        self.plugin
            .prefetchable_support()
            .map(PrefetchSupport::from_raw)
    }

    /// Tells the plugin the downstream presentation latency (in samples) for a
    /// given bus, via `IAudioPresentationLatency` — the delay between the
    /// plugin's output and what the listener hears, so latency-aware plugins can
    /// compensate. `dir` is the VST3 `kInput` / `kOutput` bus-direction constant.
    /// Returns `true` if delivered; no-op if the plugin doesn't implement the
    /// interface. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn set_audio_presentation_latency(
        &mut self,
        dir: i32,
        bus_index: i32,
        latency_samples: u32,
    ) -> bool {
        tutti_plugin_types::assert_main_thread();
        self.plugin
            .set_audio_presentation_latency(dir, bus_index, latency_samples)
    }

    /// Re-query bus counts, the MIDI buses and the 64-bit answer from the
    /// component — `initialize` may have changed them (some plugins don't
    /// declare bus counts until after init), and it is the first point the SDK
    /// allows asking.
    ///
    /// `pub(crate)` only so the activation sequence can call it from inside the
    /// deactivate/reactivate cycle. Deliberately not public:
    /// on a live instance this must never be reached on its own, or the
    /// re-enumeration runs mid-activation.
    pub(crate) fn reconcile_bus_counts(&mut self) {
        if let Some(layout) = self.plugin.audio_bus_channel_count(BusDirection::Input) {
            // `PluginInfo` carries raw usize channel counts; take the count at
            // this boundary.
            let ch = layout.count() as usize;
            if ch != self.info.num_inputs {
                self.info = self.info.clone().audio_io(ch, self.info.num_outputs);
            }
        }
        if let Some(layout) = self.plugin.audio_bus_channel_count(BusDirection::Output) {
            let ch = layout.count() as usize;
            if ch != self.info.num_outputs {
                self.info = self.info.clone().audio_io(self.info.num_inputs, ch);
            }
        }
        // Re-enumerate the full per-bus layout — `initialize` may have changed
        // bus counts, and aux/sidechain buses are only visible post-init on
        // some plugins.
        let input_bus_channels = self.plugin.audio_bus_channels(BusDirection::Input);
        let output_bus_channels = self.plugin.audio_bus_channels(BusDirection::Output);
        self.info = self
            .info
            .clone()
            .bus_channels(input_bus_channels, output_bus_channels);

        // Event buses need the same treatment, and for the same reason: a
        // component adds them in `initialize` (`addEventInput`), so the
        // pre-init query in `build_plugin_info` always sees zero. Without this,
        // every instrument reports `has_midi_input == false` and a host that
        // gates MIDI delivery on it never sends the plugin a single note.
        let receives_midi = self.plugin.bus_count(MediaType::Event, BusDirection::Input) > 0;
        let emits_midi = self
            .plugin
            .bus_count(MediaType::Event, BusDirection::Output)
            > 0;
        self.info = self
            .info
            .clone()
            .midi(receives_midi)
            .midi_output(emits_midi);
        // `canProcessSampleSize` is `[UI-thread & (Initialized | Connected)]`,
        // and a plugin may settle its answer in `initialize`.
        let supports_f64 = self.plugin.can_process_f64();
        self.info = self.info.clone().f64_support(supports_f64);
    }

    /// The channel topology of each **input** bus, as the plugin runs it.
    /// See [`output_bus_topologies`](Self::output_bus_topologies).
    ///
    /// Two named methods rather than one taking a direction, so a caller does
    /// not need the crate's private `kInput`/`kOutput` constants and cannot
    /// pass an `i32` that is neither.
    pub fn input_bus_topologies(&self) -> Vec<Option<ChannelTopology>> {
        self.bus_topologies(BusDirection::Input)
    }

    /// The channel topology of each **output** bus.
    ///
    /// Queried live from `getBusArrangement` rather than cached from
    /// negotiation, because a plugin may restructure its buses after it. An
    /// entry is `None` when the plugin's arrangement names a speaker this
    /// vocabulary cannot, or when the query failed: both mean "cannot route
    /// this bus by speaker". A bus whose arrangement is fully named yields
    /// `Some`, and its width always equals the count reported beside it.
    pub fn output_bus_topologies(&self) -> Vec<Option<ChannelTopology>> {
        self.bus_topologies(BusDirection::Output)
    }

    pub(super) fn bus_topologies(&self, direction: BusDirection) -> Vec<Option<ChannelTopology>> {
        let widths = self.plugin.audio_bus_channels(direction);
        self.read_back_arrangements(direction, widths.len())
            .into_iter()
            .zip(widths)
            .map(|(topology, width)| usable_topology(topology, width))
            .collect()
    }

    /// Reads the plugin's chosen layout for each of `num_buses` buses in
    /// `direction`. A bus whose query fails contributes an empty topology, so
    /// the length always equals `num_buses`.
    ///
    /// Decoded into a [`ChannelTopology`] rather than straight to a count: the
    /// mask carries *which speaker each channel is*, which a popcount discards.
    pub(in crate::host) fn read_back_arrangements(
        &self,
        direction: BusDirection,
        num_buses: usize,
    ) -> Vec<ChannelTopology> {
        (0..num_buses)
            .map(|i| match self.plugin.bus_arrangement(direction, i as i32) {
                Ok(arr) => crate::host::speakers::from_arrangement(arr),
                Err(_) => ChannelTopology::new([]),
            })
            .collect()
    }
}
