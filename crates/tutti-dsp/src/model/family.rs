//! [`Family`]: the pure function from a family's controls to a member, and
//! [`Grows`], the engineering that sizes what runs it.

use tutti_types::{BusTopology, SampleRate};

use super::{Cadence, Domain, Reach, Realized, Recurrence, System};

/// The buses a [`Family`] is made for: its input, and its outputs, which are one
/// topology repeated `bands` times, band-major (a crossover's), or once (every
/// other family). Its node declares exactly these buses, so the graph's dry path
/// folds the input onto band 0 by speaker (the graph refuses a dry path it would have
/// to guess), and the runner's outputs are the
/// bands in order: band `b` is the `b`-th run of [`output`](Self::output)'s width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channels {
    input: BusTopology,
    output: BusTopology,
    bands: u8,
    keyed: bool,
}

impl Channels {
    /// One output bus where the input is: a filter's.
    #[must_use]
    pub const fn same(input: BusTopology) -> Self {
        Self::new(input, input)
    }

    /// `input` in, `output` out: a reverb's stereo from a mono centre.
    #[must_use]
    pub const fn new(input: BusTopology, output: BusTopology) -> Self {
        Self {
            input,
            output,
            bands: 1,
            keyed: false,
        }
    }

    /// One output bus where the input is, and a side-chain **key** bus of the input's
    /// width after the main one: a compressor's or a gate's. The runner receives the
    /// key channels in [`Feed::key`](crate::Feed::key).
    #[must_use]
    pub const fn keyed(input: BusTopology) -> Self {
        Self {
            keyed: true,
            ..Self::new(input, input)
        }
    }

    /// The input's speakers once per band, `bands` of them (a crossover's), or
    /// `None` for no band.
    #[must_use]
    pub const fn bands(input: BusTopology, bands: u8) -> Option<Self> {
        if bands == 0 {
            return None;
        }
        Some(Self {
            input,
            output: input,
            bands,
            keyed: false,
        })
    }

    /// The input bus.
    #[must_use]
    pub const fn input(&self) -> BusTopology {
        self.input
    }

    /// The side-chain key bus: the input's width with no speaker named (a key is a
    /// level to read, not a placement), or `None` for a family with no key
    /// ([`keyed`](Self::keyed)).
    #[must_use]
    pub const fn key(&self) -> Option<BusTopology> {
        if self.keyed {
            BusTopology::unassigned(self.input.layout())
        } else {
            None
        }
    }

    /// The key's channels: the input's width when [`keyed`](Self::keyed), else 0.
    #[must_use]
    #[cfg(any(test, feature = "contract"))]
    pub(crate) const fn key_width(&self) -> usize {
        if self.keyed { self.input_width() } else { 0 }
    }

    /// One band of the output.
    #[must_use]
    pub const fn output(&self) -> BusTopology {
        self.output
    }

    /// How many bands the output repeats: 1 but for a split.
    #[must_use]
    pub const fn band_count(&self) -> usize {
        self.bands as usize
    }

    /// The input's channels.
    #[must_use]
    pub const fn input_width(&self) -> usize {
        self.input.layout().count() as usize
    }

    /// Every output channel, all bands.
    #[must_use]
    pub const fn output_width(&self) -> usize {
        self.output.layout().count() as usize * self.bands as usize
    }
}

/// The family of a processor: φ, the pure function from its controls to a member
/// and the [`Recurrence`] that runs the members.
///
/// This is φ, the family: controls to a member. The pure half
/// is [`member_into`](Self::member_into): no audio, no state and no
/// graph type. The member is the description the recurrence is held to
/// ([`Recurrence::coefficients_into`], κ, reads it, and the contract measures the
/// render against the member's queries); nothing here forces a runtime shape on the
/// recurrence. A family whose recurrence allocates from the member it runs says how
/// much in [`Grows`]; one whose state has one size says nothing.
pub trait Family: Clone + Send {
    /// Plain values in tutti units, one struct per family.
    type Controls: Copy + PartialEq + Send;

    /// The system a set of controls describes: a member of the family.
    type Member: System + Clone + Send;

    /// What runs this family's members: the recurrence [`Realized`] runs.
    type Recurrence: Recurrence<Member = Self::Member>;

    /// The grid on which `theta` is sampled, and so whether a render depends
    /// on how audio is split into blocks ([`Cadence::block_independent`]). The
    /// contract's block-independence cases run iff it is.
    const CADENCE: Cadence = Cadence::BLOCK;

