//! Public surface re-exports.
//!
//! `lib.rs` does `pub use prelude::*` so all symbols continue to resolve
//! at `bevy_tutti::Foo`. Callers import from the crate root, not from
//! `bevy_tutti::prelude` directly — keep this module `pub(crate)`.

pub use crate::core::WaveAsset;
pub use crate::loader::{
    TuttiLoader, TuttiLoaderError, TuttiStreamingLoader, TuttiStreamingLoaderError,
};
// Decode-once wave cache (Bevy-native): the single shared `Arc<Wave>` source
// for playback, analysis, and the offline render.
#[cfg(feature = "sampler")]
pub use crate::sampler::StreamingSample;
#[cfg(feature = "soundfont")]
pub use crate::synth::SoundFontAsset;
pub use tutti_wavecache::{poll_wave_cache, WaveCache, WaveCachePlugin, WaveState};

#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::{
    audio_cleanup_system, audio_parameter_sync_system, audio_playback_system, AudioVolume,
    DespawnOnFinish, PlayAudio, TuttiPlaybackPlugin,
};
// `AudioEmitter` / `AudioPlaybackState` are leaf-agnostic value types living in
// tutti-core's ECS hub, so they resolve in the minimal build too.
pub use crate::core::ecs::{AudioEmitter, AudioPlaybackState};

#[cfg(feature = "spatial")]
pub use crate::units::ecs::{
    spatial_audio_sync_system, AttenuationModel, AudioListener, SpatialAudio, TuttiSpatialPlugin,
};

#[cfg(feature = "midi")]
pub use tutti_midi_io::ecs::{MidiReceiver, MidiSequence, MidiSequenceNote};
#[cfg(feature = "midi")]
pub use tutti_midi_io::ecs::MidiInputEvent;
#[cfg(feature = "midi")]
pub use tutti_midi_io::ecs::{
    midi_input_event_system, midi_routing_sync_system, midi_sequence_setup_system,
    midi_sequence_tick_system, MidiSequenceState,
};

#[cfg(feature = "midi-hardware")]
pub use tutti_midi_io::ecs::{ConnectMidiDevice, DisconnectMidiDevice};
#[cfg(feature = "midi-hardware")]
pub use tutti_midi_io::ecs::MidiDeviceEvent;
#[cfg(feature = "midi-hardware")]
pub use tutti_midi_io::ecs::{midi_device_connect_system, midi_device_poll_system};

#[cfg(feature = "mpe")]
pub use tutti_midi_io::ecs::MpeReceiver;
#[cfg(feature = "mpe")]
pub use tutti_midi_io::ecs::{MpeExpressionResource, MpeModeConfig};
#[cfg(feature = "mpe")]
pub use tutti_midi_io::{MpeMode, MpeZone, MpeZoneConfig};

#[cfg(feature = "soundfont")]
pub use tutti_synth::ecs::{soundfont_playback_system, PlaySoundFont, TuttiSoundFontPlugin};

pub use crate::metering::{metering_sync_system, MasterMeterLevels};
pub use crate::transport::{transport_sync_system, TransportState};

pub use crate::device_state::{device_state_sync_system, AudioDeviceState};

#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::ContentBounds;

// The engine types (`TuttiEngine`, `TuttiGraph`, `NodeId`, `Wave`, …) and the
// scalar entity-as-node ECS params (`AudioNode`, `Volume`, `Frequency`, …) are
// re-exported at the crate root by `lib.rs`, and `lib.rs` does `pub use
// prelude::*`, so they are already reachable via `bevy_tutti::*` and
// `bevy_tutti::prelude::*` without re-listing them here.

