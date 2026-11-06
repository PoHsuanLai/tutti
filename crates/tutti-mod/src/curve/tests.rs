use super::*;
use audio_automation::AutomationPoint;

fn envelope() -> AutomationEnvelope<()> {
    let mut e = AutomationEnvelope::new(());
    e.add_point(AutomationPoint::with_curve(
        1.0,
        0.2,
        CurveType::Exponential,
    ));
    e.add_point(AutomationPoint::with_curve(2.0, 0.8, CurveType::Linear));
    e.add_point(AutomationPoint::new(3.0, 0.4));
    e
}

/// An envelope's pieces are its breakpoints: a hold before the first
/// and from the last, a move between, each agreeing with `value_at`
/// over its span; a breakpoint starts the piece after it.
///
/// Mutation (run): `partition_point(|p| p.time < b)` (a breakpoint ends
/// the piece before it) → beat 2.0 reads the exponential piece → fails.
#[test]
fn an_envelopes_pieces_are_its_breakpoints() {
    let e = envelope();
    assert_eq!(
        e.segment_at(Beat(0.5)),
        Some(CurveSegment::Hold {
            value: 0.2,
            until: Some(Beat(1.0))
        })
    );
    let exp = CurveSegment::Move {
        start: Beat(1.0),
        end: Beat(2.0),
        from: 0.2,
        to: 0.8,
        shape: CurveType::Exponential,
    };
    assert_eq!(e.segment_at(Beat(1.0)), Some(exp));
    assert_eq!(e.segment_at(Beat(1.7)), Some(exp));
    assert!(matches!(
        e.segment_at(Beat(2.0)),
        Some(CurveSegment::Move {
            shape: CurveType::Linear,
            ..
        })
    ));
    assert_eq!(
        e.segment_at(Beat(9.0)),
        Some(CurveSegment::Hold {
            value: 0.4,
            until: None
        })
    );
}

/// A piece the envelope's constraints would bend is no segment: a step
/// size, or a clamp cutting into its endpoints. A clamp both endpoints
/// stay inside leaves it one; a disabled envelope has none.
///
/// Mutation (run): drop the endpoint check (`inside`) → the clamped
/// piece is a segment → fails.
#[test]
fn a_bent_piece_is_no_segment() {
    assert!(envelope().with_step(0.1).segment_at(Beat(1.5)).is_none());
    assert!(envelope().with_max(0.5).segment_at(Beat(1.5)).is_none());
    assert!(
        envelope()
            .with_range(0.0, 1.0)
            .segment_at(Beat(1.5))
            .is_some()
    );
    let mut off = envelope();
    off.enabled = false;
    assert!(off.segment_at(Beat(1.5)).is_none());
}
