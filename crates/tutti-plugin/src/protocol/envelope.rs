//! Wire-envelope enums — the host↔server message types.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::metadata::PluginInfo;
use super::parameters::ParameterInfo;
use super::process::{
    AudioProcessedFullData, AudioProcessedMidiData, ProcessAudioFullData, ProcessAudioMidiData,
};
use super::sample::SampleFormat;
use super::shm::SlabLayout;

fn default_block_size() -> usize {
    512
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMessage {
    /// Lightweight metadata-only probe — reads factory/descriptor info
    /// without activating the plugin (avoids license dialogs).
    ProbePlugin {
        path: PathBuf,
    },
    LoadPlugin {
        path: PathBuf,
        sample_rate: f64,
        #[serde(default = "default_block_size")]
        block_size: usize,
        #[serde(default)]
        preferred_format: SampleFormat,
        #[serde(default)]
        shm_name: String,
    },
    UnloadPlugin,
    ProcessAudio {
        buffer_id: u32,
        num_samples: usize,
    },
    ProcessAudioMidi(Box<ProcessAudioMidiData>),
    ProcessAudioFull(Box<ProcessAudioFullData>),
    SetParameter {
        param_id: u32,
        value: f32,
    },
    GetParameter {
        param_id: u32,
    },
    GetParameterList,
    GetParameterInfo {
        param_id: u32,
    },
    SetSampleRate {
        rate: f64,
    },
    Reset,
    SaveState,
    LoadState {
        data: Vec<u8>,
    },
    OpenEditor {
        parent_handle: u64,
    },
    CloseEditor,
    EditorIdle,
    SetupSharedMemory {
        shm_name: String,
        layout: SlabLayout,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BridgeMessage {
    PluginLoaded {
        metadata: Box<PluginInfo>,
        negotiated_format: SampleFormat,
    },
    PluginUnloaded,
    AudioProcessed {
        latency_us: u64,
    },
    AudioProcessedMidi(Box<AudioProcessedMidiData>),
    AudioProcessedFull(Box<AudioProcessedFullData>),
    ParameterValue {
        value: Option<f32>,
    },
    ParameterList {
        parameters: Vec<ParameterInfo>,
    },
    ParameterInfoResponse {
        info: Option<ParameterInfo>,
    },
    StateData {
        data: Vec<u8>,
    },
    EditorOpened {
        width: u32,
        height: u32,
    },
    EditorClosed,
    ParameterChanged {
        index: i32,
        value: f32,
    },
    /// Plugin reported a latency change at runtime. Host updates the
    /// corresponding `PluginClient::set_latency` so `AudioUnit::latency()`
    /// reports the new value. Note: does NOT trigger PDC re-analysis;
    /// the graph must be committed again for compensation to update.
    LatencyChanged {
        samples: usize,
    },
    SharedMemoryReady,
    Error {
        message: String,
    },
    Ready,
    Shutdown,
}
