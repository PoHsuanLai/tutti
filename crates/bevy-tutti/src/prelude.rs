//! Public surface re-exports.
//!
//! `lib.rs` does `pub use prelude::*` so all symbols continue to resolve
//! at `bevy_tutti::Foo`. Callers import from the crate root, not from
//! `bevy_tutti::prelude` directly — keep this module `pub(crate)`.

pub use crate::loader::{
    TuttiLoader, TuttiLoaderError, TuttiStreamingLoader, TuttiStreamingLoaderError,
};
pub use tutti::core::WaveAsset;
#[cfg(feature = "soundfont")]
pub use tutti::synth::SoundFontAsset;
#[cfg(feature = "sampler")]
pub use tutti::sampler::StreamingSample;

pub use crate::playback::{
    AudioEmitter, AudioPlaybackState, AudioVolume, DespawnOnFinish, PlayAudio, TuttiPlaybackPlugin,
};
#[cfg(feature = "sampler")]
pub use crate::playback::{
    audio_cleanup_system, audio_parameter_sync_system, audio_playback_system,
};

#[cfg(feature = "spatial")]
pub use crate::spatial::{
    spatial_audio_sync_system, AttenuationModel, AudioListener, SpatialAudio, TuttiSpatialPlugin,
};

#[cfg(feature = "midi")]
pub use crate::midi::components::{MidiReceiver, MidiSequence, MidiSequenceNote};
#[cfg(feature = "midi")]
pub use crate::midi::events::MidiInputEvent;
#[cfg(feature = "midi")]
pub use crate::midi::systems::{
    midi_input_event_system, midi_routing_sync_system, midi_sequence_setup_system,
    midi_sequence_tick_system, MidiSequenceState,
};

#[cfg(feature = "midi-hardware")]
pub use crate::midi::components::{ConnectMidiDevice, DisconnectMidiDevice};
#[cfg(feature = "midi-hardware")]
pub use crate::midi::events::MidiDeviceEvent;
#[cfg(feature = "midi-hardware")]
pub use crate::midi::systems::{midi_device_connect_system, midi_device_poll_system};

#[cfg(feature = "mpe")]
pub use crate::midi::components::MpeReceiver;
#[cfg(feature = "mpe")]
pub use crate::midi::systems::{MpeExpressionResource, MpeModeConfig};
#[cfg(feature = "mpe")]
pub use tutti::midi::{MpeMode, MpeZone, MpeZoneConfig};

#[cfg(feature = "soundfont")]
pub use crate::soundfont::{soundfont_playback_system, PlaySoundFont, TuttiSoundFontPlugin};

pub use crate::metering::{metering_sync_system, MasterMeterLevels};
pub use crate::transport::{transport_sync_system, TransportState};

pub use crate::device_state::{device_state_sync_system, AudioDeviceState};

#[cfg(feature = "sampler")]
pub use crate::content_bounds::{content_bounds_sync_system, ContentBounds};

pub use tutti::{
    DeviceInfo, NodeId, TuttiDriver, TuttiEngine, TuttiEngineBuilder, TuttiGraph, Wave,
};

// Entity-as-node ECS primitives — re-exported from `tutti` so apps don't
// need to reach into `tutti::core::ecs` directly.
pub use tutti::{AudioNode, Mute, NodeKind, Pan, PluginParam, Volume};
#[cfg(feature = "dsp")]
pub use tutti::{
    Attack, CompressorRatio, DelayTime, Feedback, FilterQ, Frequency, GainDb, ModDepth, ModRate,
    Release, ReverbDamping, ReverbRoomSize, ThresholdDb, WetMix,
};
#[cfg(feature = "sampler")]
pub use tutti::{SamplerLooping, SamplerSpeed};

pub use crate::graph::{
    commit_graph, crossfade_audio_node, reconcile_audio_routing, reconcile_node_despawn,
    reconcile_params, reconcile_sidechain_links, AudioFedBy, AudioFeedsTo, GraphDirty,
    GraphReconcileSystems, SidechainOf, SidechainSources, SpawnAudioNode, TuttiGraphPlugin,
};
#[cfg(feature = "sampler")]
pub use crate::graph::{
    poll_wave_imports, promote_pending_samplers, reconcile_sampler_params, PendingSamplerLoad,
    WaveImportQueue,
};
#[cfg(feature = "midi")]
pub use crate::graph::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi};
#[cfg(feature = "plugin")]
pub use crate::graph::reconcile_plugin_params;

#[cfg(all(feature = "plugin", feature = "vst2"))]
pub use crate::vst2_load::{process_pending_vst2_builds, PendingVst2Build};

