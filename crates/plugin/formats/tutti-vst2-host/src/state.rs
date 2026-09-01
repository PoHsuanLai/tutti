//! Plugin state save / restore.
//!
//! VST2's state model has two shapes, and both are wrapped in a four-byte header
//! so a restore can tell them apart:
//!
//! - `b"CHK\0"` — the plugin opted into "preset_chunks" and supplied its own
//!   opaque binary blob. Round-trips via `get_preset_data` / `load_preset_data`.
//! - `b"PRM\0"` — fallback: each normalized `f32` parameter value, serialized in
//!   index order and replayed on restore. Loses any non-parameter state the
//!   plugin holds, but works for a plugin with no chunk mechanism.

use vst::plugin::Plugin as _;

use crate::error::{Result, Vst2Error};
use crate::instance::Vst2Instance;

/// State format header bytes:
/// - `b"CHK\0"` = chunk-based state (plugin's own binary format)
/// - `b"PRM\0"` = parameter-based state (host-serialized f32 values)
const STATE_HEADER_CHUNK: [u8; 4] = *b"CHK\0";
const STATE_HEADER_PARAMS: [u8; 4] = *b"PRM\0";

/// The parsed shape of a state blob's framing, produced by
/// [`parse_state_header`] before any plugin interaction. Splitting this out
/// keeps the pure framing/validation logic testable without a live plugin.
#[derive(Debug, PartialEq)]
pub(crate) enum StateHeader<'a> {
    /// `CHK\0` chunk payload — hand straight to `load_preset_data`.
    Chunk(&'a [u8]),
    /// `PRM\0` parameter snapshot: `count` normalized f32 values follow,
    /// carried in `values` (already length-validated against `count`).
    Params { count: i32, values: &'a [u8] },
}

/// Validate the framing of a state blob and split off its payload.
///
/// Pure: performs only the header/length/count checks that don't need a live
/// plugin (the plugin-parameter-count cross-check stays in `set_state`).
/// Errors mirror the `set_state` early-returns exactly.
pub(crate) fn parse_state_header(data: &[u8]) -> Result<StateHeader<'_>> {
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
        Ok(StateHeader::Chunk(payload))
    } else if header == STATE_HEADER_PARAMS {
        if payload.len() < 4 {
            return Err(Vst2Error::StateRestoreError(
                "Invalid parameter state (missing count)".into(),
            ));
        }

        let count = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        if count < 0 {
            return Err(Vst2Error::StateRestoreError(format!(
                "Invalid parameter count: {}",
                count
            )));
        }

        let expected_payload = 4 + (count as usize) * 4;
        if payload.len() != expected_payload {
            return Err(Vst2Error::StateRestoreError(format!(
                "Parameter state size mismatch: expected {} bytes, got {}",
                expected_payload,
                payload.len()
            )));
        }

        // Strip the count prefix; leave just the f32 value bytes.
        Ok(StateHeader::Params {
            count,
            values: &payload[4..],
        })
    } else {
        Err(Vst2Error::StateRestoreError(format!(
            "Unknown state header: {:?}",
            header
        )))
    }
}

impl Vst2Instance {
    /// Capture the current plugin state as a portable byte blob.
    ///
    /// Prefers the plugin's own chunk format if it advertises one;
    /// otherwise falls back to a parameter snapshot.
    pub fn get_state(&self) -> Result<Vec<u8>> {
        let info = self.handle.instance.get_info();

        if info.preset_chunks {
            // `try_get_preset_data`, not `get_preset_data`: the infallible one
            // folds "nothing saved" and "the save failed" into the same empty
            // `Vec`, so a failed `getChunk` silently downgraded to a parameter
            // snapshot, losing the non-parameter state chunks exist to carry.
            // A failure is now an error; an *empty* chunk still falls through
            // to the parameter snapshot, keeping a fresh plugin saveable.
            let chunk = self.params.try_get_preset_data().map_err(|e| {
                Vst2Error::StateRestoreError(format!(
                    "plugin advertises effFlagsProgramChunks but its preset \
                     chunk save failed: {e}"
                ))
            })?;
            if !chunk.is_empty() {
                let mut state = Vec::with_capacity(4 + chunk.len());
                state.extend_from_slice(&STATE_HEADER_CHUNK);
                state.extend_from_slice(&chunk);
                return Ok(state);
            }
        }

        // Fallback: serialize all parameters.
        //
        // `.max(0)`: `numParams` is raw off the `AEffect` and a plugin can put
        // anything there. Unclamped, `-1` sign-extends to `usize::MAX` and the
        // `* 4` overflows — profile-dependent, so debug panics on multiply
        // overflow while release wraps to a small capacity and carries on.
        let param_count = info.parameters.max(0);
        let mut state = Vec::with_capacity(4 + 4 + (param_count as usize) * 4);
        state.extend_from_slice(&STATE_HEADER_PARAMS);
        state.extend_from_slice(&param_count.to_le_bytes());

        for i in 0..param_count {
            // A plugin that advertises parameters but exposes no accessor
            // cannot be serialized: writing 0.0 would produce a blob that
            // restores silently and wrongly, which is worse than refusing.
            let Some(value) = self.params.get_parameter(i) else {
                return Err(Vst2Error::StateRestoreError(format!(
                    "plugin reports {param_count} parameters but exposes no \
                     getParameter, so parameter {i} cannot be saved"
                )));
            };
            state.extend_from_slice(&value.to_le_bytes());
        }

        Ok(state)
    }