// Authoring markers + construction-only authored data (in `tutti_core::ecs`).
// The preferred spawn surface: `commands.spawn((FilterNode, Frequency(..),
// FilterQ(..), ..))`; the `#[require]` lists fill defaults and the generic
// `spawn_dsp_node::<FilterNode>` builds the unit from those values.
//
// Only the markers that are actually read are exported: the 6 generic
// `spawn_dsp_node` triggers (Compressor / Gate / Filter / Reverb / Delay /
// Chorus), the LFO trigger, and the 3 type-guard markers
// (Reverb / ConvolutionReverb / Sampler — `Reverb` covers both roles).
// `ChorusNode` is aliased `ChorusNodeMarker` to avoid colliding with the
// concrete `tutti_units::ChorusNode` re-exported below.
pub use crate::core::ecs::{
    BeatSynced, ChorusNode as ChorusNodeMarker, CompressorNode, ConvolutionReverbNode, DelayNode,
    FilterMode, FilterNode, GateNode, LfoNodeMarker, LfoShapeKind, MaxDelay, ReverbNode,
    ReverbTime, SamplerNode, StereoChannels,
};

#[cfg(feature = "plugin")]
pub use tutti_plugin_host::reconcile_plugin_params;
pub use crate::graph::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_audio_routing,
    reconcile_node_despawn, reconcile_params, reconcile_sidechain_links, register_audio_node_types,
    AudioFedBy, AudioFeedsTo, GraphDirty, GraphReconcileSystems, NodeParamEpoch, SidechainOf,
    SidechainSources, SpawnAudioNode, TuttiGraphPlugin,
};
#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::{
    poll_wave_imports, promote_pending_samplers, reconcile_sampler_params, PendingSamplerLoad,
    WaveImportQueue,
};
#[cfg(feature = "convolution")]
pub use crate::units::ecs::{
    promote_pending_convolvers, start_convolver_loads, PendingConvolverLoad,
};
#[cfg(feature = "midi")]
pub use tutti_midi_io::ecs::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi};

#[cfg(feature = "automation")]
pub use crate::units::ecs::{
    automation_lane_system, reconcile_automation_writes, update_automation_envelope_system,
    AddAutomationLane, AutomationDrivesParam, AutomationLaneEmitter, AutomationLaneNode,
    AutomationParam, TuttiAutomationPlugin, UpdateAutomationEnvelope,
};

#[cfg(feature = "midi")]
pub use crate::midi_runtime::MidiBus;
#[cfg(feature = "midi-hardware")]
pub use tutti_midi_io::MidiIo;
#[cfg(feature = "midi")]
pub use tutti_midi_io::{MidiEvent, MidiInputRecord, SemanticEvent};

#[cfg(feature = "plugin")]
pub use tutti_plugin_host::{
    close_editor_observer, plugin_crash_detect_system, plugin_editor_attach_system,
    plugin_editor_idle_system, plugin_editor_open_system, plugin_editor_resize_request_system,
    plugin_editor_window_close_system, plugin_editor_window_resize_system, CloseEditor,
    OpenPluginEditor, PendingPluginEditor, PluginEditorOpen, PluginEmitter, TuttiHostingPlugin,
};
#[cfg(feature = "plugin")]
pub use tutti_plugin::catalog::{
    JsonCatalog, PluginCatalog, PluginRecord, PluginScanner, Plugins, PluginsConfig, ScanHandle,
    ScanPhase, ScanProgress, ScanResult,
};
#[cfg(feature = "plugin")]
pub use tutti_plugin::handles::PluginHandle;
#[cfg(feature = "plugin")]
pub use tutti_plugin::metadata::{ParameterFlags, ParameterInfo};

#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::{
    init_auditioner, AuditionerNode, AuditionerRes, PreviewFile, StopPreview, TuttiAuditionerPlugin,
};

#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::{
    ClipCommand, ClipSpec, SlotId, TrackClipReaderHandle, TrackClipReaderNode, TrackClipReaderRef,
    TrackClipReaderUnit,
};

#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::{
    recording_start_system, recording_stop_system, RecordingActive, RecordingResult,
    StartRecording, StopRecording, TuttiRecordingPlugin,
};
#[cfg(feature = "sampler")]
pub use crate::sampler::capture::{
    Mode as RecordingMode, Recorded as RecordedData, Source as RecordingSource,
};

