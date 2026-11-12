//! A parameter edit as a plugin reports it, whatever its format: [`ParamEdit`].
//!
//! Every format has its own way to say "the plugin moved one of its own
//! parameters": VST3's `beginEdit`, `performEdit` and `endEdit`, VST2's
//! `audioMasterBeginEdit`, `audioMasterAutomate` and `audioMasterEndEdit`,
//! CLAP's gesture and value output events, AU's parameter listener. They are
//! one event here (doc 029 section 6.1).
//!
//! The two ends of a gesture carry no value in any format, so the type has
//! none for them: only [`Change`](ParamEdit::Change) holds a value. A host that
//! wants the value a gesture began or ended at reads it from the neighbouring
//! change, or from the parameter, itself; nothing is made up here.

use crate::{Normalized, ParamId};

/// One parameter edit, in the order the plugin made it: the plugin takes hold
/// of a parameter ([`Begin`](Self::Begin)), moves it
/// ([`Change`](Self::Change), any number of times) and lets go
/// ([`End`](Self::End)).
///
/// A change with no `Begin` before it is an edit that is not a gesture: a
/// plugin that moved a value of its own accord (a preset, its own automation).
/// A host that records automation touches the parameter from `Begin` to `End`,
/// so every `Begin` and `End` a plugin sent is reported, never dropped.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ParamEdit {
    /// The plugin took hold of the parameter (VST3 `beginEdit`, VST2
    /// `audioMasterBeginEdit`, CLAP `CLAP_EVENT_PARAM_GESTURE_BEGIN`, AU
    /// `kAudioUnitEvent_BeginParameterChangeGesture`).
    Begin(ParamId),
    /// The parameter took a new value (VST3 `performEdit`, VST2
    /// `audioMasterAutomate`, CLAP `CLAP_EVENT_PARAM_VALUE`, AU
    /// `kAudioUnitEvent_ParameterValueChange`).
    Change {
        /// The parameter, by the id the plugin reports.
        id: ParamId,
        /// Its normalized value.
        value: Normalized,
        /// Whether a host that records automation should record this edit.
        ///
        /// `false` only for a CLAP event the plugin flagged
        /// `CLAP_EVENT_DONT_RECORD` (events.h: it "should not be recorded", for a
        /// value that moved because of another recorded source, such as a MIDI
        /// CC). The value is still the parameter's, and a host that shows it
        /// follows it either way.
        record: bool,
    },
    /// The plugin let go of the parameter (VST3 `endEdit`, VST2
    /// `audioMasterEndEdit`, CLAP `CLAP_EVENT_PARAM_GESTURE_END`, AU
    /// `kAudioUnitEvent_EndParameterChangeGesture`).
    End(ParamId),
}

impl ParamEdit {
    /// `id` took `value`, to be recorded.
    #[must_use]
    pub const fn change(id: ParamId, value: Normalized) -> Self {
        Self::Change {
            id,
            value,
            record: true,
        }
    }

    /// This edit, flagged as one a recording host should not record. A gesture's
    /// ends are not values and are returned as they are.
    #[must_use]
    pub const fn unrecorded(self) -> Self {
        match self {
            Self::Change { id, value, .. } => Self::Change {
                id,
                value,
                record: false,
            },
            other => other,
        }
    }

    /// The parameter the edit is about.
    #[must_use]
    pub const fn id(&self) -> ParamId {
        match *self {
            Self::Begin(id) | Self::End(id) | Self::Change { id, .. } => id,
        }
    }

    /// The value of a change; `None` for a gesture's begin or end, which carry
    /// none.
    #[must_use]
    pub const fn value(&self) -> Option<Normalized> {
        match *self {
            Self::Change { value, .. } => Some(value),
            _ => None,
        }
    }

    /// Whether a host that records automation should record it: `false` only
    /// for a change flagged not to be.
    #[must_use]
    pub const fn record(&self) -> bool {
        !matches!(*self, Self::Change { record: false, .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gesture's ends hold no value and a change does; the recording flag
    /// belongs to a change alone.
    #[test]
    fn only_a_change_holds_a_value_and_a_record_flag() {
        let id = ParamId::new(3);
        let value = Normalized::of(0.5);
        assert_eq!(ParamEdit::Begin(id).value(), None);
        assert_eq!(ParamEdit::End(id).value(), None);
        assert_eq!(ParamEdit::change(id, value).value(), Some(value));
        assert!(ParamEdit::change(id, value).record());
        assert!(!ParamEdit::change(id, value).unrecorded().record());
        assert!(ParamEdit::Begin(id).unrecorded().record());
        assert_eq!(ParamEdit::End(id).id(), id);
    }
}
