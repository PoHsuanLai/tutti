//! Plugin state save / restore.
//!
//! VST2's state model has two shapes, and both are wrapped in a four-byte tag
//! so a restore can tell them apart. Both carry the **current program's index**
//! (`effGetProgram`), and a restore sets it (`effSetProgram`) before it hands
//! the plugin anything else:
//!
//! - `b"CHK\x01"` — the plugin set `effFlagsProgramChunks` and supplied its own
//!   opaque binary blob for the **current program** (`effGetChunk`, index 1).
//! - `b"PRM\x01"` — fallback: each normalized `f32` parameter value of the current
//!   program, serialized in index order and replayed on restore. Loses any
//!   non-parameter state the plugin holds, but works for a plugin with no chunk
//!   mechanism.
//! - `b"CHK\0"` and `b"PRM\0"` are the same shapes written before the program
//!   index was carried, still read, with no program set.
//!
//! **Program chunk, not bank chunk, and why.** `aeffect.h` (authority 3) says
//! `effFlagsProgramChunks` means "program data is handled in formatless chunks", and that
//! `effGetChunk` and `effSetChunk` take `[index]: 0 for bank, 1 for program`. It does not say
//! which one a host saves. A bank chunk covers all `numPrograms` programs; a program chunk the
//! current one. What this state restores is the plugin as it sounds now, one program, and the
//! other programs are the plugin's presets, which tutti lists and selects by index and does
//! not own; so the state is the current program's chunk plus the index to put it back into.
//! A program chunk restored into a fresh instance lands in whichever program is current there
//! (program 0), which is the loss this index prevents. The header does not say what a plugin
//! does with a program chunk it is handed while another program is current; setting the
//! program first is the only order the opcodes' wording allows.

use tutti_plugin_types::MainThread;

use crate::error::{Result, Vst2Error};
use crate::loaded::Vst2Loaded;

/// State format tags:
/// - `b"CHK\x01"` = chunk-based state of the current program, with its index
/// - `b"PRM\x01"` = parameter-based state, with the program index
/// - `b"CHK\0"`, `b"PRM\0"` = the same with no program index (older states)
const STATE_HEADER_CHUNK: [u8; 4] = *b"CHK\x01";
const STATE_HEADER_PARAMS: [u8; 4] = *b"PRM\x01";
const STATE_HEADER_CHUNK_NO_PROGRAM: [u8; 4] = *b"CHK\0";
const STATE_HEADER_PARAMS_NO_PROGRAM: [u8; 4] = *b"PRM\0";

/// The program index a state records when the plugin has no programs to select.
const NO_PROGRAM: i32 = -1;

/// The parsed shape of a state blob's framing, produced by
/// [`parse_state_header`] before any plugin interaction. Splitting this out
/// keeps the pure framing/validation logic testable without a live plugin.
#[derive(Debug, PartialEq)]
pub(crate) enum StateHeader<'a> {
    /// A chunk payload — hand straight to `load_preset_data`, after the program is set.
    Chunk {
        /// The program to select first; `None` for a state that predates the index.
        program: Option<i32>,
        payload: &'a [u8],
    },
    /// A parameter snapshot: `count` normalized f32 values follow,
    /// carried in `values` (already length-validated against `count`).
    Params {
        /// The program to select first; `None` for a state that predates the index.
        program: Option<i32>,
        count: i32,
        values: &'a [u8],
    },
}

/// Validates the framing of a state blob and split off its payload.
///
/// Pure: performs only the header/length/count checks that don't need a live
/// plugin (the plugin-parameter-count cross-check stays in `write_state`).
/// Errors mirror the `write_state` early-returns exactly.
pub(crate) fn parse_state_header(data: &[u8]) -> Result<StateHeader<'_>> {
    if data.len() < 4 {
        return Err(Vst2Error::StateError(
            "State data too short (missing header)".into(),
        ));
    }

    let header: [u8; 4] = [data[0], data[1], data[2], data[3]];
    let body = &data[4..];

    let (with_program, chunk) = if header == STATE_HEADER_CHUNK {
        (true, true)
    } else if header == STATE_HEADER_CHUNK_NO_PROGRAM {
        (false, true)
    } else if header == STATE_HEADER_PARAMS {
        (true, false)
    } else if header == STATE_HEADER_PARAMS_NO_PROGRAM {
        (false, false)
    } else {
        return Err(Vst2Error::StateError(format!(
            "Unknown state header: {:?}",
            header
        )));
    };

    let (program, payload) = if with_program {
        if body.len() < 4 {
            return Err(Vst2Error::StateError(
                "State data too short (missing program index)".into(),
            ));
        }
        let program = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        (Some(program), &body[4..])
    } else {
        (None, body)
    };

    if chunk {
        if payload.is_empty() {
            return Err(Vst2Error::StateError("Empty chunk data".into()));
        }
        return Ok(StateHeader::Chunk { program, payload });
    }

    if payload.len() < 4 {
        return Err(Vst2Error::StateError(
            "Invalid parameter state (missing count)".into(),
        ));
    }

    let count = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if count < 0 {
        return Err(Vst2Error::StateError(format!(
            "Invalid parameter count: {}",
            count
        )));
    }

    let expected_payload = 4 + (count as usize) * 4;
    if payload.len() != expected_payload {
        return Err(Vst2Error::StateError(format!(
            "Parameter state size mismatch: expected {} bytes, got {}",
            expected_payload,
            payload.len()
        )));
    }

    // Strip the count prefix; leave just the f32 value bytes.
    Ok(StateHeader::Params {
        program,
        count,
        values: &payload[4..],
    })
}

