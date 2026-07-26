//! Pure message dispatch — no I/O, no async, no sockets.
//!
//! [`Session`] owns the plugin + shared memory + audio pipeline + editor
//! state. [`Session::handle`] translates one [`HostMessage`] into state
//! changes and returns a [`Reaction`] describing what the transport
//! loop should do next. Because it has no `async` and no `Transport`
//! dependency, it's unit-testable without tokio or sockets.

use crate::audio_pipeline::{AudioBlock, AudioPipeline, Clock, ProcessExtras};
use crate::editor::EditorState;
use crate::plugin::{AsyncEvent, Plugin};
use tutti_plugin::server::{
    AudioSlab, BridgeMessage, HostMessage, IpcMidiEvent, IpcMidiEventVec, MidiEventVec,
    SampleFormat, WindowHandle, MIDI_STACK_CAPACITY,
};
use tutti_plugin::Result;

/// Serialize a block's plugin MIDI-out for the wire, capped so the SmallVec
/// stays inline (no heap on the RT-adjacent subprocess audio callback). A cap
/// hit means a pathological >256-events/block plugin; log once and truncate.
fn encode_midi_out(events: &MidiEventVec) -> IpcMidiEventVec {
    if events.len() > MIDI_STACK_CAPACITY {
        tracing::warn!(
            "plugin emitted {} MIDI events this block; capping at {}",
            events.len(),
            MIDI_STACK_CAPACITY
        );
    }
    events
        .iter()
        .take(MIDI_STACK_CAPACITY)
        .map(IpcMidiEvent::from)
        .collect()
}

/// What [`Session::handle`] asks the transport loop to do.
// `Reply` wraps `BridgeMessage`, whose `AudioProcessed` variant carries an
// inline-256 MIDI-out vec (see `protocol::envelope`). Off-RT server-side; the
// size gap is the deliberate inline-capacity tradeoff, not worth boxing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum Reaction {
    /// Send this reply back to the host.
    Reply(BridgeMessage),
    /// No reply; keep looping.
    None,
    /// Host asked us to shut down.
    Shutdown,
}

impl From<BridgeMessage> for Reaction {
    fn from(m: BridgeMessage) -> Self {
        Self::Reply(m)
    }
}

#[cfg(test)]
impl Reaction {
    fn into_reply(self) -> BridgeMessage {
        match self {
            Reaction::Reply(m) => m,
            other => panic!("expected Reply(_), got {other:?}"),
        }
    }
}

pub(crate) struct Session {
    pub(crate) plugin: Option<Plugin>,
    pub(crate) shm: Option<AudioSlab>,
    pub(crate) pipeline: AudioPipeline,
    pub(crate) editor: EditorState,
    pub(crate) clock: Clock,
}

impl Session {
    pub(crate) fn new() -> Self {
        let clock = Clock::default();
        Self {
            plugin: None,
            shm: None,
            pipeline: AudioPipeline::new(clock.format),
            editor: EditorState::Closed,
            clock,
        }
    }

