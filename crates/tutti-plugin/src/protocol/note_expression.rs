//! MPE / note expression wire types.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

const NOTE_EXPR_STACK_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteExpressionType {
    Volume,
    Pan,
    Tuning,
    Vibrato,
    Brightness,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NoteExpressionValue {
    pub sample_offset: i32,
    pub note_id: i32,
    pub expression_type: NoteExpressionType,
    pub value: f64,
}

pub type NoteExpressionVec = SmallVec<[NoteExpressionValue; NOTE_EXPR_STACK_CAPACITY]>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoteExpressionChanges {
    pub changes: NoteExpressionVec,
}

impl NoteExpressionChanges {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_change(&mut self, change: NoteExpressionValue) {
        self.changes.push(change);
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}