#[cfg(feature = "automation")]
pub use crate::automation::{
    automation_lane_system, reconcile_automation_writes, AddAutomationLane, AutomationDrivesParam,
    AutomationLaneEmitter, AutomationLaneNode, AutomationParam, TuttiAutomationPlugin,
};

#[cfg(feature = "midi")]
pub use tutti::midi::{MidiEvent, Note};
#[cfg(feature = "midi")]
pub use tutti::midi_runtime::MidiBus;
#[cfg(feature = "midi-hardware")]
pub use tutti::midi::MidiIo;

#[cfg(feature = "plugin")]
pub use crate::plugin_host::{
    plugin_crash_detect_system, plugin_editor_attach_system, plugin_editor_close_system,
    plugin_editor_idle_system, plugin_editor_open_system, plugin_editor_resize_request_system,
    plugin_editor_window_close_system, plugin_editor_window_resize_system, ClosePluginEditor,
    OpenPluginEditor, PendingPluginEditor, PluginEditorOpen, PluginEmitter, TuttiHostingPlugin,
};
#[cfg(feature = "plugin")]
pub use tutti::plugin::catalog::{
    JsonCatalog, PluginCatalog, PluginRecord, PluginScanner, Plugins, PluginsConfig, ScanHandle,
    ScanPhase, ScanProgress, ScanResult,
};
#[cfg(feature = "plugin")]
pub use tutti::plugin::handles::PluginHandle;
#[cfg(feature = "plugin")]
pub use tutti::plugin::metadata::{ParameterFlags, ParameterInfo};

#[cfg(feature = "sampler")]
pub use crate::recording::{
    recording_start_system, recording_stop_system, RecordingActive, TuttiRecordingPlugin,
    RecordingResult, StartRecording, StopRecording,
};
#[cfg(feature = "sampler")]
pub use tutti::sampler::capture::{
    Mode as RecordingMode, Recorded as RecordedData, Source as RecordingSource,
};

#[cfg(feature = "analysis")]
pub use crate::analysis::{
    live_analysis_control_system, live_analysis_sync_system, TuttiAnalysisPlugin, DisableLiveAnalysis,
    EnableLiveAnalysis, LiveAnalysisData,
};

#[cfg(feature = "export")]
pub use crate::export::{
    export_poll_system, export_start_system, ExportComplete, ExportFailed, ExportInProgress,
    TuttiExportPlugin, StartExport,
};
#[cfg(feature = "export")]
pub use tutti::export::{
    AudioFormat, Handle as ExportHandle, Normalize, State as ExportState, Written,
};

#[cfg(feature = "sampler")]
pub use crate::audio_input::{
    audio_input_control_system, audio_input_sync_system, AudioInputDeviceInfo, TuttiAudioInputPlugin,
    AudioInputState, DisableAudioInput, EnableAudioInput,
};

#[cfg(feature = "sampler")]
pub use crate::time_stretch::{
    time_stretch_sync_system, TimeStretch, TimeStretchControl, TuttiTimeStretchPlugin,
};
#[cfg(feature = "sampler")]
pub use tutti::sampler::stretch::Unit as TimeStretchUnit;

pub use crate::dsp::{dsp_lfo_system, AddLfo, TuttiDspPlugin};
pub use tutti::units::{LfoMode, LfoNode, LfoShape};
#[cfg(feature = "dsp")]
pub use crate::dsp::{
    dsp_chorus_system, dsp_compressor_system, dsp_delay_system, dsp_filter_system,
    dsp_gate_system, dsp_reverb_system, AddChorus, AddCompressor, AddDelay, AddFilter, AddGate,
    AddReverb,
};
#[cfg(feature = "dsp")]
pub use tutti::units::{
    ChorusNode, Compressor, Gate, StereoDelayLineNode, StereoSvfFilterNode, SvfType,
};

// Resource newtypes (defined in `crate::resources`).
pub use crate::resources::{
    AudioConfig, MeteringRes, TransportRes, TuttiDriverRes, TuttiGraphRes,
};
#[cfg(feature = "midi")]
pub use crate::resources::MidiBusRes;
#[cfg(feature = "midi-hardware")]
pub use crate::resources::MidiIoRes;
#[cfg(feature = "sampler")]
pub use crate::resources::SamplerRes;
#[cfg(feature = "soundfont")]
pub use crate::resources::SoundFontRes;
#[cfg(feature = "analysis")]
pub use crate::resources::AnalysisRes;
#[cfg(feature = "plugin")]
pub use crate::resources::PluginEditorMainThread;
#[cfg(feature = "plugin")]
pub use crate::resources::PluginsRes;
