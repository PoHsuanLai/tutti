//! [`Lti`]: a linear time-invariant system.

use tutti_types::Hz;

use super::System;
use crate::Complex64;

/// A linear time-invariant system (design 024 section 4.2, class 1; order 0 is a
/// gain matrix). The queries are the system's invariants, answered from its
/// structure and never from its realization.
///
/// `spectral_radius`, `l1_norm` and `hinf_norm` are **sufficient bounds**: they
/// never fall under the true figure, and the contract measures that the
/// realization is under them.
pub trait Lti: System {
    /// The input signals.
    fn inputs(&self) -> usize {
        1
    }

    /// The output signals.
    fn outputs(&self) -> usize {
        1
    }

    /// `H(e^jw)` from the system's input to output `output`, at each frequency of
    /// `freqs` into `gain` (the same length). Control side: it may allocate. A
    /// system with no response to give fills `gain` with NaN.
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]);

    /// An upper bound on the spectral radius of the state map: the slowest
    /// decay per frame. The system is stable when it is below 1.
    fn spectral_radius(&self) -> f64;

    /// An upper bound on `||h||_1`, the l1 norm of the impulse response: the BIBO
    /// gain, so that `|x| <= 1` gives `|y| <= l1_norm` (the guarantee against
    /// overs). Infinity when no bound is known.
    fn l1_norm(&self) -> f64;

    /// An upper bound on `||H||_inf`, the peak of `|H(e^jw)|`: what a response
    /// display scales to. A system with nothing tighter returns
    /// [`l1_norm`](Self::l1_norm), which is never below it.
    fn hinf_norm(&self) -> f64 {
        self.l1_norm()
    }

    /// Whether no state can grow. By default the spectral radius is under 1; a
    /// system whose structure gives a sharper test overrides it (the reverb
    /// network accepts a frozen loop, which has radius 1, and refuses
    /// hostile figures the radius does not look at).
    fn is_stable(&self) -> bool {
        self.spectral_radius() < 1.0
    }

    /// `|H(1)|`, the gain at 0 Hz.
    fn dc_gain(&self) -> f64 {
        let mut gain = [Complex64::new(0.0, 0.0)];
        self.frequency_response(0, &[Hz(0.0)], &mut gain);
        gain[0].norm()
    }
}