#[cfg(feature = "analysis")]
pub use tutti_analysis::ecs::{
    live_analysis_control_system, live_analysis_sync_system, AnalysisRes, DisableLiveAnalysis,
    EnableLiveAnalysis, LiveAnalysisData, TuttiAnalysisPlugin,
};

#[cfg(feature = "export")]
pub use crate::export::{
    export_poll_system, export_start_system, ExportComplete, ExportFailed, ExportInProgress,
    StartExport, TuttiExportPlugin,
};
#[cfg(all(feature = "export", feature = "sampler"))]
pub use crate::render_region::{
    prepare_region_render_system, region_render_poll_system, spawn_region_render_system,
    RegionRenderComplete, RegionRenderConfig, RegionRenderFailed, RegionRenderInProgress,
    RegionRenderNet, RegionRenderSystems, StartRegionRender, TuttiRegionRenderPlugin,
};
#[cfg(feature = "export")]
pub use tutti_export::{
    AudioFormat, Handle as ExportHandle, Normalize, State as ExportState, Written,
};

#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::{
    audio_input_control_system, audio_input_sync_system, AudioInputDeviceInfo, AudioInputState,
    DisableAudioInput, EnableAudioInput, TuttiAudioInputPlugin,
};

#[cfg(feature = "sampler")]
pub use crate::sampler::stretch::Unit as TimeStretchUnit;
#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::{
    time_stretch_sync_system, TimeStretch, TimeStretchControl, TuttiTimeStretchPlugin,
};

// DSP spawn pipeline + plugin now live in `tutti_units::ecs`; surface them
// under the same `dsp` gate via the crate root (`bevy_tutti::*`).
#[allow(deprecated)]
pub use crate::units::ecs::AddLfo;
#[cfg(feature = "dsp")]
pub use crate::units::ecs::{
    dsp_chorus_system, dsp_compressor_system, dsp_delay_system, dsp_filter_system, dsp_gate_system,
    dsp_reverb_system,
};
// Generic marker-driven spawn (replaces the six `spawn_*_nodes` systems).
#[cfg(feature = "dsp")]
pub use crate::units::ecs::{spawn_dsp_node, AddDspNode, DspNode, SpawnParams};
pub use crate::units::ecs::{dsp_lfo_system, spawn_lfo_nodes, TuttiDspPlugin};
#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub use crate::units::ecs::{AddChorus, AddCompressor, AddDelay, AddFilter, AddGate, AddReverb};
#[cfg(feature = "dsp")]
pub use crate::units::{
    BrickwallLimiter, ChorusNode, Compressor, FlangerNode, Gate, LadderFilterNode, LadderType,
    LimiterNode, PhaserNode, StereoDelayLineNode, StereoLadderFilterNode, StereoPhaserNode,
    StereoSvfFilterNode, SvfType,
};
pub use crate::units::{LfoMode, LfoNode, LfoShape};

// Resource newtypes (defined in `crate::resources`).
// `AnalysisRes` is re-exported above from `tutti_analysis::ecs`.
#[cfg(feature = "midi")]
pub use tutti_midi_io::ecs::MidiBusRes;
#[cfg(feature = "midi-hardware")]
pub use tutti_midi_io::ecs::MidiIoRes;
#[cfg(feature = "plugin")]
pub use tutti_plugin_host::PluginEditorMainThread;
#[cfg(feature = "plugin")]
pub use tutti_plugin_host::PluginsRes;
#[cfg(feature = "sampler")]
pub use crate::sampler::ecs::{SamplerRes, TuttiSamplerPlugin};
#[cfg(feature = "soundfont")]
pub use crate::resources::SoundFontRes;
pub use crate::resources::{AudioConfig, MeteringRes, TransportRes, TuttiDriverRes, TuttiGraphRes};