    /// Restore a state blob previously produced by [`get_state`](Self::get_state).
    pub fn set_state(&self, data: &[u8]) -> Result<()> {
        match parse_state_header(data)? {
            StateHeader::Chunk(payload) => {
                // `effSetChunk` reports whether the plugin took the blob. This
                // arm used to discard that answer and always return `Ok(())`,
                // so a plugin refusing a chunk (truncated, foreign, or a format
                // version it no longer reads) reported a successful restore
                // while sitting at its defaults.
                if !self.params.load_preset_data(payload) {
                    return Err(Vst2Error::StateRestoreError(format!(
                        "plugin rejected the {} byte preset chunk (effSetChunk \
                         did not report success)",
                        payload.len()
                    )));
                }
                Ok(())
            }
            StateHeader::Params { count, values } => {
                // `.max(0)` as in `get_state`: `numParams` is raw from the
                // `AEffect`. Unclamped, a plugin declaring `-1` makes this
                // comparison `0 > -1` and rejects the empty snapshot
                // `get_state` just wrote for that same plugin.
                let actual_count = self.handle.instance.get_info().parameters.max(0);
                if count > actual_count {
                    return Err(Vst2Error::StateRestoreError(format!(
                        "State has {} parameters but plugin only has {}",
                        count, actual_count
                    )));
                }

                for i in 0..count {
                    let offset = i as usize * 4;
                    let value = f32::from_le_bytes([
                        values[offset],
                        values[offset + 1],
                        values[offset + 2],
                        values[offset + 3],
                    ]);
                    let value = value.clamp(0.0, 1.0);
                    if !self.params.set_parameter(i, value) {
                        // Reporting success here would claim a preset was
                        // restored while every value went nowhere.
                        return Err(Vst2Error::StateRestoreError(format!(
                            "plugin exposes no setParameter, so parameter {i} \
                             could not be restored"
                        )));
                    }
                }

                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_too_short() {
        let err = parse_state_header(&[0x43, 0x48]).unwrap_err();
        assert!(matches!(err, Vst2Error::StateRestoreError(_)));
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn unknown_header() {
        let err = parse_state_header(b"XXX\0payload").unwrap_err();
        assert!(err.to_string().contains("Unknown state header"));
    }

    #[test]
    fn chunk_empty_payload_rejected() {
        let err = parse_state_header(&STATE_HEADER_CHUNK).unwrap_err();
        assert!(err.to_string().contains("Empty chunk data"));
    }

    #[test]
    fn chunk_valid() {
        let mut data = STATE_HEADER_CHUNK.to_vec();
        data.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            parse_state_header(&data).unwrap(),
            StateHeader::Chunk(&[1, 2, 3, 4])
        );
    }

    #[test]
    fn params_missing_count() {
        // Header present but fewer than 4 count bytes follow.
        let mut data = STATE_HEADER_PARAMS.to_vec();
        data.extend_from_slice(&[0, 0]);
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("missing count"));
    }

    #[test]
    fn params_negative_count() {
        let mut data = STATE_HEADER_PARAMS.to_vec();
        data.extend_from_slice(&(-1i32).to_le_bytes());
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("Invalid parameter count"));
    }

    #[test]
    fn params_size_mismatch() {
        // count says 2 params (needs 4 + 2*4 = 12 bytes payload) but only 1
        // value's worth of bytes follow.
        let mut data = STATE_HEADER_PARAMS.to_vec();
        data.extend_from_slice(&2i32.to_le_bytes());
        data.extend_from_slice(&1.0f32.to_le_bytes());
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("size mismatch"));
    }

    #[test]
    fn params_valid_roundtrip() {
        let mut data = STATE_HEADER_PARAMS.to_vec();
        data.extend_from_slice(&2i32.to_le_bytes());
        data.extend_from_slice(&0.25f32.to_le_bytes());
        data.extend_from_slice(&0.75f32.to_le_bytes());

        match parse_state_header(&data).unwrap() {
            StateHeader::Params { count, values } => {
                assert_eq!(count, 2);
                assert_eq!(values.len(), 8);
                assert_eq!(f32::from_le_bytes(values[0..4].try_into().unwrap()), 0.25);
                assert_eq!(f32::from_le_bytes(values[4..8].try_into().unwrap()), 0.75);
            }
            other => panic!("expected Params, got {other:?}"),
        }
    }
}