    /// Translate `msg` into state changes + a [`Reaction`].
    pub(crate) fn handle(&mut self, msg: HostMessage) -> Result<Reaction> {
        use HostMessage as M;
        match msg {
            M::ProbePlugin { path } => Ok(self.handle_probe(&path).into()),
            M::LoadPlugin {
                path,
                sample_rate,
                block_size,
                preferred_format,
                shm_name: _,
            } => self.handle_load(path, sample_rate, block_size, preferred_format),
            M::UnloadPlugin => {
                self.plugin = None;
                self.shm = None;
                Ok(Reaction::None)
            }

            M::ProcessAudio(data) => {
                let midi: MidiEventVec = data.midi_events.iter().map(|&e| e.into()).collect();
                let extras = ProcessExtras {
                    param_changes: &data.param_changes,
                    note_expression: &data.note_expression,
                    chords: &data.chords,
                    scales: &data.scales,
                    expr_texts: &data.expr_texts,
                    expr_ints: &data.expr_ints,
                    transport: &data.transport,
                };
                self.handle_process(AudioBlock {
                    buffer_id: data.buffer_id,
                    num_samples: data.num_samples,
                    midi: &midi,
                    extras: Some(extras),
                })
            }

            M::SetParameter { param_id, value } => {
                if let Some(plugin) = self.plugin.as_mut() {
                    plugin.instance_mut().set_parameter(param_id, value as f64);
                }
                Ok(Reaction::None)
            }
            M::SetAutomationState { mode } => {
                if let Some(plugin) = self.plugin.as_mut() {
                    plugin.instance_mut().set_automation_state(mode);
                }
                Ok(Reaction::None)
            }
            M::GetParameter { param_id } => {
                let value = self
                    .plugin
                    .as_mut()
                    .map(|p| p.instance_mut().get_parameter(param_id) as f32);
                Ok(BridgeMessage::ParameterValue { value }.into())
            }
            M::GetParameterList => {
                let parameters = self
                    .plugin
                    .as_ref()
                    .map(|p| p.instance().get_parameter_list())
                    .unwrap_or_default();
                Ok(BridgeMessage::ParameterList { parameters }.into())
            }
            M::GetParameterInfo { param_id } => {
                let info = self.plugin.as_ref().and_then(|p| {
                    p.instance()
                        .get_parameter_list()
                        .into_iter()
                        .find(|info| info.id == param_id)
                });
                Ok(BridgeMessage::ParameterInfoResponse { info }.into())
            }

            M::OpenEditor { parent_handle } => Ok(self.handle_open_editor(parent_handle).into()),
            M::CloseEditor => {
                if let Some(plugin) = self.plugin.as_mut() {
                    plugin.instance_mut().close_editor();
                    self.editor = EditorState::Closed;
                }
                Ok(Reaction::None)
            }
            M::SetSampleRate { rate } => {
                self.clock.sample_rate = rate;
                if let Some(plugin) = self.plugin.as_mut() {
                    plugin.instance_mut().set_sample_rate(rate);
                }
                Ok(Reaction::None)
            }
            M::Reset => Ok(Reaction::None),

            M::SaveState => Ok(self.handle_save_state().into()),
            M::LoadState { data } => Ok(self.handle_load_state(&data)),

            M::SetupSharedMemory { shm_name, layout } => {
                self.shm = Some(AudioSlab::open(shm_name, layout)?);
                Ok(BridgeMessage::SharedMemoryReady.into())
            }
            M::Shutdown => Ok(Reaction::Shutdown),
        }
    }

    /// Drain async events from the loaded plugin, mapping each into a
    /// `BridgeMessage` the transport can push back. Called from the
    /// server's audio phase after every message dispatch.
    pub(crate) fn drain_async_events(&mut self) -> Vec<BridgeMessage> {
        let Some(plugin) = self.plugin.as_mut() else {
            return Vec::new();
        };
        plugin
            .poll_async_events()
            .into_iter()
            .map(|e| match e {
                AsyncEvent::ParameterChanged { index, value } => {
                    BridgeMessage::ParameterChanged { index, value }
                }
                AsyncEvent::LatencyChanged { samples } => BridgeMessage::LatencyChanged { samples },
                AsyncEvent::ParamValuesChanged => BridgeMessage::PluginParamValuesChanged,
                AsyncEvent::ParamTitlesChanged => BridgeMessage::PluginParamTitlesChanged,
                AsyncEvent::IoChanged => BridgeMessage::PluginIoChanged,
                AsyncEvent::Reloaded => BridgeMessage::PluginReloaded,
            })
            .collect()
    }

    /// Close the editor cleanly (if open). Called by `PluginServer`'s
    /// `Drop` so a subprocess teardown doesn't leak a floating editor
    /// window.
    pub(crate) fn close_editor_on_drop(&mut self) {
        if self.editor.is_open() {
            if let Some(plugin) = self.plugin.as_mut() {
                plugin.instance_mut().close_editor();
            }
            self.editor = EditorState::Closed;
        }
    }

