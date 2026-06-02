//! Plugin metadata — name, vendor, I/O counts, editor info.
//!
//! Wire data: rides on `BridgeMessage::PluginLoaded`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioIO {
    pub inputs: usize,
    pub outputs: usize,
}

impl AudioIO {
    pub fn stereo() -> Self {
        Self {
            inputs: 2,
            outputs: 2,
        }
    }
}

/// Direction of an audio bus, as seen by the plugin.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum BusDirection {
    #[default]
    Input,
    Output,
}

/// One audio bus in a plugin's I/O layout. The bus list (see
/// [`PluginInfo::buses`] / [`crate::protocol::SlabLayout::buses`]) is ordered
/// per direction by bus index; bus 0 of each direction is the main bus and
/// later input buses are aux/sidechain inputs.
///
/// Wire-format addition for multi-bus / sidechain hosting. Every field
/// deserializes from older peers that never sent a bus list (an empty `buses`
/// vector == today's single flat main bus), so the type is fully
/// back-compatible.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusLayout {
    pub direction: BusDirection,
    pub channels: usize,
}

impl BusLayout {
    pub fn input(channels: usize) -> Self {
        Self {
            direction: BusDirection::Input,
            channels,
        }
    }

    pub fn output(channels: usize) -> Self {
        Self {
            direction: BusDirection::Output,
            channels,
        }
    }
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub audio_io: AudioIO,
    pub receives_midi: bool,
    pub has_editor: bool,
    pub editor_size: Option<(u32, u32)>,
    pub latency_samples: usize,
    #[serde(default)]
    pub supports_f64: bool,
    /// Full per-bus I/O layout, ordered per direction by bus index.
    ///
    /// `#[serde(default)]` → an older peer that never sent a bus list
    /// deserializes to an empty vector, which callers treat as today's
    /// single-bus behaviour (main in = `audio_io.inputs`, main out =
    /// `audio_io.outputs`, no aux/sidechain buses). The flat `audio_io` counts
    /// remain authoritative for bus 0 and total back-compat.
    #[serde(default)]
    pub buses: Vec<BusLayout>,
}

impl PluginInfo {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            vendor: String::new(),
            version: "1.0.0".to_string(),
            audio_io: AudioIO::stereo(),
            receives_midi: false,
            has_editor: false,
            editor_size: None,
            latency_samples: 0,
            supports_f64: false,
            buses: Vec::new(),
        }
    }

    pub fn vendor(mut self, vendor: impl Into<String>) -> Self {
        self.vendor = vendor.into();
        self
    }

    pub fn author(mut self, author: impl Into<String>) -> Self {
        self.vendor = author.into();
        self
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn audio_io(mut self, inputs: usize, outputs: usize) -> Self {
        self.audio_io = AudioIO { inputs, outputs };
        self
    }

    pub fn midi(mut self, receives_midi: bool) -> Self {
        self.receives_midi = receives_midi;
        self
    }

    pub fn editor(mut self, has_editor: bool, size: Option<(u32, u32)>) -> Self {
        self.has_editor = has_editor;
        self.editor_size = size;
        self
    }

    pub fn latency(mut self, samples: usize) -> Self {
        self.latency_samples = samples;
        self
    }

    pub fn f64_support(mut self, supports_f64: bool) -> Self {
        self.supports_f64 = supports_f64;
        self
    }

    /// Record the full per-bus I/O layout. An empty list keeps single-bus
    /// behaviour (the `audio_io` counts stand alone).
    pub fn buses(mut self, buses: Vec<BusLayout>) -> Self {
        self.buses = buses;
        self
    }

    /// Per-bus channel counts for input buses, in bus-index order. Falls back
    /// to a single main bus of `audio_io.inputs` when no bus list was sent
    /// (single-bus legacy).
    pub fn input_bus_channels(&self) -> Vec<usize> {
        if self.buses.is_empty() {
            return vec![self.audio_io.inputs];
        }
        self.buses
            .iter()
            .filter(|b| b.direction == BusDirection::Input)
            .map(|b| b.channels)
            .collect()
    }

    /// Per-bus channel counts for output buses, in bus-index order. Falls back
    /// to a single main bus of `audio_io.outputs` when no bus list was sent.
    pub fn output_bus_channels(&self) -> Vec<usize> {
        if self.buses.is_empty() {
            return vec![self.audio_io.outputs];
        }
        self.buses
            .iter()
            .filter(|b| b.direction == BusDirection::Output)
            .map(|b| b.channels)
            .collect()
    }

    /// Returns true if this plugin is a synthesizer/instrument (receives MIDI).
    pub fn is_synth(&self) -> bool {
        self.receives_midi
    }

    /// Returns true if this plugin is an effect (does not receive MIDI).
    pub fn is_effect(&self) -> bool {
        !self.receives_midi
    }
}

