//! Boxed bulk payloads for the `ProcessAudio*` wire messages.

use serde::{Deserialize, Serialize};

use super::midi::IpcMidiEventVec;
use super::note_expression::NoteExpressionChanges;
use super::parameters::ParameterChanges;
use super::transport::TransportInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessAudioMidiData {
    pub buffer_id: u32,
    pub num_samples: usize,
    pub midi_events: IpcMidiEventVec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessAudioFullData {
    pub buffer_id: u32,
    pub num_samples: usize,
    pub midi_events: IpcMidiEventVec,
    pub param_changes: ParameterChanges,
    pub note_expression: NoteExpressionChanges,
    pub transport: TransportInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioProcessedMidiData {
    pub latency_us: u64,
    pub midi_output: IpcMidiEventVec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioProcessedFullData {
    pub latency_us: u64,
    pub midi_output: IpcMidiEventVec,
    pub param_output: ParameterChanges,
    pub note_expression_output: NoteExpressionChanges,
}
