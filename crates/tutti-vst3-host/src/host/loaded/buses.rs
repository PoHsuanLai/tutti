//! Bus configuration: the layout read back from the component, topologies, latency and tail.

use super::*;
use crate::host::instance::bus_layout;
use tutti_plugin_types::{BusRole, MainThread, PluginBus, PluginBuses};

impl Vst3Loaded {
    /// Reads the current processing latency directly from
    /// `IAudioProcessor::getLatencySamples`. Call this at load time and
    /// whenever [`RestartOutcome::latency_changed`] is set to get the fresh
    /// value for PDC.
    pub fn latency_samples(&self, _main: &MainThread) -> u32 {
        self.plugin.latency_samples()
    }

    /// Reads the plugin's tail length from `IAudioProcessor::getTailSamples` —
    /// how long it keeps sounding after its input goes silent.
    ///
    /// The raw count, so `0` (no tail) and the saturating `u32::MAX` (an
    /// effectively unbounded one) both reach the caller as the plugin stated
    /// them. `PluginTail::from_vst3_samples` is what turns those into the shared
    /// vocabulary; this stays at the ABI's own type so nothing is decided here.
    pub fn tail_samples(&self, _main: &MainThread) -> u32 {
        self.plugin.tail_samples()
    }

    /// Queries the plugin's offline/prefetch processing support via
    /// `IPrefetchableSupport`. `None` if the plugin doesn't implement the
    /// interface. Main/UI thread.
    pub fn prefetchable_support(&self, main: &MainThread) -> Option<PrefetchSupport> {
        self.plugin.prefetchable_support(main)
    }

    /// Tells the plugin the downstream presentation latency (in samples) for a
    /// given bus, via `IAudioPresentationLatency` — the delay between the
    /// plugin's output and what the listener hears, so latency-aware plugins can
    /// compensate. `dir` is the bus's direction.
    /// Returns `true` if delivered; no-op if the plugin doesn't implement the
    /// interface. Main/UI thread.
    pub fn set_audio_presentation_latency(
        &mut self,
        main: &MainThread,
        dir: crate::BusDirection,
        bus_index: i32,
        latency_samples: u32,
    ) -> bool {
        self.plugin
            .set_audio_presentation_latency(main, dir, bus_index, latency_samples)
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
    pub(crate) fn reconcile_bus_counts(&mut self, main: &MainThread) {
        if let Some(layout) = self
            .plugin
            .audio_bus_channel_count(main, BusDirection::Input)
        {
            // `PluginInfo` carries raw usize channel counts; take the count at
            // this boundary.
            let ch = layout.count() as usize;
            if ch != self.info.num_inputs {
                self.info = self.info.clone().audio_io(ch, self.info.num_outputs);
            }
        }
        if let Some(layout) = self
            .plugin
            .audio_bus_channel_count(main, BusDirection::Output)
        {
            let ch = layout.count() as usize;
            if ch != self.info.num_outputs {
                self.info = self.info.clone().audio_io(self.info.num_inputs, ch);
            }
        }
        // Re-enumerate the full per-bus layout — `initialize` may have changed
        // bus counts, and aux/sidechain buses are only visible post-init on
        // some plugins.
        let input_bus_channels = self.plugin.audio_bus_channels(main, BusDirection::Input);
        let output_bus_channels = self.plugin.audio_bus_channels(main, BusDirection::Output);
        self.info = self
            .info
            .clone()
            .bus_channels(input_bus_channels, output_bus_channels);

        // Event buses need the same treatment, and for the same reason: a
        // component adds them in `initialize` (`addEventInput`), so the
        // pre-init query in `build_plugin_info` always sees zero. Without this,
        // every instrument reports `has_midi_input == false` and a host that
        // gates MIDI delivery on it never sends the plugin a single note.
        let receives_midi = self
            .plugin
            .bus_count(main, MediaType::Event, BusDirection::Input)
            > 0;
        let emits_midi = self
            .plugin
            .bus_count(main, MediaType::Event, BusDirection::Output)
            > 0;
        self.info = self
            .info
            .clone()
            .midi(receives_midi)
            .midi_output(emits_midi);
        // `canProcessSampleSize` is `[UI-thread & (Initialized | Connected)]`,
        // and a plugin may settle its answer in `initialize`.
        let supports_f64 = self.plugin.can_process_f64(main);
        self.info = self.info.clone().f64_support(supports_f64);
    }

    /// The plugin's audio buses in `direction`, as it runs them.
    ///
    /// Each bus is its width as the last reconcile read it (the widths the
    /// buffers are sized to), its arrangement queried live from
    /// `getBusArrangement` (a plugin may restructure its buses after
    /// negotiation; `None` when the arrangement names a speaker this
    /// vocabulary cannot, or the query failed: both mean "cannot route this bus
    /// by speaker"), and its role from `busType`: `kMain` is
    /// [`BusRole::Main`], `kAux` (or a bus whose info cannot be read)
    /// [`BusRole::Aux`]. VST3 marks no bus a sidechain.
    pub fn buses(&self, main: &MainThread, direction: BusDirection) -> PluginBuses {
        let widths = match direction {
            BusDirection::Input => &self.info.input_bus_channels,
            BusDirection::Output => &self.info.output_bus_channels,
        };
        let arrangements = self.read_back_arrangements(main, direction, widths.len());
        widths
            .iter()
            .zip(arrangements)
            .enumerate()
            .map(|(i, (&width, arrangement))| {
                let kind = self
                    .plugin
                    .bus_info(main, MediaType::Audio, direction, i as i32)
                    .ok()
                    .and_then(|bus| bus.bus_type());
                let role = if kind == Some(crate::BusType::Main) {
                    BusRole::Main
                } else {
                    BusRole::Aux
                };
                PluginBus::new(bus_layout(width), role).placed(usable_topology(arrangement, width))
            })
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
        main: &MainThread,
        direction: BusDirection,
        num_buses: usize,
    ) -> Vec<ChannelTopology> {
        (0..num_buses)
            .map(
                |i| match self.plugin.bus_arrangement(main, direction, i as i32) {
                    Ok(arr) => crate::host::speakers::from_arrangement(arr),
                    Err(_) => ChannelTopology::new([]),
                },
            )
            .collect()
    }
}
