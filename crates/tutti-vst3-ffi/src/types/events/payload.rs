//! The payload types of [`Payload`](super::Payload).

use super::{TextRef, sdk};

/// Wraps an SDK event struct that is plain data, with a `Debug` that lists
/// the fields (the bindings derive none) and `Deref` to the struct.
macro_rules! sdk_event {
    ($(#[$m:meta])* $name:ident($sdk:ident) { $($field:ident),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy)]
        pub struct $name(pub sdk::$sdk);

        impl From<sdk::$sdk> for $name {
            fn from(raw: sdk::$sdk) -> Self {
                Self(raw)
            }
        }

        impl std::ops::Deref for $name {
            type Target = sdk::$sdk;

            fn deref(&self) -> &sdk::$sdk {
                &self.0
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name))
                    $(.field(stringify!($field), &self.0.$field))*
                    .finish()
            }
        }
    };
}

sdk_event! {
    /// `NoteOnEvent`: velocity is normalized to `0.0..=1.0`.
    NoteOn(NoteOnEvent) { channel, pitch, tuning, velocity, length, noteId }
}
sdk_event! {
    /// `NoteOffEvent`: velocity is the normalized release velocity.
    NoteOff(NoteOffEvent) { channel, pitch, velocity, noteId, tuning }
}
sdk_event! {
    /// `PolyPressureEvent`: per-note aftertouch, normalized to `0.0..=1.0`.
    PolyPressure(PolyPressureEvent) { channel, pitch, pressure, noteId }
}
sdk_event! {
    /// `NoteExpressionValueEvent`: a normalized per-note expression value.
    ExpressionValue(NoteExpressionValueEvent) { typeId, noteId, value }
}
sdk_event! {
    /// `NoteExpressionIntValueEvent`: a stepped expression. The SDK's `value`
    /// is unsigned; the signed reading is the caller's.
    ExpressionInt(NoteExpressionIntValueEvent) { typeId, noteId, value }
}
sdk_event! {
    /// `LegacyMIDICCOutEvent`: a CC the plugin sends to the host.
    LegacyCcOut(LegacyMIDICCOutEvent) { controlNumber, channel, value, value2 }
}

impl NoteOn {
    /// A note-on with no length.
    #[must_use]
    pub fn new(channel: i16, pitch: i16, velocity: f32, note_id: i32) -> Self {
        Self(sdk::NoteOnEvent {
            channel,
            pitch,
            tuning: 0.0,
            velocity,
            length: 0,
            noteId: note_id,
        })
    }
}

impl NoteOff {
    /// A note-off with release `velocity`.
    #[must_use]
    pub fn new(channel: i16, pitch: i16, velocity: f32, note_id: i32) -> Self {
        Self(sdk::NoteOffEvent {
            channel,
            pitch,
            velocity,
            noteId: note_id,
            tuning: 0.0,
        })
    }
}

impl PolyPressure {
    /// Pressure on a note.
    #[must_use]
    pub fn new(channel: i16, pitch: i16, pressure: f32, note_id: i32) -> Self {
        Self(sdk::PolyPressureEvent {
            channel,
            pitch,
            pressure,
            noteId: note_id,
        })
    }
}

impl ExpressionValue {
    /// A normalized value for expression `type_id` of a note.
    #[must_use]
    pub fn new(type_id: u32, note_id: i32, value: f64) -> Self {
        Self(sdk::NoteExpressionValueEvent {
            typeId: type_id,
            noteId: note_id,
            value,
        })
    }
}

impl ExpressionInt {
    /// A stepped value for expression `type_id` of a note.
    #[must_use]
    pub fn new(type_id: u32, note_id: i32, value: u64) -> Self {
        Self(sdk::NoteExpressionIntValueEvent {
            typeId: type_id,
            noteId: note_id,
            value,
        })
    }
}