impl Vst2Loaded {
    /// Captures the plugin's current state as a byte blob that
    /// `write_state` can restore.
    ///
    /// Uses the plugin's own chunk format (`effGetChunk`, the current program's) when it
    /// declares `effFlagsProgramChunks` and returns a non-empty chunk; otherwise saves
    /// every parameter's normalized value, which loses any state the plugin
    /// keeps outside its parameters. The blob starts with a four-byte tag
    /// (`CHK\x01` or `PRM\x01`) and the current program's index (see the module's docs),
    /// and is only meaningful to this crate.
    ///
    /// # Errors
    ///
    /// Returns [`Vst2Error::StateError`] if the plugin's chunk save fails, or if
    /// it declares parameters but exposes no `getParameter` to read them.
    ///
    /// `effGetChunk` is a main-thread opcode, so this takes the proof.
    pub fn read_state(&self, main: &MainThread) -> Result<Vec<u8>> {
        let info = self.instance.get_info();
        // `effGetProgram` is meaningful only for a plugin that has programs.
        let program = if self.instance.read_num_programs() > 0 {
            self.instance.get_preset_num(main)
        } else {
            NO_PROGRAM
        };

        if info.preset_chunks {
            // `try_get_preset_data`, not `get_preset_data`: the infallible one
            // folds "nothing saved" and "the save failed" into the same empty
            // `Vec`, so a failed `getChunk` would silently downgrade to a
            // parameter snapshot, losing the non-parameter state chunks exist
            // to carry. A failure is an error; an *empty* chunk still falls through
            // to the parameter snapshot, keeping a fresh plugin saveable.
            let chunk = self.instance.try_get_preset_data(main).map_err(|e| {
                Vst2Error::StateError(format!(
                    "plugin advertises effFlagsProgramChunks but its preset \
                     chunk save failed: {e}"
                ))
            })?;
            if !chunk.is_empty() {
                let mut state = Vec::with_capacity(8 + chunk.len());
                state.extend_from_slice(&STATE_HEADER_CHUNK);
                state.extend_from_slice(&program.to_le_bytes());
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
        let mut state = Vec::with_capacity(4 + 4 + 4 + (param_count as usize) * 4);
        state.extend_from_slice(&STATE_HEADER_PARAMS);
        state.extend_from_slice(&program.to_le_bytes());
        state.extend_from_slice(&param_count.to_le_bytes());

        for i in 0..param_count {
            // A plugin that advertises parameters but exposes no accessor
            // cannot be serialized: writing 0.0 would produce a blob that
            // restores silently and wrongly, which is worse than refusing.
            let Some(value) = self.instance.get_parameter(i) else {
                return Err(Vst2Error::StateError(format!(
                    "plugin reports {param_count} parameters but exposes no \
                     getParameter, so parameter {i} cannot be saved"
                )));
            };
            state.extend_from_slice(&value.to_le_bytes());
        }

        Ok(state)
    }

    /// Restores a state blob previously produced by
    /// [`read_state`](Self::read_state).
    ///
    /// Parameter values are clamped to `[0.0, 1.0]` before they are written.
    ///
    /// # Errors
    ///
    /// Returns [`Vst2Error::StateError`] if the blob is malformed (unknown
    /// header, truncated, or an empty chunk), if a parameter snapshot holds
    /// more parameters than the plugin declares, or if it exposes no `setParameter` to write
    /// parameters with. A plugin's own refusal of a chunk is not an error here: the header
    /// gives `effSetChunk` no return value to refuse with.
    pub(crate) fn write_state(&self, main: &MainThread, data: &[u8]) -> Result<()> {
        let state = parse_state_header(data)?;
        // The program first: a program chunk is the current program's, and a parameter
        // snapshot is its values. A recorded index the plugin does not have (or `-1`, for a
        // plugin that had no programs) selects nothing.
        let (StateHeader::Chunk { program, .. } | StateHeader::Params { program, .. }) = &state;
        if let Some(program) = *program
            && (0..self.instance.read_num_programs()).contains(&program)
        {
            self.instance.change_preset(main, program);
        }
        match state {
            StateHeader::Chunk { payload, .. } => {
                // `effSetChunk` has no return value in the header (aeffect.h documents
                // none, and a JUCE plugin answers 0 after loading), so there is no refusal
                // to read: the call returning is all that is known.
                self.instance.load_preset_data(main, payload);
                Ok(())
            }
            StateHeader::Params { count, values, .. } => {
                // `.max(0)` as in `read_state`: `numParams` is raw from the
                // `AEffect`. Unclamped, a plugin declaring `-1` makes this
                // comparison `0 > -1` and rejects the empty snapshot
                // `read_state` just wrote for that same plugin.
                let actual_count = self.instance.get_info().parameters.max(0);
                if count > actual_count {
                    return Err(Vst2Error::StateError(format!(
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
                    if !self.instance.set_parameter(i, value) {
                        // Reporting success here would claim a preset was
                        // restored while every value went nowhere.
                        return Err(Vst2Error::StateError(format!(
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
        assert!(matches!(err, Vst2Error::StateError(_)));
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn unknown_header() {
        let err = parse_state_header(b"XXX\0payload").unwrap_err();
        assert!(err.to_string().contains("Unknown state header"));
    }

    #[test]
    fn chunk_empty_payload_rejected() {
        let mut data = STATE_HEADER_CHUNK.to_vec();
        data.extend_from_slice(&0i32.to_le_bytes());
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("Empty chunk data"));
    }

    #[test]
    fn chunk_valid() {
        let mut data = STATE_HEADER_CHUNK.to_vec();
        data.extend_from_slice(&3i32.to_le_bytes());
        data.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            parse_state_header(&data).unwrap(),
            StateHeader::Chunk {
                program: Some(3),
                payload: &[1, 2, 3, 4]
            }
        );
    }

    #[test]
    fn a_state_without_a_program_index_is_still_read() {
        let mut data = STATE_HEADER_CHUNK_NO_PROGRAM.to_vec();
        data.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            parse_state_header(&data).unwrap(),
            StateHeader::Chunk {
                program: None,
                payload: &[1, 2, 3, 4]
            }
        );
    }

    #[test]
    fn a_program_state_cut_before_its_index_is_refused() {
        let mut data = STATE_HEADER_CHUNK.to_vec();
        data.extend_from_slice(&[0, 0]);
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("missing program index"));
    }

    #[test]
    fn params_missing_count() {
        // Header present but fewer than 4 count bytes follow.
        let mut data = STATE_HEADER_PARAMS_NO_PROGRAM.to_vec();
        data.extend_from_slice(&[0, 0]);
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("missing count"));
    }

    #[test]
    fn params_negative_count() {
        let mut data = STATE_HEADER_PARAMS_NO_PROGRAM.to_vec();
        data.extend_from_slice(&(-1i32).to_le_bytes());
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("Invalid parameter count"));
    }

    #[test]
    fn params_size_mismatch() {
        // count says 2 params (needs 4 + 2*4 = 12 bytes payload) but only 1
        // value's worth of bytes follow.
        let mut data = STATE_HEADER_PARAMS_NO_PROGRAM.to_vec();
        data.extend_from_slice(&2i32.to_le_bytes());
        data.extend_from_slice(&1.0f32.to_le_bytes());
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("size mismatch"));
    }

    #[test]
    fn params_valid_roundtrip() {
        let mut data = STATE_HEADER_PARAMS_NO_PROGRAM.to_vec();
        data.extend_from_slice(&2i32.to_le_bytes());
        data.extend_from_slice(&0.25f32.to_le_bytes());
        data.extend_from_slice(&0.75f32.to_le_bytes());

        match parse_state_header(&data).unwrap() {
            StateHeader::Params {
                program,
                count,
                values,
            } => {
                assert_eq!(program, None);
                assert_eq!(count, 2);
                assert_eq!(values.len(), 8);
                assert_eq!(f32::from_le_bytes(values[0..4].try_into().unwrap()), 0.25);
                assert_eq!(f32::from_le_bytes(values[4..8].try_into().unwrap()), 0.75);
            }
            other => panic!("expected Params, got {other:?}"),
        }
    }

    #[test]
    fn params_with_a_program_index() {
        let mut data = STATE_HEADER_PARAMS.to_vec();
        data.extend_from_slice(&5i32.to_le_bytes());
        data.extend_from_slice(&1i32.to_le_bytes());
        data.extend_from_slice(&0.5f32.to_le_bytes());
        match parse_state_header(&data).unwrap() {
            StateHeader::Params {
                program,
                count,
                values,
            } => {
                assert_eq!((program, count, values.len()), (Some(5), 1, 4));
            }
            other => panic!("expected Params, got {other:?}"),
        }
    }
}
