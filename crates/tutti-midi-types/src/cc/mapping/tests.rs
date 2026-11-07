use super::*;

#[test]
fn test_map_value() {
    let mapping = CCMapping::new(
        Some(MidiChannel::FIRST),
        CCNumber::MOD_WHEEL,
        CCTarget::MasterVolume,
        0.0,
        1.0,
    );
    assert_eq!(mapping.map_value(0), 0.0);
    assert_eq!(mapping.map_value(127), 1.0);
    assert!((mapping.map_value(64) - 0.504).abs() < 0.01);
}

#[test]
fn test_map_value_custom_range() {
    let mapping = CCMapping::new(
        Some(MidiChannel::FIRST),
        CCNumber::MOD_WHEEL,
        CCTarget::Tempo,
        60.0,
        200.0,
    );
    assert_eq!(mapping.map_value(0), 60.0);
    assert_eq!(mapping.map_value(127), 200.0);
}

#[test]
fn test_matches() {
    let mapping = CCMapping::new(
        Some(MidiChannel::FIRST),
        CCNumber::MOD_WHEEL,
        CCTarget::MasterVolume,
        0.0,
        1.0,
    );
    assert!(mapping.matches(MidiChannel::FIRST, CCNumber::MOD_WHEEL));
    assert!(!mapping.matches(MidiChannel::of(1), CCNumber::MOD_WHEEL)); // Wrong channel
    assert!(!mapping.matches(MidiChannel::FIRST, CCNumber::BREATH)); // Wrong CC

    // Test any channel
    let any_channel = CCMapping::new(None, CCNumber::MOD_WHEEL, CCTarget::MasterVolume, 0.0, 1.0);
    assert!(any_channel.matches(MidiChannel::FIRST, CCNumber::MOD_WHEEL));
    assert!(any_channel.matches(MidiChannel::LAST, CCNumber::MOD_WHEEL));
}

#[test]
fn test_matches_disabled() {
    let mut mapping = CCMapping::new(
        Some(MidiChannel::FIRST),
        CCNumber::MOD_WHEEL,
        CCTarget::MasterVolume,
        0.0,
        1.0,
    );
    assert!(mapping.matches(MidiChannel::FIRST, CCNumber::MOD_WHEEL));

    mapping.enabled = false;
    assert!(!mapping.matches(MidiChannel::FIRST, CCNumber::MOD_WHEEL));
}

#[test]
fn test_map_value_inverted_range() {
    // Inverted mapping: CC 0 → 1.0, CC 127 → 0.0
    let mapping = CCMapping::new(
        Some(MidiChannel::FIRST),
        CCNumber::MOD_WHEEL,
        CCTarget::MasterVolume,
        1.0,
        0.0,
    );
    assert_eq!(mapping.map_value(0), 1.0);
    assert_eq!(mapping.map_value(127), 0.0);
    assert!((mapping.map_value(64) - 0.496).abs() < 0.01);
}

#[test]
fn test_map_value_same_range() {
    // Constant output: min == max
    let mapping = CCMapping::new(
        Some(MidiChannel::FIRST),
        CCNumber::MOD_WHEEL,
        CCTarget::MasterVolume,
        0.5,
        0.5,
    );
    assert_eq!(mapping.map_value(0), 0.5);
    assert_eq!(mapping.map_value(64), 0.5);
    assert_eq!(mapping.map_value(127), 0.5);
}
