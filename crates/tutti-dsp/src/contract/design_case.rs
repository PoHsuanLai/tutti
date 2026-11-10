//! The traits a family implements to be held to the contract: what to sweep and
//! what it promises.

use tutti_types::{Hz, SampleRate, Seconds};

use crate::{Band, Bounds, Ceiling, Design, Effect, Lti, MaxDecay, Network};

/// A hook of the contract that returns a list: a case that has nothing to run
/// when the list is empty passes for no reason, so a design says which hooks it
/// leaves empty and why ([`DesignCase::vacant`], the `vacuity` cases).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Hook {
    /// [`DesignCase::hostile`].
    Hostile,
    /// [`RealizationCase::probes`].
    Probes,
    /// [`RealizationCase::extremes`].
    Extremes,
    /// [`RealizationCase::steps`].
    Steps,
    /// [`RealizationCase::static_probes`].
    StaticProbes,
    /// [`LtiCase::anchors`].
    Anchors,
    /// [`ReverbCase::decay_pairs`].
    DecayPairs,
    /// [`ReverbCase::size_pairs`].
    SizePairs,
    /// [`ReverbCase::diffusion_pairs`].
    DiffusionPairs,
    /// [`ReverbCase::pre_delay_pairs`].
    PreDelayPairs,
    /// [`ReverbCase::freeze_pairs`].
    FreezePairs,
    /// [`ReverbCase::width_pairs`].
    WidthPairs,
    /// [`ReverbCase::wet_pairs`].
    WetPairs,
}

/// A design plus what the contract needs to know to test it, whatever the class.
pub trait DesignCase: Design {
    /// The rates to design at.
    fn rates(&self) -> Vec<SampleRate> {
        vec![
            SampleRate(44_100.0),
            SampleRate(48_000.0),
            SampleRate(96_000.0),
        ]
    }

    /// Controls in range: a grid with the corners, and seeded random draws.
    fn reachable(&self) -> Vec<Self::Controls>;

    /// Controls out of range or non-finite, which `design` must still turn into
    /// a model the family can run.
    fn hostile(&self) -> Vec<Self::Controls> {
        Vec::new()
    }

    /// The ceiling the design is bounded against at `rate`.
    fn ceiling(&self, rate: SampleRate) -> Ceiling<Self::Range> {
        Ceiling::at(rate).expect("a positive finite rate")
    }

    /// The frames of musical delay before the model's first sound: a pre-delay,
    /// the shortest path, a delay time. **Never latency**: the `arrival` case adds
    /// the realization's declared latency to this and measures the first output
    /// sample against the sum. Zero by default.
    fn musical_delay(&self, _model: &Self::Model) -> usize {
        0
    }

    /// The hooks this design leaves empty, each with why: a hook that returns
    /// nothing and is not listed fails the `vacuity` case, and one that is listed
    /// but returns something fails it too, so a case cannot stop running unseen.
    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[]
    }
}

/// A design whose realization is measured: the controls to render at.
pub trait RealizationCase:
    DesignCase + Design<Realization: Effect<<Self as Design>::Model> + Clone>
{
    /// The controls the measured cases run over: a few, spread across the
    /// controls' range, because each renders seconds of audio.
    fn probes(&self) -> Vec<Self::Controls>;

    /// The controls at the extreme: the longest decay and the largest size,
    /// where a loop is closest to unstable.
    fn extremes(&self) -> Vec<Self::Controls>;

    /// `(a, b)`: two controls a step apart, for the click-free, landed and
    /// stream-clock cases.
    fn steps(&self) -> Vec<(Self::Controls, Self::Controls)>;

    /// The controls whose first echoes the realization must place to the frame:
    /// `probes` without the modulation of the line lengths, which moves an echo
    /// off the model's centre figure. All of `probes` by default.
    fn static_probes(&self) -> Vec<Self::Controls> {
        self.probes()
    }
}

/// A frequency at which a family's response is known in closed form.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor<C> {
    /// The controls the model is designed for.
    pub controls: C,
    /// The rate.
    pub rate: SampleRate,
    /// The output the response is read at.
    pub output: usize,
    /// The frequency.
    pub freq: Hz,
    /// The magnitude `|H|` the family's figures give there.
    pub gain: f64,
    /// The relative error the model may have against it.
    pub tolerance: f64,
}

/// The class group of a family whose model is [`Lti`]: its anchors and how
/// close its realization must come to the model's response.
pub trait LtiCase: DesignCase + Design<Model: Lti> {
    /// Closed-form points of the response (a low-pass at its cutoff, a shelf's
    /// half gain, a reverb's gain at DC), from the family's figures and never from a
    /// render.
    fn anchors(&self) -> Vec<Anchor<Self::Controls>> {
        Vec::new()
    }

