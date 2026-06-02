//! MIDI 1.0 ↔ MIDI 2.0 scalar resolution conversion (spec Min-Center-Max).
//!
//! `midi2` applies the same scaling inside its `HybridSchemaProperty`
//! conversions when typed messages are rebuffered between 7-bit and UMP
//! forms. These free functions cover places where a tutti consumer has a
//! raw 7-bit value (from a hardware port, a DAW automation knob, or a test
//! helper) and wants the 16/32-bit UMP value to pass into a constructor like
//! [`MidiEvent::note_on`](crate::ump::MidiEvent::note_on).

/// 7-bit velocity → 16-bit velocity (MIDI 2.0 native form).
#[inline]
pub fn midi1_velocity_to_midi2(v: u8) -> u16 {
    if v == 0 {
        0
    } else {
        let v7 = u32::from(v);
        ((v7 * 65535 + 63) / 127) as u16
    }
}

/// 16-bit velocity → 7-bit (lossy).
#[inline]
pub fn midi2_velocity_to_midi1(v: u16) -> u8 {
    if v == 0 {
        0
    } else {
        let v16 = u32::from(v);
        ((v16 * 127 + 32767) / 65535).min(127) as u8
    }
}

/// 7-bit CC / pressure → 32-bit (MIDI 2.0 native form).
#[inline]
pub fn midi1_cc_to_midi2(v: u8) -> u32 {
    if v == 0 {
        0
    } else if v == 127 {
        u32::MAX
    } else {
        let v7 = u64::from(v);
        ((v7 * u32::MAX as u64 + 63) / 127) as u32
    }
}

/// 32-bit CC / pressure → 7-bit (lossy).
#[inline]
pub fn midi2_cc_to_midi1(v: u32) -> u8 {
    if v == 0 {
        0
    } else {
        let v32 = u64::from(v);
        ((v32 * 127 + 0x7FFF_FFFF) / u32::MAX as u64).min(127) as u8
    }
}

/// 14-bit pitch bend (center 8192) → 32-bit (center 0x8000_0000).
#[inline]
pub fn midi1_pitch_bend_to_midi2(v: u16) -> u32 {
    if v == 0 {
        0
    } else if v == 16383 {
        u32::MAX
    } else {
        let v14 = u64::from(v);
        ((v14 * u32::MAX as u64 + 8191) / 16383) as u32
    }
}

/// 32-bit pitch bend → 14-bit (lossy).
#[inline]
pub fn midi2_pitch_bend_to_midi1(v: u32) -> u16 {
    if v == 0 {
        0
    } else {
        let v32 = u64::from(v);
        ((v32 * 16383 + 0x7FFF_FFFF) / u32::MAX as u64).min(16383) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_roundtrip() {
        for v in 0..=127u8 {
            assert_eq!(midi2_velocity_to_midi1(midi1_velocity_to_midi2(v)), v);
        }
    }

    #[test]
    fn cc_roundtrip() {
        for v in 0..=127u8 {
            assert_eq!(midi2_cc_to_midi1(midi1_cc_to_midi2(v)), v);
        }
    }

    #[test]
    fn pitch_bend_roundtrip() {
        for v in 0..=16383u16 {
            assert_eq!(midi2_pitch_bend_to_midi1(midi1_pitch_bend_to_midi2(v)), v);
        }
    }
}