    /// The widths of the signal this family takes and gives.
    fn channels(&self) -> Channels;

    /// An empty member at `rate`: what [`member_into`](Self::member_into) writes
    /// into. Its [`latency`](System::latency) is the one the node declares to the graph
    /// (with what the recurrence adds), so a member's latency must be the blank's whatever
    /// the controls: the contract's `latency` case holds every member to it.
    fn blank(&self, rate: SampleRate) -> Self::Member;

    /// φ: writes the member for `controls` at `rate` into `out`, replacing all of
    /// it.
    ///
    /// Pure and allocation-free: the same controls give the same member, with
    /// no clock, no state and no heap. It writes **in place** because a reverb
    /// `Network` is about 56 KB, too large to build as a value on an audio
    /// thread (whose stack may be 512 KB); a node writes into a member it owns.
    /// Total: a control outside its range is clamped and a non-finite one takes the
    /// family's default, so the member is always one the family can run.
    fn member_into(&self, controls: &Self::Controls, rate: SampleRate, out: &mut Self::Member);

    /// The member for `controls` at `rate`, as a value: a test, a UI and the
    /// control side call this; an audio thread calls
    /// [`member_into`](Self::member_into).
    fn member(&self, controls: &Self::Controls, rate: SampleRate) -> Self::Member {
        let mut member = self.blank(rate);
        self.member_into(controls, rate, &mut member);
        member
    }

    /// An unprepared recurrence for this family. Control side; allocates.
    fn recurrence(&self) -> Self::Recurrence;

    /// An unprepared runner of this family's recurrence, at rest on the family's blank
    /// member. Control side; allocates.
    fn realized(&self) -> Realized<Self::Recurrence> {
        Realized::new(self.recurrence(), &self.blank(SampleRate::DEFAULT))
    }
}

/// A [`Family`] whose recurrence allocates from the member it runs, so it must be
/// built for a worst case: `need`, `reach` and the
/// condition between them.
///
/// [`Capacity::need`](crate::Capacity::need) is `need : M → 𝒦`, what one member's state
/// needs. [`reach`](Self::reach) is `reach : 𝒫(C) × ℝ₊ → 𝒦`, what every member of a
/// [`Domain`] at a rate needs, monotone in the domain. They meet one condition, the whole
/// correctness of [`Capacity`](crate::Capacity), [`Domain`] and [`Reach`]: for every control
/// `c` in the domain, `need(φ(c)) ≤ reach(D)`
/// ([`Capacity::dominates`](crate::Capacity::dominates) is the `≤`). A family whose state
/// has one size (a filter) does not implement this: its recurrence's capacity is `()`.
pub trait Grows: Family {
    /// The family's own limit on a control ([`Reach::domain`]): the `D ⊆ C` that
    /// [`reach`](Self::reach) is taken over.
    type Domain: Domain;

    /// The worst case of [`Family::member_into`] over the controls'
    /// declared domain (and the limits in `reach`): what the recurrence allocates
    /// for and what the node declares as its tail ceiling.
    fn reach(&self, reach: &Reach<Self::Domain>) -> <Self::Recurrence as Recurrence>::Capacity;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A split's outputs are its input's speakers once per band, so its width
    /// is the input's times the bands; any other family has one output bus.
    #[test]
    fn a_split_repeats_its_input_once_per_band() {
        let stereo = BusTopology::STEREO;
        let split = Channels::bands(stereo, 3).expect("three bands");
        assert_eq!((split.output(), split.band_count()), (stereo, 3));
        assert_eq!((split.input_width(), split.output_width()), (2, 6));
        assert_eq!(Channels::bands(stereo, 0), None);
        let same = Channels::same(stereo);
        assert_eq!((same.band_count(), same.output_width()), (1, 2));
        assert_eq!((same.key(), same.key_width()), (None, 0));
    }

    /// A keyed family has the input's width again as its key, and one output bus: its
    /// output is not repeated and its input is the main bus alone.
    #[test]
    fn a_keyed_family_has_a_key_the_width_of_its_input() {
        let keyed = Channels::keyed(BusTopology::STEREO);
        assert_eq!(
            keyed.key(),
            BusTopology::unassigned(BusTopology::STEREO.layout())
        );
        assert!(keyed.key().is_some());
        assert_eq!(
            (keyed.input_width(), keyed.key_width(), keyed.output_width()),
            (2, 2, 2)
        );
        assert_eq!(keyed.band_count(), 1);
    }
}
