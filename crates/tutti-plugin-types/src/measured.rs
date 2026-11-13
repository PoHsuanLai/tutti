//! What a probe measured of a plugin, and what it made of the figures (doc 023
//! section 6): the record `PluginDescriptor::measured` holds, the verdict beside
//! the plugin's own claim, and the configuration a figure belongs to.
//!
//! A claim is the plugin's word, a measurement is one experiment at one
//! configuration. The verdict is shown to a person and rewrites nothing; the
//! only consumer of the figures is [`Processing::ceiling`](crate::Processing),
//! which a measurement can only raise.

use tutti_types::{Db, MaxBlock, SampleRate, Samples};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The probe's floor: output at or below it is silence (doc 023 Q3). Not exact
/// silence, because a plugin's own dither or denoise noise is not a tail, and
/// not a per-plugin knob, because a floor chosen per plugin hides a dishonest
/// tail. Recorded in every [`ProbeConfig`].
pub const PROBE_FLOOR: Db = Db(-100.0);

/// A hash of a plugin's state, as the FNV-1a of its bytes: which patch a
/// measurement is of. Two equal hashes are two equal states for the purpose of
/// "is this figure about the state I have"; it is no integrity check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StateHash(pub u64);

impl StateHash {
    /// The hash of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        Self(
            bytes
                .iter()
                .fold(OFFSET, |h, &b| (h ^ u64::from(b)).wrapping_mul(PRIME)),
        )
    }
}

/// The configuration a measurement was made at: the figures depend on it (a
/// block-sized lookahead, a rate-dependent tail), so every [`Measured`] says.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ProbeConfig {
    /// The rate the plugin was activated at.
    pub rate: SampleRate,
    /// The most frames a block carried.
    pub block: MaxBlock,
    /// The floor the figures were taken against ([`PROBE_FLOOR`]).
    pub floor: Db,
    /// The state the plugin was measured in: the one it held at load.
    pub state: StateHash,
}

/// Why a figure was not judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum NotChecked {
    /// The plugin claims an unbounded tail: no figure the probe can read is
    /// more or less than that.
    UnboundedClaim,
    /// The plugin did not say what its tail is, so there is nothing to hold the
    /// measurement to (and the ceiling never uses one for such a plugin).
    UnknownClaim,
    /// The output was still above the floor when the render budget ran out and
    /// the plugin is an instrument, whose note has no lower bound to report.
    NeverReachedFloor,
    /// Nothing rose above the floor in the onset window: the latency has no
    /// first frame to read.
    NoOnset,
    /// An instrument's first sound is its attack, not its latency.
    Instrument,
}

/// The tail the plugin claimed against the tail it was measured to keep.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum TailVerdict {
    /// The plugin rings no longer than it said.
    Honest,
    /// The plugin rings longer than it said. `measured` is a lower bound: the
    /// last frame above the floor, or the render budget when it was still
    /// sounding.
    TailLonger {
        /// What the plugin reported.
        reported: Samples,
        /// What was measured.
        measured: Samples,
    },
    /// Silent input did not bring the output below the floor: hiss, a noise
    /// generator, a free-running oscillator. The tail is not judged.
    NeverSettles {
        /// The level the output held with silence in.
        level: Db,
    },
    /// Not judged, and why.
    NotChecked(NotChecked),
}

/// The latency the plugin reported against the first frame above the floor.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum LatencyVerdict {
    /// The first sound is where the plugin said.
    Honest,
    /// The first sound is elsewhere. Recorded, never corrected: for any plugin
    /// but a plain delay the first frame is only an onset.
    LatencyDiffers {
        /// What the plugin reported.
        reported: Samples,
        /// The first frame above the floor.
        measured: Samples,
    },
    /// Silent input did not bring the output below the floor, so no frame is
    /// the first.
    NeverSettles {
        /// The level the output held with silence in.
        level: Db,
    },
    /// Not judged, and why.
    NotChecked(NotChecked),
}

/// What the probe made of the plugin's claims: two independent judgements, since
/// a plugin can be both [`TailLonger`](TailVerdict::TailLonger) and
/// [`LatencyDiffers`](LatencyVerdict::LatencyDiffers), and an unbounded tail
/// leaves the latency as checkable as ever.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Verdict {
    /// The tail judgement.
    pub tail: TailVerdict,
    /// The latency judgement.
    pub latency: LatencyVerdict,
}

impl Verdict {
    /// Whether both claims held: neither was contradicted. A claim that was not
    /// checked is not held.
    #[must_use]
    pub fn is_honest(&self) -> bool {
        self.tail == TailVerdict::Honest && self.latency == LatencyVerdict::Honest
    }
}

impl TailVerdict {
    /// How much a verdict says against the plugin, for merging the verdicts of
    /// two configurations into the one that is kept.
    fn rank(&self) -> u8 {
        match self {
            Self::NeverSettles { .. } => 3,
            Self::TailLonger { .. } => 2,
            Self::NotChecked(_) => 1,
            Self::Honest => 0,
        }
    }

    /// The worse of two: a contradiction beats a gap, a gap beats a pass, and of
    /// two longer tails the longer. Ties keep `self`.
    #[must_use]
    pub fn worse(self, other: Self) -> Self {
        match (self, other) {
            (Self::TailLonger { measured: a, .. }, Self::TailLonger { measured: b, .. }) => {
                if b > a { other } else { self }
            }
            _ if other.rank() > self.rank() => other,
            _ => self,
        }
    }
}

impl LatencyVerdict {
    fn rank(&self) -> u8 {
        match self {
            Self::NeverSettles { .. } => 3,
            Self::LatencyDiffers { .. } => 2,
            Self::NotChecked(_) => 1,
            Self::Honest => 0,
        }
    }

