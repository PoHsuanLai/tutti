//! Plugin state save / restore.
//!
//! VST2's state model has two shapes and we wrap both in a tiny header
//! so the host can tell them apart on restore:
//!
//! - `b"CHK\0"` — the plugin opted into "preset_chunks" and handed us
//!   its own opaque binary blob. Round-trips via `get_preset_data` /
//!   `load_preset_data`.
//! - `b"PRM\0"` — fallback. We serialize each normalized f32 parameter
//!   value and replay them on restore. Loses any non-parameter state
//!   the plugin holds, but works for plugins without a chunk mechanism.

use vst::plugin::Plugin as _;

use crate::error::{Result, Vst2Error};
use crate::instance::Vst2Instance;

/// State format header bytes:
/// - `b"CHK\0"` = chunk-based state (plugin's own binary format)
/// - `b"PRM\0"` = parameter-based state (host-serialized f32 values)
const STATE_HEADER_CHUNK: [u8; 4] = *b"CHK\0";
const STATE_HEADER_PARAMS: [u8; 4] = *b"PRM\0";

impl Vst2Instance {
    /// Capture the current plugin state as a portable byte blob.
    ///
    /// Prefers the plugin's own chunk format if it advertises one;
    /// otherwise falls back to a parameter snapshot.
    pub fn save_state(&self) -> Result<Vec<u8>> {
        let info = self.handle.instance.get_info();

        if info.preset_chunks {
            let chunk = self.params.get_preset_data();
            if !chunk.is_empty() {
                let mut state = Vec::with_capacity(4 + chunk.len());
                state.extend_from_slice(&STATE_HEADER_CHUNK);
                state.extend_from_slice(&chunk);
                return Ok(state);
            }
        }

        // Fallback: serialize all parameters.
        let param_count = info.parameters;
        let mut state = Vec::with_capacity(4 + 4 + (param_count as usize) * 4);
        state.extend_from_slice(&STATE_HEADER_PARAMS);
        state.extend_from_slice(&param_count.to_le_bytes());

        for i in 0..param_count {
            let value = self.params.get_parameter(i);
            state.extend_from_slice(&value.to_le_bytes());
        }

        Ok(state)
    }

    /// Restore a state blob previously produced by [`save_state`](Self::save_state).
    pub fn load_state(&self, data: &[u8]) -> Result<()> {
        if data.len() < 4 {
            return Err(Vst2Error::StateRestoreError(
                "State data too short (missing header)".into(),
            ));
        }

        let header: [u8; 4] = [data[0], data[1], data[2], data[3]];
        let payload = &data[4..];

        if header == STATE_HEADER_CHUNK {
            if payload.is_empty() {
                return Err(Vst2Error::StateRestoreError("Empty chunk data".into()));
            }
            self.params.load_preset_data(payload);
            Ok(())
        } else if header == STATE_HEADER_PARAMS {
            if payload.len() < 4 {
                return Err(Vst2Error::StateRestoreError(
                    "Invalid parameter state (missing count)".into(),
                ));
            }

            let param_count = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            if param_count < 0 {
                return Err(Vst2Error::StateRestoreError(format!(
                    "Invalid parameter count: {}",
                    param_count
                )));
            }

            let expected_payload = 4 + (param_count as usize) * 4;
            if payload.len() != expected_payload {
                return Err(Vst2Error::StateRestoreError(format!(
                    "Parameter state size mismatch: expected {} bytes, got {}",
                    expected_payload,
                    payload.len()
                )));
            }

            let actual_count = self.handle.instance.get_info().parameters;
            if param_count > actual_count {
                return Err(Vst2Error::StateRestoreError(format!(
                    "State has {} parameters but plugin only has {}",
                    param_count, actual_count
                )));
            }

            for i in 0..param_count {
                let offset = 4 + (i as usize * 4);
                let value = f32::from_le_bytes([
                    payload[offset],
                    payload[offset + 1],
                    payload[offset + 2],
                    payload[offset + 3],
                ]);
                let value = value.clamp(0.0, 1.0);
                self.params.set_parameter(i, value);
            }

            Ok(())
        } else {
            Err(Vst2Error::StateRestoreError(format!(
                "Unknown state header: {:?}",
                header
            )))
        }
    }
}
