use super::*;

const RATE: f64 = 48_000.0;

fn check(freqs: &[f32]) -> Result<(), CrossoverError> {
    let f: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
    validate(&f, SampleRate::of(RATE))
}

/// The band count is 2 to 6.
#[test]
fn the_band_count_is_two_to_six() {
    assert!(matches!(
        check(&[]),
        Err(CrossoverError::Bands {
            bands: 1,
            min: 2,
            max: 6
        })
    ));
    assert!(check(&[100.0]).is_ok());
    assert!(check(&[1.0, 2.0, 3.0, 4.0, 5.0]).is_ok());
    assert!(matches!(
        check(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        Err(CrossoverError::Bands { bands: 7, .. })
    ));
}

/// Frequencies must be finite, above 0, strictly increasing, below Nyquist.
#[test]
fn frequencies_are_validated() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -5.0] {
        assert!(
            matches!(
                check(&[bad]),
                Err(CrossoverError::Frequency { index: 0, .. })
            ),
            "{bad}"
        );
        assert!(
            matches!(
                check(&[100.0, bad]),
                Err(CrossoverError::Frequency { index: 1, .. })
            ),
            "{bad}"
        );
    }
    assert_eq!(
        check(&[500.0, 500.0]).unwrap_err(),
        CrossoverError::Order { index: 1 }
    );
    assert_eq!(
        check(&[500.0, 100.0]).unwrap_err(),
        CrossoverError::Order { index: 1 }
    );
    assert!(matches!(
        check(&[24_000.0]),
        Err(CrossoverError::Nyquist { index: 0, .. })
    ));
    assert!(matches!(
        check(&[100.0, 30_000.0]),
        Err(CrossoverError::Nyquist { index: 1, .. })
    ));
    assert!(check(&[23_999.0]).is_ok());
}

/// The messages say what is wrong.
#[test]
fn errors_name_what_is_wrong() {
    let e = check(&[500.0, 100.0]).unwrap_err().to_string();
    assert!(e.contains("crossover 1") && e.contains("increase"), "{e}");
}

/// A set is fitted under what the coefficients can be solved at, strictly increasing
/// still: a frequency out of range costs only itself.
#[test]
fn a_set_is_fitted_under_the_rate_and_stays_increasing() {
    let rate = SampleRate::of(RATE);
    let mut set = [Hz(30_000.0), Hz(25_000.0)];
    fit_to_rate(&mut set, rate);
    assert!(set[0] < set[1], "{set:?}");
    assert!(f64::from(set[1].get()) <= SOLVE_CEILING * RATE);
    let mut ok = [Hz(100.0), Hz(1_000.0)];
    fit_to_rate(&mut ok, rate);
    assert_eq!(ok, [Hz(100.0), Hz(1_000.0)]);
}