    /// The worse of two, as [`TailVerdict::worse`]. Ties keep `self`.
    #[must_use]
    pub fn worse(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// What a probe measured of a plugin (doc 023 section 6): the figures a plugin's
/// own claim is checked against, and the verdict of the check.
///
/// The probe measures at two configurations and keeps one record
/// ([`merge`](Self::merge)): the larger tail, with the configuration that
/// produced it, and the worse verdict of each judgement. The ceiling only ever
/// rises, so the larger figure is the safe one.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Measured {
    /// The last frame above the probe's floor after the impulse (or, for an
    /// instrument, after the note-off), less the plugin's reported latency: what
    /// the plugin was measured to keep sounding once its input stopped. `None`
    /// when the tail was not judged.
    pub tail: Option<Samples>,
    /// The first frame above the floor: the processing latency measured, as an
    /// onset. `None` when no frame rose above the floor, or for an instrument.
    pub latency: Option<Samples>,
    /// What the tail figure was measured at.
    pub config: ProbeConfig,
    /// What the figures say about the plugin's claims.
    pub verdict: Verdict,
}

impl Measured {
    /// One record from two measurements: the larger tail with its own
    /// configuration, the first latency, and the worse verdict of each
    /// judgement. `self` is the first configuration and wins ties.
    ///
    /// The latency is the first configuration's because a block-dependent
    /// latency has no larger or smaller "safe" figure (nothing reads it as a
    /// ceiling); a disagreement between the configurations is in the verdict.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        let longer = match (self.tail, other.tail) {
            (Some(a), Some(b)) => b > a,
            (None, Some(_)) => true,
            _ => false,
        };
        let (tail, config) = if longer {
            (other.tail, other.config)
        } else {
            (self.tail, self.config)
        };
        Self {
            tail,
            latency: self.latency.or(other.latency),
            config,
            verdict: Verdict {
                tail: self.verdict.tail.worse(other.verdict.tail),
                latency: self.verdict.latency.worse(other.verdict.latency),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(rate: f64, block: usize) -> ProbeConfig {
        ProbeConfig {
            rate: SampleRate(rate),
            block: MaxBlock::new(Samples(block)).unwrap(),
            floor: PROBE_FLOOR,
            state: StateHash::of(b"state"),
        }
    }

    fn honest(tail: Option<usize>, cfg: ProbeConfig) -> Measured {
        Measured {
            tail: tail.map(Samples),
            latency: Some(Samples(0)),
            config: cfg,
            verdict: Verdict {
                tail: TailVerdict::Honest,
                latency: LatencyVerdict::Honest,
            },
        }
    }

    /// FNV-1a's published test vectors: the hash is the algorithm it names, so
    /// a record written by one build is read by another.
    #[test]
    fn the_state_hash_is_fnv1a() {
        assert_eq!(StateHash::of(b"").0, 0xcbf2_9ce4_8422_2325);
        assert_eq!(StateHash::of(b"a").0, 0xaf63_dc4c_8601_ec8c);
        assert_eq!(StateHash::of(b"foobar").0, 0x8594_4171_f739_67e8);
    }

    /// **Merging keeps the larger tail with the configuration that produced
    /// it**, whichever order they come in; a tie keeps the first.
    #[test]
    fn a_merge_keeps_the_larger_tail_and_its_config() {
        let (a, b) = (config(48_000.0, 512), config(44_100.0, 64));
        let first = honest(Some(100), a);
        let second = honest(Some(250), b);
        assert_eq!(first.merge(second).tail, Some(Samples(250)));
        assert_eq!(first.merge(second).config, b);
        assert_eq!(second.merge(first).config, b);
        let tie = honest(Some(100), b);
        assert_eq!(first.merge(tie).config, a);
        assert_eq!(honest(None, a).merge(second).tail, Some(Samples(250)));
    }

    /// **A contradiction outranks a gap and a gap outranks a pass**, and of two
    /// longer tails the longer is kept: one configuration's honesty does not
    /// excuse the other's lie.
    #[test]
    fn a_merge_keeps_the_worse_verdict() {
        let longer = |m| TailVerdict::TailLonger {
            reported: Samples(10),
            measured: Samples(m),
        };
        assert_eq!(
            TailVerdict::Honest.worse(longer(20)),
            longer(20),
            "a pass loses"
        );
        assert_eq!(longer(20).worse(TailVerdict::Honest), longer(20));
        assert_eq!(longer(20).worse(longer(30)), longer(30));
        assert_eq!(longer(30).worse(longer(20)), longer(30));
        let gap = TailVerdict::NotChecked(NotChecked::NeverReachedFloor);
        assert_eq!(TailVerdict::Honest.worse(gap), gap, "a gap is not a pass");
        assert_eq!(gap.worse(longer(20)), longer(20));
        let hiss = TailVerdict::NeverSettles { level: Db(-60.0) };
        assert_eq!(longer(20).worse(hiss), hiss);
        let differs = LatencyVerdict::LatencyDiffers {
            reported: Samples(0),
            measured: Samples(64),
        };
        assert_eq!(LatencyVerdict::Honest.worse(differs), differs);
        assert_eq!(differs.worse(LatencyVerdict::Honest), differs);
    }

    /// Only a plugin whose both claims were checked and held is honest.
    #[test]
    fn a_gap_is_not_honest() {
        let mut v = Verdict {
            tail: TailVerdict::Honest,
            latency: LatencyVerdict::Honest,
        };
        assert!(v.is_honest());
        v.latency = LatencyVerdict::NotChecked(NotChecked::NoOnset);
        assert!(!v.is_honest());
    }
}