    /// Whether the controls ask for a marginal system (a frozen reverb: radius 1,
    /// a tail that never ends). The radius may reach 1 there and the measured
    /// cases skip it.
    fn marginal(&self, _controls: &Self::Controls) -> bool {
        false
    }

    /// The controls the `response` case compares the realization's impulse
    /// response to the model's at, or `None` for the
    /// [`static_probes`](RealizationCase::static_probes) (the default). A family
    /// whose realization is the model up to something the response sees and the
    /// decay does not (a sign on one line) names the controls that leave it out and
    /// says why.
    fn response_probes(&self) -> Option<Vec<Self::Controls>> {
        None
    }

    /// The frames of input that make `trips` trips of the system's longest loop:
    /// what a burst must last to fill it. A tenth of a second a trip by default.
    fn fill(&self, rate: SampleRate, trips: f32) -> usize {
        (0.1 * f64::from(trips) * rate.get()) as usize
    }

    /// The frequency, in cycles a frame, of the lowest band, where a decay is
    /// slowest: DC by default.
    fn low_tone(&self, _rate: SampleRate) -> f32 {
        0.0
    }

    /// The relative error (over all frequencies, in the `L2` sense) the
    /// realization's response may have against the model's.
    fn response_tolerance(&self) -> f64 {
        0.01
    }

    /// How far under the spectral radius the measured decay per frame may be, as
    /// a fraction of the radius' own rate: the measured rate must be at least
    /// `1 - tolerance` of it. 3% by default, the measurement's own noise: the
    /// reverbs' measured rates are 11 to 28% over the bound.
    fn decay_tolerance(&self) -> f64 {
        0.03
    }

    /// How many times longer the model's tail to a floor may be than the
    /// measured time to it, and still be called tight. 3 by default: the reverbs'
    /// tails are 1.3 to 2.1 times the measured time (a bound, not the figure).
    fn tail_slack(&self) -> f64 {
        3.0
    }

    /// How many times the measured peak of the response `hinf_norm` may exceed:
    /// what a display scaled to it would waste.
    fn hinf_slack(&self) -> f64 {
        50.0
    }
}

/// The class group of a reverb [`Network`]: the controls a reverb has and a
/// filter does not, and the figures they ask for.
pub trait ReverbCase:
    DesignCase + Design<Model = Network, Bounds = Bounds, Range = MaxDecay>
{
    /// The RT60 the controls ask for in `band`, from the formula and the controls
    /// alone (`None`: the controls do not ask).
    fn requested_rt60(
        &self,
        _controls: &Self::Controls,
        _rate: SampleRate,
        _band: Band,
    ) -> Option<Seconds> {
        None
    }

    /// Whether the controls ask for the lossless freeze.
    fn frozen(&self, _controls: &Self::Controls) -> bool {
        false
    }

    /// The pre-delay the controls ask for.
    fn pre_delay(&self, _controls: &Self::Controls) -> Seconds {
        Seconds::new(0.0)
    }

    /// `(without, with, by)`: the same controls without a pre-delay and with one
    /// of `by`.
    fn pre_delay_pairs(&self) -> Vec<(Self::Controls, Self::Controls, Seconds)> {
        Vec::new()
    }

    /// `(a, b)`: `b` asks for at least the decay of `a`, nothing else changed.
    fn decay_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// `(a, b)`: `b` is a larger room than `a`, nothing else changed.
    fn size_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// `(a, b)`: `b` diffuses more than `a`, nothing else changed.
    fn diffusion_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// Whether the design says it is wet-only (no direct path).
    fn wet_only(&self) -> bool {
        true
    }

    /// The relative error the realization's measured RT60 may have against the
    /// network's `rt60` (the realization declares it: design 022 section 8.2).
    fn tolerance(&self) -> f32 {
        0.1
    }

    /// The depths, in dB below the plateau, between which the decay is fitted.
    /// 40 to 65 by default: Freeverb flushes values under 1e-6 to zero, which a
    /// deeper fit would measure.
    fn fit_depth(&self) -> (f64, f64) {
        (40.0, 65.0)
    }

    /// The frequency, in cycles a frame, the burst of `band` is played at: the
    /// three points [`Band`] names for a network that is not given by bands (DC, a
    /// quarter of the rate, Nyquist) by default.
    fn tone(&self, band: Band, rate: SampleRate) -> f32 {
        let _ = rate;
        [0.0, 0.25, 0.5][band.index()]
    }

    /// `(thawed, frozen)`: the same controls without and with the freeze.
    fn freeze_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// `(natural, mono)`: the same controls at the stereo width the design calls
    /// natural and at none.
    fn width_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// `(a, b, ratio)`: `b` is the same sound as `a` at `ratio` times the level
    /// (`0` for an exact silence).
    fn wet_pairs(&self) -> Vec<(Self::Controls, Self::Controls, f32)> {
        Vec::new()
    }
}
