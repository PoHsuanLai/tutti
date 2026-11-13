//! [`Lti`]: a linear time-invariant system.

use tutti_types::Hz;

use super::System;
use crate::Complex64;

/// A linear time-invariant system (order 0 is a
/// gain matrix). The queries are the system's invariants, answered from its
/// structure and never from its realization.
///
/// A response `H` exists if and only if the system is linear and commutes with the
/// unit delay. A nonlinear member (the ladder's `tanh`, a compressor) has none; one that
/// has a small-signal model implements `Lti` for that model, as
/// [`LadderModel`](crate::LadderModel) does, and the analysis applies to it.
///
/// The queries describe a member at fixed controls, not the system while its controls
/// move, and two limits follow. The frozen response
/// overstates decay near Nyquist under a delay modulated with linear interpolation: a
/// fractional delay of one half cancels a tone there and the modulation visits every
/// fraction, so the contract holds the queries on static probes only. And a frozen
/// [`spectral_radius`](Self::spectral_radius) below 1 does not imply that the system is
/// stable while its controls move.
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
}
