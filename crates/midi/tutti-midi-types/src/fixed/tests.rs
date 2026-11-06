use super::*;

/// Analytic: the integer part is the high 7 bits, the fraction the rest.
///
/// Mutations (each fails): dividing by `1 << 24`; dividing by `1 << 8` in
/// the Q7.9 case.
#[test]
fn integer_and_fraction_split() {
    assert_eq!(q7_25_to_f32(0), 0.0);
    assert_eq!(q7_25_to_f32(127 << 25), 127.0);
    assert_eq!(q7_25_to_f32((12 << 25) | (1 << 24)), 12.5);
    assert_eq!(q7_25_to_f32((12 << 25) | (1 << 23)), 12.25);
    assert_eq!(q7_9_to_f32(0), 0.0);
    assert_eq!(q7_9_to_f32((127 << 9) | 128), 127.25);
}
