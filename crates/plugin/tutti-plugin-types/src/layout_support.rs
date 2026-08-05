//! [`LayoutSupport`] — what a caller can actually do about a plugin's channel
//! layout.
//!
//! The counterpart to `PresetSupport`, and deliberately a *smaller* answer.
//! Presets have two independent capabilities (list, load) because formats
//! genuinely differ on each. Layout has two as well — *report* what the plugin
//! is running, and *propose* a different one — but only the first is built, so
//! this enum describes reporting and says nothing about proposing.
//!
//! Naming the missing half would be the mistake this type exists to avoid. A
//! `Negotiable` variant no caller could act on is exactly the write-only
//! surface D-9 and D-11 both diagnosed: a capability reported but never
//! reachable. It arrives when a proposal path does, and not before.

use crate::LoadedPlugin;

/// What is known about a plugin's channel placement.
///
/// Derived from [`LoadedPlugin`] rather than from a `Features` bit, because
/// there is no bit to read: topology is per-bus data, and "some buses answered"
/// is a real state that one flag cannot express.
///
/// A UI switches on this to decide what to *show*. None of the variants implies
/// a layout can be changed — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayoutSupport {
    /// Every bus in both directions reported a placement, and each agrees with
    /// the channel count beside it.
    ///
    /// The only state in which a caller may route by speaker: it can ask which
    /// channel carries the LFE and get an answer for every bus.
    Full,
    /// Some buses reported a placement and some did not.
    ///
    /// Common rather than exotic — an AU whose main bus publishes a layout tag
    /// while its sidechain does not, or a VST3 plugin using one speaker this
    /// vocabulary cannot name. A caller may use the buses that answered and
    /// must fall back to channel order for the rest; it must not assume the
    /// gaps are stereo.
    Partial,
    /// No bus reported a placement.
    ///
    /// Either the format cannot express it (VST2), or nothing was asked. A UI
    /// shows channel numbers rather than speaker names.
    None,
}

impl LayoutSupport {
    /// Derive from the per-bus topology a load reported.
    ///
    /// Three states from two questions — "did every bus answer?" and "did any?"
    /// — which is why this is an enum rather than a bool. Collapsing `Partial`
    /// into `None` would discard placements the plugin did report; collapsing
    /// it into `Full` would have a caller ask for a speaker on a bus that never
    /// named one.
    ///
    /// Completeness is [`LoadedPlugin::topology_is_complete`], which requires
    /// more than presence: each topology's width must match the count beside
    /// it, since the count sizes the buffers and two descriptions of one bus
    /// cannot be adjudicated.
    pub fn of(loaded: &LoadedPlugin) -> Self {
        if loaded.topology_is_complete() {
            return Self::Full;
        }
        let any = loaded
            .input_topology
            .iter()
            .chain(loaded.output_topology.iter())
            .any(Option::is_some);
        if any {
            Self::Partial
        } else {
            Self::None
        }
    }

    /// Whether *every* bus can be routed by speaker.
    ///
    /// The predicate a caller checks before doing anything that needs a
    /// placement for each channel — a downmix that must find the LFE, say. False
    /// for [`Partial`](Self::Partial): a per-bus caller reads the buses
    /// individually instead.
    pub fn is_complete(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether any bus reported a placement at all.
    ///
    /// What a UI checks before offering speaker names anywhere.
    pub fn has_any(self) -> bool {
        matches!(self, Self::Full | Self::Partial)
    }
}

#[cfg(test)]
mod tests {
    use smallvec::SmallVec;

    use super::*;
    use crate::{ChannelLayout, ChannelTopology};

    fn stereo() -> ChannelTopology {
        ChannelTopology::smpte(ChannelLayout::STEREO).expect("stereo has an order")
    }

    fn one_bus_each(
        input: Option<ChannelTopology>,
        output: Option<ChannelTopology>,
    ) -> LoadedPlugin {
        LoadedPlugin {
            inputs: SmallVec::from_slice(&[ChannelLayout::STEREO]),
            outputs: SmallVec::from_slice(&[ChannelLayout::STEREO]),
            input_topology: SmallVec::from_vec(vec![input]),
            output_topology: SmallVec::from_vec(vec![output]),
            ..Default::default()
        }
    }

    /// Every bus answering, and agreeing with its count, is `Full`.
    #[test]
    fn every_bus_answering_is_full() {
        let loaded = one_bus_each(Some(stereo()), Some(stereo()));
        assert_eq!(LayoutSupport::of(&loaded), LayoutSupport::Full);
        assert!(LayoutSupport::of(&loaded).is_complete());
        assert!(LayoutSupport::of(&loaded).has_any());
    }

    /// One bus answering and one not is `Partial`, not `Full` and not `None`.
    ///
    /// The state both collapses get wrong: reported as `Full`, a caller asks
    /// the silent bus for a speaker it never named; as `None`, the placement
    /// the plugin *did* report is thrown away.
    #[test]
    fn a_mix_of_answered_and_silent_buses_is_partial() {
        let loaded = one_bus_each(Some(stereo()), None);
        let support = LayoutSupport::of(&loaded);
        assert_eq!(support, LayoutSupport::Partial);
        assert!(
            !support.is_complete(),
            "a partial answer must not license per-channel routing"
        );
        assert!(support.has_any(), "the bus that did answer is still usable");
    }

    /// No bus answering is `None`.
    #[test]
    fn no_bus_answering_is_none() {
        let loaded = one_bus_each(None, None);
        assert_eq!(LayoutSupport::of(&loaded), LayoutSupport::None);
        assert!(!LayoutSupport::of(&loaded).has_any());
    }

    /// A format that reports nothing at all is `None`, not `Partial`.
    ///
    /// Empty lists rather than lists of `None` — what VST2 produces, and what
    /// any loader built before this field produces. Distinguished from the test
    /// above because the two reach the same answer by different routes, and a
    /// length-based check would get one of them wrong.
    #[test]
    fn a_format_that_reports_no_topology_at_all_is_none() {
        let loaded = LoadedPlugin {
            inputs: SmallVec::from_slice(&[ChannelLayout::STEREO]),
            outputs: SmallVec::from_slice(&[ChannelLayout::STEREO]),
            ..Default::default()
        };
        assert_eq!(LayoutSupport::of(&loaded), LayoutSupport::None);
    }

    /// A topology disagreeing with its bus width is not `Full`.
    ///
    /// It answered, so it is not `None` — but the answer cannot be acted on,
    /// because the count sizes the buffer and the topology describes a
    /// different number of channels. `Partial` is the honest reading: something
    /// was said, and not enough of it is usable.
    #[test]
    fn a_topology_disagreeing_with_its_width_is_not_full() {
        let mono = ChannelTopology::smpte(ChannelLayout::MONO).expect("mono has an order");
        let loaded = one_bus_each(Some(mono), Some(stereo()));
        assert_eq!(LayoutSupport::of(&loaded), LayoutSupport::Partial);
        assert!(!LayoutSupport::of(&loaded).is_complete());
    }

    /// A plugin with no buses at all is `Full`, vacuously.
    ///
    /// Recorded rather than defended: `topology_is_complete` is a "for every
    /// bus" claim, and it holds over an empty set. Nothing can go wrong — there
    /// is no channel to misroute — but the answer surprises, so it is pinned so
    /// a future change to that predicate has to consider it.
    #[test]
    fn a_plugin_with_no_buses_is_vacuously_full() {
        assert_eq!(
            LayoutSupport::of(&LoadedPlugin::default()),
            LayoutSupport::Full
        );
    }
}
