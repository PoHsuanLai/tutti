//! Owns a wasmtime `Store` and the bindgen `AudioPlugin` bindings for
//! one loaded WASM audio plugin.
//!
//! The instance is *not* `Send`-friendly to share across threads
//! directly — it carries `&mut store` semantics in every guest call.
//! The caller is responsible for serializing access (the in-process
//! audio_unit/control_backend pair does this via `Arc<Mutex<…>>`).
//!
//! Inherent methods mirror what the audio thread and the control
//! thread need, with the audio path's `process_f32` taking borrowed
//! input slices and writing into borrowed output slices to keep the
//! per-call lift the only Component-Model allocation we can't avoid
//! at the v0.1 WIT contract.

use std::path::Path;

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::Store;
use wasmtime_wasi::p2::add_to_linker_sync;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use tutti_midi_types::ump::MidiEvent as UmpMidiEvent;

use tutti_plugin::server::MidiEventVec;
use tutti_plugin::{BridgeError, LoadStage, Result};
use tutti_plugin::server::{BusChannels, Features, LoadedPlugin, PluginClass, PluginDescriptor};
use tutti_plugin_types::{ParameterFlags, ParameterInfo};

use crate::runtime::{self, EPOCH_DEADLINE_TICKS};

// =============================================================================
// Bindings — generated from wit/audio-plugin.wit
// =============================================================================

wasmtime::component::bindgen!({
    world: "audio-plugin",
    path: "wit",
});

pub use exports::dawai::audio_plugin::plugin as guest_plugin;

// =============================================================================
// Per-instance store payload
// =============================================================================

/// Wasmtime store data. Holds the WASI context the guest needs for
/// stdio/random (other WASI surfaces are not bound to the linker).
pub(super) struct WasmHostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasmHostState {
    fn new() -> Self {
        // No filesystem, no network, no env. Stdio and clocks only —
        // this is the audio worklet scope.
        let wasi = WasiCtxBuilder::new().inherit_stdio().build();
        Self {
            wasi,
            table: ResourceTable::new(),
        }
    }
}

impl WasiView for WasmHostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// =============================================================================
// WasmInstance
// =============================================================================

pub(super) struct WasmInstance {
    store: Store<WasmHostState>,
    bindings: AudioPlugin,
    descriptor: PluginDescriptor,
    loaded: LoadedPlugin,
    parameters: Vec<ParameterInfo>,
    /// Reusable per-block scratch for the input planar tree. The outer
    /// Vec is sized once at first `process_f32` and reused; only the
    /// inner channel-slice contents change per block.
    inputs_planar: Vec<Vec<f32>>,
    /// Reusable scratch for the MIDI-in WIT conversion.
    midi_in: Vec<guest_plugin::MidiEvent>,
}

impl WasmInstance {
    /// Probe metadata without keeping the instance alive.
    #[allow(dead_code)]
    pub(super) fn probe(path: &Path) -> Result<PluginDescriptor> {
        let inst = Self::load(path, 44100.0, 256)?;
        Ok(inst.descriptor.clone())
    }

    pub(super) fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        let engine =
            runtime::engine().map_err(|e| load_failed(path, LoadStage::Opening, e))?;
        let component = Component::from_file(&engine, path).map_err(|e| {
            load_failed(
                path,
                LoadStage::Opening,
                format!("failed to read WASM component: {e}"),
            )
        })?;

        let mut linker = Linker::<WasmHostState>::new(&engine);
        add_to_linker_sync(&mut linker).map_err(|e| {
            load_failed(
                path,
                LoadStage::Initialization,
                format!("WASI linker init failed: {e}"),
            )
        })?;

        let mut store = Store::new(&engine, WasmHostState::new());
        // Epoch interruption is on at the engine level; without a per-
        // store deadline a guest would trap immediately. Set a deadline
        // that's effectively "forever" for non-audio calls (init / state
        // I/O); the audio path bumps it to EPOCH_DEADLINE_TICKS per
        // process call.
        store.set_epoch_deadline(u64::MAX);