    fn handle_probe(&self, path: &std::path::Path) -> BridgeMessage {
        match Plugin::probe(path) {
            Ok(descriptor) => BridgeMessage::PluginLoaded {
                descriptor: Box::new(descriptor),
                // A probe never activates the plugin, so there is no load data.
                loaded: Default::default(),
                negotiated_format: self.clock.format,
            },
            Err(e) => BridgeMessage::Error {
                message: format!("Probe failed: {e}"),
            },
        }
    }

    fn handle_load(
        &mut self,
        path: std::path::PathBuf,
        sample_rate: f64,
        block_size: usize,
        preferred_format: SampleFormat,
    ) -> Result<Reaction> {
        let (plugin, descriptor, loaded, negotiated) =
            Plugin::load(&path, sample_rate, block_size, preferred_format)?;
        self.clock = Clock {
            sample_rate,
            format: negotiated,
        };
        self.pipeline.set_format(negotiated);
        self.plugin = Some(plugin);
        Ok(BridgeMessage::PluginLoaded {
            descriptor: Box::new(descriptor),
            loaded,
            negotiated_format: negotiated,
        }
        .into())
    }

    fn handle_process(&mut self, block: AudioBlock<'_>) -> Result<Reaction> {
        // Read before `block` is moved into the pipeline: every AudioProcessed
        // reply below must echo it, including the degenerate no-shm one, or the
        // host's match-or-silence wait would time out on a reply it can't
        // attribute.
        let buffer_id = block.buffer_id;
        let Some(plugin) = self.plugin.as_mut() else {
            return Ok(BridgeMessage::Error {
                message: "No plugin loaded".to_string(),
            }
            .into());
        };
        let Some(shm) = self.shm.as_mut() else {
            // Matches prior behavior: process path only ran under the full
            // `plugin + shared_buffer` pair. No shm ⇒ produce an empty
            // AudioProcessed reply so the host stays in sync.
            return Ok(BridgeMessage::AudioProcessed {
                latency_us: 0,
                buffer_id,
                midi_out: IpcMidiEventVec::new(),
            }
            .into());
        };

        let output = self
            .pipeline
            .process(plugin.instance_mut(), shm, &self.clock, block)?;
        Ok(BridgeMessage::AudioProcessed {
            latency_us: output.latency_us,
            buffer_id,
            midi_out: encode_midi_out(&output.midi_out),
        }
        .into())
    }

    fn handle_open_editor(&mut self, parent_handle: u64) -> BridgeMessage {
        let Some(plugin) = self.plugin.as_mut() else {
            return BridgeMessage::Error {
                message: "No plugin loaded".to_string(),
            };
        };
        // SAFETY: parent_handle is a raw platform window pointer carried
        // as u64 over IPC; the single unsafe boundary for editor embedding.
        let handle = unsafe { WindowHandle::from_u64(parent_handle) };
        match plugin.instance_mut().open_editor(handle) {
            Ok(size) => {
                self.editor = EditorState::Open;
                BridgeMessage::EditorOpened {
                    width: size.width,
                    height: size.height,
                }
            }
            Err(e) => BridgeMessage::Error {
                message: format!("Failed to open editor: {e}"),
            },
        }
    }

    fn handle_save_state(&mut self) -> BridgeMessage {
        match self.plugin.as_mut() {
            Some(p) => match p.instance_mut().get_state() {
                Ok(data) => BridgeMessage::StateData { data },
                Err(e) => BridgeMessage::Error {
                    message: format!("Failed to save state: {e}"),
                },
            },
            None => BridgeMessage::StateData { data: vec![] },
        }
    }

    fn handle_load_state(&mut self, data: &[u8]) -> Reaction {
        let Some(plugin) = self.plugin.as_mut() else {
            return Reaction::None;
        };
        match plugin.instance_mut().set_state(data) {
            Ok(()) => Reaction::None,
            Err(e) => BridgeMessage::Error {
                message: format!("Failed to load state: {e}"),
            }
            .into(),
        }
    }
}

