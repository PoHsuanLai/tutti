use super::*;

/// A 7-bit wire velocity survives the trip through [`Velocity`] unchanged.
///
/// The property that matters at an input edge: a controller sending 100 and
/// a document storing that note must produce the same 100 on the way out,
/// or every save/load cycle drifts the dynamics.
#[test]
fn a_7bit_velocity_round_trips_through_the_unit() {
    for v in 0u8..=127 {
        assert_eq!(velocity_to_midi1(velocity_from_midi1(v)), v, "velocity {v}");
    }
}

/// Out-of-range input is clamped, not wrapped.
///
/// `as u8` on a value above 255 wraps, which would turn the loudest
/// possible note into a near-silent one.
#[test]
fn an_out_of_range_velocity_clamps() {
    use tutti_types::Velocity;
    assert_eq!(velocity_to_midi1(Velocity(1.5)), 127);
    assert_eq!(velocity_to_midi1(Velocity(-0.5)), 0);
}

#[test]
fn spec_min_center_max_vectors() {
    // M2-104 Appendix D.1.3 worked examples — a linear scale fails these.
    assert_eq!(midi1_velocity_to_midi2(10), 0x1400);
    assert_eq!(midi1_velocity_to_midi2(64), 0x8000); // center → center
    assert_eq!(midi1_velocity_to_midi2(87), 0xaeba);
    assert_eq!(midi1_velocity_to_midi2(127), 0xffff);
    assert_eq!(midi1_pitch_bend_to_midi2(8192), 0x8000_0000); // bend center
    assert_eq!(midi1_cc_to_midi2(127), 0xffff_ffff);
    assert_eq!(midi1_cc_to_midi2(0), 0);
}

/// Promoting a MIDI 1.0 value and narrowing it back reproduces the original
/// for *every* code in the source range — the MCM widening in Appendix D.1.3
/// is exactly invertible, and a 1.0 device round-tripped through a 2.0 graph
/// must not drift.
#[test]
fn every_midi1_code_survives_promotion_and_narrowing() {
    for v in 0..=127u8 {
        assert_eq!(
            midi2_velocity_to_midi1(midi1_velocity_to_midi2(v)),
            v,
            "velocity {v}"
        );
        assert_eq!(midi2_cc_to_midi1(midi1_cc_to_midi2(v)), v, "cc {v}");
    }
    // Pitch bend is 14-bit, so it gets its own full sweep.
    for v in 0..=16383u16 {
        assert_eq!(
            midi2_pitch_bend_to_midi1(midi1_pitch_bend_to_midi2(v)),
            v,
            "bend {v}"
        );
    }
}

#[test]
fn unit_f32_inverses_are_exact_enough() {
    for &u in &[0u16, 1, 12345, 40000, u16::MAX] {
        assert_eq!(unit_f32_to_u16(u16_to_unit_f32(u)), u);
    }
    // u32 has more bits than f32's mantissa; allow a small relative slack.
    for &u in &[0u32, 1, 0x4000_0000, 0x8000_0000, u32::MAX] {
        let back = unit_f32_to_u32(u32_to_unit_f32(u));
        let diff = (back as i64 - u as i64).unsigned_abs();
        assert!(diff <= 256, "u32 round-trip {u} -> {back}");
    }
}

#[test]
fn signed_bend_inverse_centers() {
    assert_eq!(signed_f32_to_bend_u32(0.0), 0x8000_0000);
    assert_eq!(signed_f32_to_bend_u32(-1.0), 0);
    assert_eq!(signed_f32_to_bend_u32(1.0), u32::MAX);
    // Clamps out-of-range input.
    assert_eq!(signed_f32_to_bend_u32(-2.0), 0);
    assert_eq!(signed_f32_to_bend_u32(2.0), u32::MAX);
}
/// **A `Velocity` reaches the wire at 16-bit resolution, not 7.**
///
/// This regressed once and was invisible: `velocity_to_midi2` routed through
/// the 7-bit rung (`midi1_velocity_to_midi2(velocity_to_midi1(v))`) to keep
/// the center exact, which capped the whole float type at **128 distinct
/// codes**. `Velocity` is float-backed *precisely because* the wire field is
/// 16-bit, so that silently undid the reason the type exists.
///
/// It hid because the two directions were asymmetric — `to` went via MCM,
/// `from` was a plain divide — so a round trip of any 7-bit value was exact,
/// and those were the only values the path could emit. It was
/// self-consistently lossy.
#[test]
fn a_velocity_is_not_quantized_to_seven_bits() {
    use std::collections::BTreeSet;
    let codes: BTreeSet<u16> = (0..=1000)
        .map(|i| velocity_to_midi2(tutti_types::Velocity(i as f32 / 1000.0)))
        .collect();
    assert!(
        codes.len() > 900,
        "1001 distinct inputs collapsed to {} codes; 128 means the 7-bit rung is back",
        codes.len()
    );
}

/// The three fixed points stay exact — the property the 7-bit rung existed
/// to protect, and the reason a plain `* 65535.0` is wrong (`0.5 * 65535.0`
/// is `32767.5`, which is not `0x8000`).
#[test]
fn velocity_endpoints_and_centre_are_exact() {
    use tutti_types::Velocity;
    assert_eq!(velocity_to_midi2(Velocity::SILENT), 0x0000);
    assert_eq!(
        velocity_to_midi2(Velocity::CENTER),
        0x8000,
        "centre must be mezzo-forte"
    );
    assert_eq!(velocity_to_midi2(Velocity::MAX), u16::MAX);
    // And `note_on_7bit`'s path agrees with the widened one, so the two
    // constructors cannot disagree about the same note.
    assert_eq!(
        velocity_to_midi2(Velocity::CENTER),
        midi1_velocity_to_midi2(velocity_to_midi1(Velocity::CENTER))
    );
}

/// A `Velocity` survives a round trip far better than the 7-bit grid allows.
#[test]
fn a_velocity_round_trips_within_one_code() {
    for i in 0..=1000 {
        let v = i as f32 / 1000.0;
        let back = velocity_from_midi2(velocity_to_midi2(tutti_types::Velocity(v))).get();
        assert!(
            (back - v).abs() < 1e-4,
            "round trip of {v} returned {back}; the 7-bit grid would be 0.0078 off"
        );
    }
}

/// The two directions are inverses at the center, not merely near it.
///
/// A plain `v / 0xffff` would map `0x8000` to `0.5000076`, so `CENTER`
/// would not survive a round trip; both halves are piecewise about the
/// same anchor instead. A forward path that only emits 7-bit codes would
/// hide the error below the grid.
#[test]
fn the_two_velocity_directions_are_inverses() {
    use tutti_types::Velocity;
    for v in [Velocity::SILENT, Velocity::CENTER, Velocity::MAX] {
        assert_eq!(velocity_from_midi2(velocity_to_midi2(v)), v);
    }
    for code in [0u16, 0x4000, 0x8000, 0xC000, 0xffff] {
        assert_eq!(velocity_to_midi2(velocity_from_midi2(code)), code);
    }
}