#[cfg(test)]
mod bus_layout_tests {
    use super::*;

    /// Legacy mirror of `PluginInfo` *without* the trailing
    /// `supports_f64` / `buses` fields — the exact shape an older build would
    /// serialize. Lets the tests below pin down what each wire format does
    /// when an old payload meets the new struct.
    #[derive(Serialize)]
    struct LegacyPluginInfo {
        id: String,
        name: String,
        vendor: String,
        version: String,
        audio_io: AudioIO,
        receives_midi: bool,
        has_editor: bool,
        editor_size: Option<(u32, u32)>,
        latency_samples: usize,
    }

    fn legacy() -> LegacyPluginInfo {
        LegacyPluginInfo {
            id: "vst3.x".into(),
            name: "X".into(),
            vendor: String::new(),
            version: "1.0.0".into(),
            audio_io: AudioIO::stereo(),
            receives_midi: false,
            has_editor: false,
            editor_size: None,
            latency_samples: 0,
        }
    }

    /// Same-version bincode round-trip (the actual wire serializer) preserves
    /// an empty bus list, and the accessors fall back to the flat `audio_io`
    /// counts as a single main bus.
    #[test]
    fn empty_buses_round_trips_and_falls_back() {
        let info = PluginInfo::new("vst3.x", "X").audio_io(2, 2);
        assert!(info.buses.is_empty());
        let bytes = bincode::serialize(&info).unwrap();
        let back: PluginInfo = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, info);
        assert!(back.buses.is_empty());
        assert_eq!(back.input_bus_channels(), vec![2]);
        assert_eq!(back.output_bus_channels(), vec![2]);
    }

    /// CROSS-VERSION REALITY CHECK (bincode is NOT self-describing): an old
    /// payload that omits the trailing `#[serde(default)]` fields does **not**
    /// bincode-deserialize into the new struct — `#[serde(default)]` cannot
    /// rescue a non-self-describing format. This documents the wire-format
    /// constraint: host and plugin-server must be built from the same revision
    /// (which is the deployment model — the host launches the bundled
    /// `plugin-server` binary). The pre-existing `supports_f64` field already
    /// carried this same constraint.
    #[test]
    fn bincode_is_not_cross_version_compatible() {
        let legacy_bytes = bincode::serialize(&legacy()).unwrap();
        // Shorter payload (no supports_f64 byte, no buses len) → bincode runs
        // off the end trying to read the new fields.
        assert!(bincode::deserialize::<PluginInfo>(&legacy_bytes).is_err());
    }

    /// A self-describing format (JSON) DOES honour `#[serde(default)]`: an old
    /// payload omitting `supports_f64` / `buses` fills both with their
    /// defaults. (JSON isn't the wire format, but proves the serde attrs are
    /// correct should a self-describing transport ever be used.)
    #[cfg(feature = "json")]
    #[test]
    fn json_honours_serde_default_for_legacy_payload() {
        let legacy_json = serde_json::to_string(&legacy()).unwrap();
        let from_legacy: PluginInfo = serde_json::from_str(&legacy_json).unwrap();
        assert!(from_legacy.buses.is_empty());
        assert!(!from_legacy.supports_f64);
        assert_eq!(from_legacy.input_bus_channels(), vec![2]);
    }

    /// Multi-bus layout round-trips and the per-direction accessors split it
    /// back out in bus-index order.
    #[test]
    fn multi_bus_round_trips() {
        let info = PluginInfo::new("vst3.sc", "Sidechain Comp")
            .audio_io(2, 2)
            .buses(vec![
                BusLayout::input(2),  // main in
                BusLayout::input(1),  // sidechain in
                BusLayout::output(2), // main out
            ]);
        let bytes = bincode::serialize(&info).unwrap();
        let back: PluginInfo = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, info);
        assert_eq!(back.input_bus_channels(), vec![2, 1]);
        assert_eq!(back.output_bus_channels(), vec![2]);
    }
}