// Tests — pure dispatch, no tokio, no sockets.

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin::server::{
        Features, IpcMidiEventVec, NoteExpressionChanges, ParameterChanges, TransportInfo,
    };
    use tutti_plugin::{BridgeError, LoadStage};

    fn assert_none(r: Reaction) {
        assert!(matches!(r, Reaction::None), "got {r:?}");
    }

    fn assert_error_contains(r: Reaction, needle: &str) {
        let reply = r.into_reply();
        match reply {
            BridgeMessage::Error { ref message } if message.contains(needle) => {}
            _ => panic!("expected Error containing {needle:?}, got {reply:?}"),
        }
    }

    #[test]
    fn new_session_defaults() {
        let s = Session::new();
        assert_eq!(s.clock.sample_rate, 44100.0);
        assert_eq!(s.clock.format, SampleFormat::Float32);
        assert!(s.plugin.is_none());
        assert!(s.shm.is_none());
        assert!(!s.editor.is_open());
    }

    #[test]
    fn get_parameter_no_plugin() {
        let mut s = Session::new();
        let reply = s
            .handle(HostMessage::GetParameter { param_id: 0 })
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterValue { value } => assert!(value.is_none()),
            other => panic!("expected ParameterValue, got {other:?}"),
        }
    }

    #[test]
    fn get_parameter_list_no_plugin() {
        let mut s = Session::new();
        let reply = s
            .handle(HostMessage::GetParameterList)
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterList { parameters } => assert!(parameters.is_empty()),
            other => panic!("expected ParameterList, got {other:?}"),
        }
    }

    #[test]
    fn get_parameter_info_no_plugin() {
        let mut s = Session::new();
        let reply = s
            .handle(HostMessage::GetParameterInfo { param_id: 0 })
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterInfoResponse { info } => assert!(info.is_none()),
            other => panic!("expected ParameterInfoResponse, got {other:?}"),
        }
    }

    #[test]
    fn process_audio_no_plugin() {
        let mut s = Session::new();
        let r = s
            .handle(HostMessage::ProcessAudio(Box::new(
                tutti_plugin::server::ProcessAudioData {
                    buffer_id: 0,
                    num_samples: 256,
                    midi_events: IpcMidiEventVec::new(),
                    param_changes: ParameterChanges::new(),
                    note_expression: NoteExpressionChanges::new(),
                    transport: TransportInfo::default(),
                    ..Default::default()
                },
            )))
            .unwrap();
        assert_error_contains(r, "No plugin loaded");
    }

    #[test]
    fn set_parameter_no_plugin() {
        let mut s = Session::new();
        let r = s
            .handle(HostMessage::SetParameter {
                param_id: 0,
                value: 0.5,
            })
            .unwrap();
        assert_none(r);
    }

    #[test]
    fn set_sample_rate_no_plugin_updates_clock() {
        let mut s = Session::new();
        let r = s
            .handle(HostMessage::SetSampleRate { rate: 96000.0 })
            .unwrap();
        assert_none(r);
        assert_eq!(s.clock.sample_rate, 96000.0);
    }

    #[test]
    fn save_state_no_plugin() {
        let mut s = Session::new();
        let reply = s.handle(HostMessage::SaveState).unwrap().into_reply();
        match reply {
            BridgeMessage::StateData { data } => assert!(data.is_empty()),
            other => panic!("expected StateData, got {other:?}"),
        }
    }

    #[test]
    fn load_state_no_plugin() {
        let mut s = Session::new();
        let r = s
            .handle(HostMessage::LoadState {
                data: vec![1, 2, 3],
            })
            .unwrap();
        assert_none(r);
    }

    #[test]
    fn open_editor_no_plugin() {
        let mut s = Session::new();
        let r = s
            .handle(HostMessage::OpenEditor { parent_handle: 0 })
            .unwrap();
        assert_error_contains(r, "No plugin loaded");
    }

    #[test]
    fn close_editor_no_plugin() {
        let mut s = Session::new();
        let r = s.handle(HostMessage::CloseEditor).unwrap();
        assert_none(r);
    }

    #[test]
    fn unload_plugin_clears_shm() {
        let mut s = Session::new();
        let r = s.handle(HostMessage::UnloadPlugin).unwrap();
        assert_none(r);
        assert!(s.plugin.is_none());
        assert!(s.shm.is_none());
    }

    #[test]
    fn reset_no_plugin() {
        let mut s = Session::new();
        assert_none(s.handle(HostMessage::Reset).unwrap());
    }

    #[test]
    fn shutdown_signals_shutdown() {
        let mut s = Session::new();
        let r = s.handle(HostMessage::Shutdown).unwrap();
        assert!(matches!(r, Reaction::Shutdown));
    }

    #[test]
    fn load_plugin_missing_path_errors() {
        let mut s = Session::new();
        let err = s
            .handle(HostMessage::LoadPlugin {
                path: std::path::PathBuf::from("/nonexistent/path/plugin.vst3"),
                sample_rate: 44100.0,
                block_size: 512,
                preferred_format: SampleFormat::Float32,
                shm_name: "x".into(),
            })
            .unwrap_err();
        match err {
            BridgeError::LoadFailed { stage, reason, .. } => {
                assert_eq!(stage, LoadStage::Scanning);
                assert!(reason.contains("not found"));
            }
            other => panic!("expected LoadFailed, got {other:?}"),
        }
    }

    // --------------------------------------------------------------------
    // CLAP integration tests — require TAL-NoiseMaker installed.
    // --------------------------------------------------------------------

    #[cfg(feature = "clap")]
    const CLAP_PLUGIN: &str = "/Library/Audio/Plug-Ins/CLAP/TAL-NoiseMaker.clap";

    /// Load the real CLAP plugin into a Session and attach shared memory.
    /// Returns (session, shm_guard). The guard keeps the creator-side mmap
    /// alive while the session holds its own mapping.
    #[cfg(feature = "clap")]
    fn load_clap(name: &str, preferred_format: SampleFormat) -> (Session, AudioSlab) {
        let mut s = Session::new();
        s.handle(HostMessage::LoadPlugin {
            path: std::path::PathBuf::from(CLAP_PLUGIN),
            sample_rate: 44100.0,
            block_size: 512,
            preferred_format,
            shm_name: name.to_string(),
        })
        .unwrap();

        let buffer_name = format!("tutti_vst_buffer_{}_{}", name, std::process::id());
        let layout = tutti_plugin::server::SlabLayout {
            channels: tutti_plugin::server::ChannelLayout::Stereo,
            samples_per_channel: 8192,
            format: preferred_format,
            inputs: smallvec::smallvec![],
            outputs: smallvec::smallvec![],
        };
        let shm_guard = AudioSlab::create(buffer_name.clone(), layout.clone()).unwrap();
        s.shm = Some(AudioSlab::open(buffer_name, layout).unwrap());
        (s, shm_guard)
    }

    #[test]
    #[cfg(feature = "clap")]
    fn load_clap_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (s, _shm) = load_clap("load_clap", SampleFormat::Float32);
        assert!(s.plugin.is_some());
    }

    #[test]
    #[cfg(feature = "clap")]
    fn load_clap_plugin_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (s, _shm) = load_clap("load_clap_f64", SampleFormat::Float64);
        let loaded = s.plugin.as_ref().unwrap().instance().loaded();
        if loaded.features.contains(Features::F64_AUDIO) {
            assert_eq!(s.clock.format, SampleFormat::Float64);
        } else {
            assert_eq!(s.clock.format, SampleFormat::Float32);
        }
    }

    /// Fetch any param_id from the loaded plugin's parameter list.
    #[cfg(feature = "clap")]
    fn first_param_id(s: &mut Session) -> u32 {
        let reply = s
            .handle(HostMessage::GetParameterList)
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterList { parameters } => {
                assert!(!parameters.is_empty(), "need at least one parameter");
                parameters[0].id
            }
            other => panic!("expected ParameterList, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn get_parameter_list_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("get_param_list_clap", SampleFormat::Float32);
        let reply = s
            .handle(HostMessage::GetParameterList)
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterList { parameters } => assert!(!parameters.is_empty()),
            other => panic!("expected ParameterList, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn get_parameter_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("get_param_clap", SampleFormat::Float32);
        let param_id = first_param_id(&mut s);
        let reply = s
            .handle(HostMessage::GetParameter { param_id })
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterValue { value } => assert!(value.is_some()),
            other => panic!("expected ParameterValue, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn get_parameter_info_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("get_param_info_clap", SampleFormat::Float32);
        let param_id = first_param_id(&mut s);
        let reply = s
            .handle(HostMessage::GetParameterInfo { param_id })
            .unwrap()
            .into_reply();
        match reply {
            BridgeMessage::ParameterInfoResponse { info } => {
                let info = info.expect("info should exist");
                assert_eq!(info.id, param_id);
            }
            other => panic!("expected ParameterInfoResponse, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn set_parameter_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("set_param_clap", SampleFormat::Float32);
        let param_id = first_param_id(&mut s);
        let r = s
            .handle(HostMessage::SetParameter {
                param_id,
                value: 0.5,
            })
            .unwrap();
        assert_none(r);
    }

    /// Pull a SaveState blob out of a session, asserting it's non-empty.
    #[cfg(feature = "clap")]
    fn save_state_bytes(s: &mut Session) -> Vec<u8> {
        match s.handle(HostMessage::SaveState).unwrap().into_reply() {
            BridgeMessage::StateData { data } => {
                assert!(!data.is_empty(), "state should be non-empty");
                data
            }
            other => panic!("expected StateData, got {other:?}"),
        }
    }

    #[test]
    #[cfg(feature = "clap")]
    fn save_load_state() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("save_load_state_clap", SampleFormat::Float32);
        let data = save_state_bytes(&mut s);
        let r = s.handle(HostMessage::LoadState { data }).unwrap();
        assert_none(r);
    }

    /// Byte-exact round-trip: the bytes a plugin emits must survive the
    /// host<->subprocess IPC boundary and a load+re-save unchanged. Proves
    /// the state path carries the opaque blob verbatim (no UTF-8 assumption,
    /// truncation, or re-encoding) AND that the plugin's own state is stable.
    #[test]
    #[cfg(feature = "clap")]
    fn save_load_state_round_trips_byte_exact() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("rt_state_clap", SampleFormat::Float32);

        let first = save_state_bytes(&mut s);
        assert_none(
            s.handle(HostMessage::LoadState {
                data: first.clone(),
            })
            .unwrap(),
        );
        let second = save_state_bytes(&mut s);

        assert_eq!(
            first,
            second,
            "state blob changed across save → load → save \
             (len {} vs {}) — the IPC path or plugin mutated it",
            first.len(),
            second.len()
        );
    }

    #[test]
    #[cfg(feature = "clap")]
    fn set_sample_rate_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("set_sr_clap", SampleFormat::Float32);
        let r = s
            .handle(HostMessage::SetSampleRate { rate: 96000.0 })
            .unwrap();
        assert_none(r);
        assert_eq!(s.clock.sample_rate, 96000.0);
    }

    #[test]
    #[cfg(feature = "clap")]
    fn unload_loaded_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("unload_clap", SampleFormat::Float32);
        assert!(s.plugin.is_some());
        assert!(s.shm.is_some());
        let r = s.handle(HostMessage::UnloadPlugin).unwrap();
        assert_none(r);
        assert!(s.plugin.is_none());
        assert!(s.shm.is_none());
    }

    #[test]
    #[cfg(feature = "clap")]
    fn process_audio_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("proc_audio_clap", SampleFormat::Float32);
        let reply = s
            .handle(HostMessage::ProcessAudio(Box::new(
                tutti_plugin::server::ProcessAudioData {
                    buffer_id: 0,
                    num_samples: 512,
                    midi_events: IpcMidiEventVec::new(),
                    param_changes: ParameterChanges::new(),
                    note_expression: NoteExpressionChanges::new(),
                    transport: TransportInfo::default(),
                    ..Default::default()
                },
            )))
            .unwrap()
            .into_reply();
        assert!(
            matches!(reply, BridgeMessage::AudioProcessed { .. }),
            "unexpected reply: {reply:?}"
        );
    }

    #[test]
    #[cfg(feature = "clap")]
    fn editor_check_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap("editor_check_clap", SampleFormat::Float32);
        let plugin = s.plugin.as_mut().expect("plugin loaded");
        let _has_editor: bool = plugin.instance().descriptor().has_editor;
    }
}
