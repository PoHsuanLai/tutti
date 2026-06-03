//! Public surface re-exports.
//!
//! `lib.rs` does `pub use prelude::*` so all symbols continue to resolve
//! at `bevy_tutti::Foo`. Callers import from the crate root, not from
//! `bevy_tutti::prelude` directly — keep this module `pub(crate)`.

pub use crate::loader::{
    TuttiLoader, TuttiLoaderError, TuttiStreamingLoader, TuttiStreamingLoaderError,
};
pub use crate::core::WaveAsset;
// Decode-once wave cache (Bevy-native): the single shared `Arc<Wave>` source
// for playback, analysis, and the offline render.
pub use tutti_wavecache::{poll_wave_cache, WaveCache, WaveCachePlugin, WaveState};
#[cfg(feature = "soundfont")]
pub use crate::synth::SoundFontAsset;
#[cfg(feature = "sampler")]
pub use crate::sampler::StreamingSample;

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
pub use tutti_midi_io::{MpeMode, MpeZone, MpeZoneConfig};

#[cfg(feature = "soundfont")]
pub use crate::soundfont::{soundfont_playback_system, PlaySoundFont, TuttiSoundFontPlugin};

pub use crate::metering::{metering_sync_system, MasterMeterLevels};
pub use crate::transport::{transport_sync_system, TransportState};

pub use crate::device_state::{device_state_sync_system, AudioDeviceState};

#[cfg(feature = "sampler")]
pub use crate::content_bounds::ContentBounds;

// The engine types (`TuttiEngine`, `TuttiGraph`, `NodeId`, `Wave`, …) and the
// scalar entity-as-node ECS params (`AudioNode`, `Volume`, `Frequency`, …) are
// re-exported at the crate root by `lib.rs`, and `lib.rs` does `pub use
// prelude::*`, so they are already reachable via `bevy_tutti::*` and
// `bevy_tutti::prelude::*` without re-listing them here.

// B7 authoring markers + construction-only authored data (added in
// `tutti_core::ecs`). The preferred spawn surface: `commands.spawn((FilterNode,
// Frequency(..), FilterQ(..), ..))`. The `#[require]` lists fill in defaults;
// the marker spawn systems build the unit from those component values.
// Several marker names collide with the concrete `tutti_units` unit names
// already re-exported below (`ChorusNode`, `FlangerNode`, `PhaserNode`,
// `LimiterNode`). Those four markers are aliased with a `Marker` suffix to
// disambiguate; the rest keep their natural name.
pub use crate::core::ecs::{
    BeatSynced, BrickwallLimiterNode, ChorusNode as ChorusNodeMarker, CompressorNode,
    ConvolutionReverbNode, DelayNode, DistortionNode, EqBandNode, FilterMode, FilterNode,
    FlangerNode as FlangerNodeMarker, GateNode, LadderNode, LfoNodeMarker, LfoShapeKind,
    LimiterNode as LimiterNodeMarker, MaxDelay, PhaserNode as PhaserNodeMarker, ReverbNode,
    ReverbTime, SamplerNode, SpatialPannerNode, StereoChannels,
};

pub use crate::graph::{
    commit_graph, crossfade_audio_node, insert_node_marker, reconcile_audio_routing,
    reconcile_node_despawn, reconcile_params, reconcile_sidechain_links, register_audio_node_types,
    AudioFedBy, AudioFeedsTo, GraphDirty, GraphReconcileSystems, NodeParamEpoch, SidechainOf,
    SidechainSources, SpawnAudioNode, TuttiGraphPlugin,
};
#[cfg(feature = "sampler")]
pub use crate::graph::{
    poll_wave_imports, promote_pending_samplers, reconcile_sampler_params, PendingSamplerLoad,
    WaveImportQueue,
};
#[cfg(feature = "convolution")]
pub use crate::graph::{
    promote_pending_convolvers, start_convolver_loads, PendingConvolverLoad,
};
#[cfg(feature = "midi")]
pub use crate::graph::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi};
#[cfg(feature = "plugin")]
pub use crate::graph::reconcile_plugin_params;

#[cfg(feature = "automation")]
pub use crate::automation::{
    automation_lane_system, reconcile_automation_writes, update_automation_envelope_system,
    AddAutomationLane, AutomationDrivesParam, AutomationLaneEmitter, AutomationLaneNode,
    AutomationParam, TuttiAutomationPlugin, UpdateAutomationEnvelope,
};

#[cfg(feature = "midi")]
pub use tutti_midi_io::{MidiEvent, MidiInputRecord, SemanticEvent};
#[cfg(feature = "midi")]
pub use crate::midi_runtime::MidiBus;
#[cfg(feature = "midi-hardware")]
pub use tutti_midi_io::MidiIo;

#[cfg(feature = "plugin")]
pub use crate::plugin_host::{
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
pub use crate::auditioner::{
    init_auditioner, AuditionerNode, AuditionerRes, PreviewFile, StopPreview,
    TuttiAuditionerPlugin,
};

#[cfg(feature = "sampler")]
pub use crate::track_clip_reader::{
    ClipCommand, ClipSpec, SlotId, TrackClipReaderHandle, TrackClipReaderNode,
    TrackClipReaderRef, TrackClipReaderUnit,
};

#[cfg(feature = "sampler")]
pub use crate::recording::{
    recording_start_system, recording_stop_system, RecordingActive, TuttiRecordingPlugin,
    RecordingResult, StartRecording, StopRecording,
};
#[cfg(feature = "sampler")]
pub use crate::sampler::capture::{
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
#[cfg(all(feature = "export", feature = "sampler"))]
pub use crate::render_region::{
    prepare_region_render_system, region_render_poll_system, spawn_region_render_system,
    RegionRenderComplete, RegionRenderFailed, RegionRenderInProgress, RegionRenderNet,
    RegionRenderSystems, StartRegionRender, TuttiRegionRenderPlugin,
};
#[cfg(feature = "export")]
pub use tutti_export::{
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
pub use crate::sampler::stretch::Unit as TimeStretchUnit;

pub use crate::dsp::{dsp_lfo_system, spawn_lfo_nodes, TuttiDspPlugin};
#[allow(deprecated)]
pub use crate::dsp::AddLfo;
pub use crate::units::{LfoMode, LfoNode, LfoShape};
#[cfg(feature = "dsp")]
pub use crate::dsp::{
    dsp_chorus_system, dsp_compressor_system, dsp_delay_system, dsp_filter_system,
    dsp_gate_system, dsp_reverb_system, spawn_chorus_nodes, spawn_compressor_nodes,
    spawn_delay_nodes, spawn_filter_nodes, spawn_gate_nodes, spawn_reverb_nodes,
};
#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub use crate::dsp::{AddChorus, AddCompressor, AddDelay, AddFilter, AddGate, AddReverb};
#[cfg(feature = "dsp")]
pub use crate::units::{
    BrickwallLimiter, ChorusNode, Compressor, FlangerNode, Gate, LadderFilterNode, LadderType,
    LimiterNode, PhaserNode, StereoDelayLineNode, StereoLadderFilterNode, StereoPhaserNode,
    StereoSvfFilterNode, SvfType,
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