/// `DataEvent` with its bytes inline.
#[derive(Debug, Clone, Copy)]
///
/// The fields are private: `size` is what the plugin is told it may read from
/// `bytes`, so it must never exceed [`CAPACITY`](Self::CAPACITY), and every
/// constructor keeps it so. A literal cannot say otherwise:
///
/// ```compile_fail,E0451
/// use tutti_vst3_ffi::Data;
///
/// let _ = Data { size: 1_000_000, data_type: 0, bytes: [0; 16] };
/// ```
pub struct Data {
    /// Valid bytes in `bytes`, at most 16.
    size: u32,
    /// The `DataEvent.type` subtype.
    data_type: u32,
    /// The payload; only the first `size` bytes are valid.
    bytes: [u8; Data::CAPACITY],
}

impl Data {
    /// Bytes held inline.
    pub const CAPACITY: usize = 16;

    /// `bytes` under `data_type`, a compile error past [`CAPACITY`](Self::CAPACITY).
    #[must_use]
    pub fn inline<const N: usize>(data_type: u32, bytes: [u8; N]) -> Self {
        const { assert!(N <= Data::CAPACITY) };
        let mut inline = [0; Self::CAPACITY];
        inline[..N].copy_from_slice(&bytes);
        Self {
            #[allow(clippy::cast_possible_truncation, reason = "N <= 16, asserted above")]
            size: N as u32,
            data_type,
            bytes: inline,
        }
    }

    /// `bytes` under `data_type`, or `None` past [`CAPACITY`](Self::CAPACITY).
    #[must_use]
    pub fn new(data_type: u32, bytes: &[u8]) -> Option<Self> {
        let mut inline = [0; Self::CAPACITY];
        inline.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(Self {
            size: u32::try_from(bytes.len()).ok()?,
            data_type,
            bytes: inline,
        })
    }

    /// `bytes` under `data_type`, or `None` when they run past
    /// [`CAPACITY`](Self::CAPACITY).
    #[must_use]
    pub fn try_from_iter(data_type: u32, bytes: impl IntoIterator<Item = u8>) -> Option<Self> {
        let mut inline = [0; Self::CAPACITY];
        let mut size = 0;
        for byte in bytes {
            *inline.get_mut(size)? = byte;
            size += 1;
        }
        Some(Self {
            size: u32::try_from(size).ok()?,
            data_type,
            bytes: inline,
        })
    }

    /// The bytes a plugin sent, `size` clamped to
    /// [`CAPACITY`](Self::CAPACITY).
    pub(super) fn from_plugin(data_type: u32, size: u32, bytes: [u8; Self::CAPACITY]) -> Self {
        Self {
            #[allow(clippy::cast_possible_truncation, reason = "CAPACITY is 16")]
            size: size.min(Self::CAPACITY as u32),
            data_type,
            bytes,
        }
    }

    /// Valid bytes in the payload, at most [`CAPACITY`](Self::CAPACITY).
    #[must_use]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// The `DataEvent.type` subtype.
    #[must_use]
    pub fn data_type(&self) -> u32 {
        self.data_type
    }

    /// The valid bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.bytes[..(self.size as usize).min(Self::CAPACITY)]
    }
}

/// A note-expression text annotation; `text` indexes the list's arena.
#[derive(Debug, Clone, Copy)]
pub struct ExpressionText {
    /// The expression the text annotates.
    pub type_id: u32,
    /// The note's id.
    pub note_id: i32,
    /// The text.
    pub text: TextRef,
}

/// A chord context change; `text` indexes the list's arena.
#[derive(Debug, Clone, Copy)]
pub struct Chord {
    /// Root as a MIDI note number.
    pub root: i16,
    /// Bass note as a MIDI note number.
    pub bass_note: i16,
    /// Bitmask of chord degrees.
    pub mask: i16,
    /// Display name.
    pub text: TextRef,
}

/// A scale context change; `text` indexes the list's arena.
#[derive(Debug, Clone, Copy)]
pub struct Scale {
    /// Root as a MIDI note number.
    pub root: i16,
    /// 12-bit mask of scale degrees, bit 0 = C.
    pub mask: i16,
    /// Display name.
    pub text: TextRef,
}