        let bindings = AudioPlugin::instantiate(&mut store, &component, &linker).map_err(|e| {
            load_failed(
                path,
                LoadStage::Instantiation,
                format!("component instantiation failed: {e}"),
            )
        })?;

        let metadata_wit = bindings
            .dawai_audio_plugin_plugin()
            .call_init(&mut store, sample_rate, block_size as u32)
            .map_err(|e| {
                load_failed(
                    path,
                    LoadStage::Initialization,
                    format!("guest init trapped: {e}"),
                )
            })?
            .map_err(|reason| {
                load_failed(
                    path,
                    LoadStage::Initialization,
                    format!("guest init: {reason}"),
                )
            })?;

        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| format!("wasm.{s}"))
            .unwrap_or_else(|| "wasm.unknown".to_string());

        let descriptor = PluginDescriptor {
            id,
            name: metadata_wit.name,
            vendor: metadata_wit.vendor,
            version: metadata_wit.version,
            class: PluginClass::Wasm {
                receives_midi: metadata_wit.midi.receives,
            },
            has_editor: false,
        };
        // The WASM audio-plugin world (v0.1) is single-bus, f32-only, headless,
        // and consumes MIDI-1 input. It has no editor, no f64, no transport /
        // automation / note-expression / sequencer context, and its guest MIDI
        // output is discarded — so only MIDI_IN is advertised.
        let mut features = Features::empty();
        features.set(Features::MIDI_IN, metadata_wit.midi.receives);
        let loaded = LoadedPlugin {
            inputs: BusChannels::from_slice(&[metadata_wit.audio.inputs as usize]),
            outputs: BusChannels::from_slice(&[metadata_wit.audio.outputs as usize]),
            latency_samples: metadata_wit.latency_samples as usize,
            features,
        };

        let parameters = metadata_wit
            .parameters
            .into_iter()
            .map(wit_to_parameter_info)
            .collect();

        let _ = sample_rate; // reported to guest at init; the host carries it on the AudioUnit
        Ok(Self {
            store,
            bindings,
            descriptor,
            loaded,
            parameters,
            inputs_planar: Vec::new(),
            midi_in: Vec::new(),
        })
    }

    pub(super) fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    pub(super) fn loaded(&self) -> &LoadedPlugin {
        &self.loaded
    }

    pub(super) fn parameters(&self) -> &[ParameterInfo] {
        &self.parameters
    }

    /// Run one audio block.
    ///
    /// `inputs` and `outputs` are caller-owned planar buffers; `outputs`
    /// is overwritten with the guest's output (truncated or zero-padded
    /// per channel). Returns any MIDI events the guest emitted.
    ///
    /// Per-call allocations the caller should know about:
    /// - The guest's `list<list<f32>>` return is owned by wasmtime's
    ///   canonical-ABI lift; this is unavoidable until v0.2 WIT.
    /// - The MIDI-out `MidiEventVec` is allocated locally; for the
    ///   typical zero-MIDI block it stays on the stack via `SmallVec`.
    pub(super) fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        midi_events: &[UmpMidiEvent],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
    ) -> Result<MidiEventVec> {
        // Stage the input slices into our cached planar tree. The outer
        // Vec stops reallocating once it has n_in slots; the inner per-
        // channel buffers grow once to `num_samples` and stay there.
        let n_in = inputs.len();
        if self.inputs_planar.len() < n_in {
            self.inputs_planar.resize_with(n_in, Vec::new);
        }
        for (slot, src) in self.inputs_planar.iter_mut().zip(inputs.iter()) {
            slot.clear();
            slot.extend_from_slice(src);
        }
        let input_view = &self.inputs_planar[..n_in];

        // Convert UMP MIDI events to MIDI 1.0 bytes for the guest. UMP
        // events with no MIDI 1.0 representation (per-note CC, RPN/NRPN,
        // SysEx, utility) are dropped — the audio plugin v0.1 contract
        // is MIDI 1.0 only.
        self.midi_in.clear();
        for ev in midi_events {
            if let Some((bytes, len)) = ev.to_midi1_bytes() {
                self.midi_in.push(guest_plugin::MidiEvent {
                    time_frames: ev.frame_offset,
                    data: bytes[..len as usize].to_vec(),
                });
            }
        }

        // Bracket the guest call with an epoch deadline so a runaway
        // process traps within `EPOCH_DEADLINE_TICKS × watchdog period`
        // (~4 ms at the defaults). After the call returns, push the
        // deadline back out so post-block control calls (state, params)
        // don't trip on a stale deadline.
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let result = self
            .bindings
            .dawai_audio_plugin_plugin()
            .call_process(&mut self.store, input_view, &self.midi_in)
            .map_err(|e| BridgeError::ProcessError(format!("guest trap in process: {e}")))?
            .map_err(|reason| BridgeError::ProcessError(format!("guest process: {reason}")));
        self.store.set_epoch_deadline(u64::MAX);
        let (outputs_planar, process_output) = result?;

        // Copy the guest's owned outputs into the borrowed output
        // slices. Truncate or zero-pad per channel.
        for (ch_idx, out) in outputs.iter_mut().enumerate() {
            if let Some(src) = outputs_planar.get(ch_idx) {
                let copy_len = src.len().min(num_samples);
                out[..copy_len].copy_from_slice(&src[..copy_len]);
                for s in &mut out[copy_len..num_samples] {
                    *s = 0.0;
                }
            } else {
                for s in &mut out[..num_samples] {
                    *s = 0.0;
                }
            }
        }

        let mut midi_out = MidiEventVec::new();
        for ev in process_output.midi {
            if let Some(ump) = UmpMidiEvent::from_midi1_bytes(ev.time_frames, &ev.data) {
                midi_out.push(ump);
            }
        }

        Ok(midi_out)
    }

    pub(super) fn set_sample_rate(&mut self, rate: f64) {
        let _ = self
            .bindings
            .dawai_audio_plugin_plugin()
            .call_set_sample_rate(&mut self.store, rate);
    }

    /// Returns the parameter's last-known value. The WIT contract has
    /// `get-parameter` but the bindgen signature takes `&mut store`,
    /// which we'd need to expose on every read. For now this returns
    /// the cached default — matches the VST2 in-process backend's
    /// best-effort read.
    pub(super) fn get_parameter(&self, id: u32) -> f64 {
        self.parameters
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.default_value)
            .unwrap_or(0.0)
    }

    pub(super) fn set_parameter(&mut self, id: u32, value: f64) {
        let _ = self
            .bindings
            .dawai_audio_plugin_plugin()
            .call_set_parameter(&mut self.store, id, value);
    }

    pub(super) fn get_state(&mut self) -> Result<Vec<u8>> {
        self.bindings
            .dawai_audio_plugin_plugin()
            .call_get_state(&mut self.store)
            .map_err(|e| BridgeError::StateSaveError(format!("guest trap: {e}")))?
            .map_err(BridgeError::StateSaveError)
    }

    pub(super) fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.bindings
            .dawai_audio_plugin_plugin()
            .call_set_state(&mut self.store, data)
            .map_err(|e| BridgeError::StateRestoreError(format!("guest trap: {e}")))?
            .map_err(BridgeError::StateRestoreError)
    }

    /// Warm-prime trap handlers and JIT code paths by running one zero
    /// block on the main thread. After this returns, the audio thread's
    /// first `process_f32` won't pay first-call setup costs.
    pub(super) fn warm_prime(&mut self) {
        let n_in = self.loaded.total_inputs();
        let n_out = self.loaded.total_outputs();
        const N: usize = 64;
        let empty: Vec<f32> = vec![0.0; N];
        let input_refs: Vec<&[f32]> = (0..n_in).map(|_| empty.as_slice()).collect();
        let mut output_bufs: Vec<Vec<f32>> = (0..n_out).map(|_| vec![0.0; N]).collect();
        let mut output_refs: Vec<&mut [f32]> =
            output_bufs.iter_mut().map(|v| v.as_mut_slice()).collect();
        // Ignore errors — the goal is to touch the call path, not assert
        // correctness here. If the guest traps on its first call, that
        // surfaces on the next real `process_f32` anyway.
        let _ = self.process_f32(&input_refs, &[], &mut output_refs, N);
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn load_failed(path: &Path, stage: LoadStage, reason: impl Into<String>) -> BridgeError {
    BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage,
        reason: reason.into(),
    }
}

fn wit_to_parameter_info(p: guest_plugin::ParameterInfo) -> ParameterInfo {
    ParameterInfo {
        id: p.id,
        name: p.name,
        unit: p.unit,
        min_value: p.min_value,
        max_value: p.max_value,
        default_value: p.default_value,
        step_count: 0,
        flags: ParameterFlags {
            automatable: true,
            ..ParameterFlags::default()
        },
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn missing_wasm_file_errors() {
        let res = WasmInstance::load(Path::new("/nonexistent/plugin.wasm"), 44100.0, 256);
        assert!(matches!(res, Err(BridgeError::LoadFailed { .. })));
    }

    #[test]
    fn engine_initializes() {
        assert!(runtime::engine().is_ok());
    }

    /// Path to the reverb fixture in `examples/wasm-plugins/reverb/`.
    /// `None` if the fixture hasn't been built — tests skip rather than
    /// fail since the fixture lives outside the cargo workspace.
    fn reverb_fixture_path() -> Option<PathBuf> {
        // crates/tutti-plugin → tutti → workspace root → examples/...
        let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../../examples/wasm-plugins/reverb/target/wasm32-wasip2/release/wasm_reverb_plugin.wasm",
        );
        candidate.exists().then_some(candidate)
    }

    fn skip_if_no_reverb(name: &str) -> Option<PathBuf> {
        match reverb_fixture_path() {
            Some(p) => Some(p),
            None => {
                eprintln!(
                    "[{name}] skipping: reverb.wasm fixture not built. Run \
                     `examples/wasm-plugins/reverb/build.sh` to enable this test."
                );
                None
            }
        }
    }

    #[test]
    fn reverb_load_reports_metadata() {
        let Some(path) = skip_if_no_reverb("reverb_load_reports_metadata") else {
            return;
        };
        let inst = WasmInstance::load(&path, 44100.0, 256).expect("reverb should load");
        assert_eq!(inst.descriptor().name, "Simple Reverb");
        assert_eq!(inst.loaded().total_inputs(), 2);
        assert_eq!(inst.loaded().total_outputs(), 2);
        assert_eq!(inst.parameters().len(), 4);
    }

    #[test]
    fn reverb_processes_impulse() {
        let Some(path) = skip_if_no_reverb("reverb_processes_impulse") else {
            return;
        };
        let mut inst = WasmInstance::load(&path, 44100.0, 4096).expect("reverb should load");

        const N: usize = 4096;
        let mut left = vec![0.0_f32; N];
        let mut right = vec![0.0_f32; N];
        left[0] = 1.0;
        right[0] = 1.0;

        let mut out_l = vec![0.0_f32; N];
        let mut out_r = vec![0.0_f32; N];
        let inputs: [&[f32]; 2] = [&left, &right];
        let mut outputs: [&mut [f32]; 2] = [out_l.as_mut_slice(), out_r.as_mut_slice()];
        inst.process_f32(&inputs, &[], &mut outputs, N)
            .expect("process succeeds");

        assert!(
            out_l[0].abs() > 0.6 && out_l[0].abs() <= 1.0,
            "expected dry hit ~0.7 at sample 0, got {}",
            out_l[0]
        );

        let tail_energy: f32 = out_l[1700..]
            .iter()
            .map(|s| s * s)
            .sum::<f32>()
            .sqrt();
        assert!(
            tail_energy > 0.0,
            "expected reverb tail energy > 0 at sample 1700+, got {}",
            tail_energy
        );
    }

    #[test]
    fn reverb_parameter_roundtrip() {
        let Some(path) = skip_if_no_reverb("reverb_parameter_roundtrip") else {
            return;
        };
        let mut inst = WasmInstance::load(&path, 44100.0, 256).expect("reverb should load");
        inst.set_parameter(0, 0.9);
        let _ = inst.get_parameter(0);
    }

    #[test]
    fn reverb_state_roundtrip() {
        let Some(path) = skip_if_no_reverb("reverb_state_roundtrip") else {
            return;
        };
        let mut inst = WasmInstance::load(&path, 44100.0, 256).expect("reverb should load");
        inst.set_parameter(0, 0.9);
        inst.set_parameter(1, 0.1);
        let state = inst.get_state().expect("get_state succeeds");
        assert_eq!(state.len(), 16, "reverb preset is 4×f32 = 16 bytes");

        let mut inst2 = WasmInstance::load(&path, 44100.0, 256).expect("reverb should load");
        inst2.set_state(&state).expect("set_state succeeds");
        let restored = inst2.get_state().expect("get_state after restore");
        assert_eq!(restored, state, "state round-trips byte-identical");
    }

    fn synth_dsp_fixture_path() -> Option<PathBuf> {
        let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/wasm-plugins/synth-with-ui/dist/synth_with_ui_dsp.wasm");
        candidate.exists().then_some(candidate)
    }

    fn skip_if_no_synth_dsp(name: &str) -> Option<PathBuf> {
        match synth_dsp_fixture_path() {
            Some(p) => Some(p),
            None => {
                eprintln!(
                    "[{name}] skipping: synth_with_ui_dsp.wasm fixture not built. Run \
                     `examples/wasm-plugins/synth-with-ui/build.sh` to enable this test."
                );
                None
            }
        }
    }

    #[test]
    fn synth_dsp_load_reports_metadata() {
        let Some(path) = skip_if_no_synth_dsp("synth_dsp_load_reports_metadata") else {
            return;
        };
        let inst = WasmInstance::load(&path, 44100.0, 256).expect("synth dsp should load");
        assert_eq!(inst.descriptor().name, "Subtractive Synth");
        assert_eq!(inst.loaded().total_inputs(), 0);
        assert_eq!(inst.loaded().total_outputs(), 2);
        assert!(
            matches!(
                inst.descriptor().class,
                PluginClass::Wasm { receives_midi: true }
            ),
            "synth should declare MIDI receive"
        );
        assert_eq!(inst.parameters().len(), 3);
    }

    #[test]
    fn synth_dsp_silent_without_midi() {
        let Some(path) = skip_if_no_synth_dsp("synth_dsp_silent_without_midi") else {
            return;
        };
        let mut inst = WasmInstance::load(&path, 44100.0, 256).expect("synth dsp should load");

        const N: usize = 256;
        let mut out_l = vec![0.0_f32; N];
        let mut out_r = vec![0.0_f32; N];
        let inputs: [&[f32]; 0] = [];
        let mut outputs: [&mut [f32]; 2] = [out_l.as_mut_slice(), out_r.as_mut_slice()];
        inst.process_f32(&inputs, &[], &mut outputs, N)
            .expect("process succeeds");

        let energy: f32 = out_l.iter().map(|s| s * s).sum::<f32>().sqrt();
        assert!(
            energy < 1e-6,
            "synth without note-on should be silent, got energy {energy}"
        );
    }
}
