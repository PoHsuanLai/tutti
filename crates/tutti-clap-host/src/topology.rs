//! [`SurroundChannel`] → [`ChannelTopology`] decoding.
//!
//! # The one format whose model already matches
//!
//! CLAP's `clap.surround` gives the host `channel_map[i]` — the speaker fed by
//! channel `i` (`surround.h:66`). That is exactly what a [`ChannelTopology`]
//! is, so this conversion is a rename per element rather than a table of named
//! layouts. Nothing here has to decide an order, because CLAP already stated
//! one.
//!
//! Contrast the other two: VST3 hands over a *set* whose order must be derived
//! from bit positions, and AU hands over a *tag* whose order comes from a table
//! of Apple's header comments. CLAP is the format the shared type was shaped
//! after.
//!
//! # Why decoding is total
//!
//! It cannot fail. [`SurroundChannel`] carries an `Unknown(u8)` arm and
//! [`Speaker`] an `Unknown(u16)`, so a position the shared vocabulary cannot
//! name still occupies its channel's slot. That matters more here than
//! anywhere: the map is positional, so a dropped element renumbers every
//! channel after it, which is why `decode_surround_channel_map` never filters.
//! The host never tells a plugin a layout, so there is no inverse.

use tutti_types::{ChannelTopology, Speaker};

use crate::types::SurroundChannel;

/// The shared [`Speaker`] a CLAP position means.
///
/// One-to-one for every position CLAP names. `SurroundChannel::Unknown` becomes
/// `Speaker::Unknown` carrying the same raw value, so the channel keeps its
/// slot and the position survives a round trip.
fn speaker_of(channel: SurroundChannel) -> Speaker {
    match channel {
        SurroundChannel::FrontLeft => Speaker::FrontLeft,
        SurroundChannel::FrontRight => Speaker::FrontRight,
        SurroundChannel::FrontCenter => Speaker::FrontCenter,
        SurroundChannel::LowFrequency => Speaker::LowFrequency,
        SurroundChannel::BackLeft => Speaker::BackLeft,
        SurroundChannel::BackRight => Speaker::BackRight,
        SurroundChannel::FrontLeftCenter => Speaker::FrontLeftOfCenter,
        SurroundChannel::FrontRightCenter => Speaker::FrontRightOfCenter,
        SurroundChannel::BackCenter => Speaker::BackCenter,
        SurroundChannel::SideLeft => Speaker::SideLeft,
        SurroundChannel::SideRight => Speaker::SideRight,
        SurroundChannel::TopCenter => Speaker::TopCenter,
        SurroundChannel::TopFrontLeft => Speaker::TopFrontLeft,
        SurroundChannel::TopFrontCenter => Speaker::TopFrontCenter,
        SurroundChannel::TopFrontRight => Speaker::TopFrontRight,
        SurroundChannel::TopBackLeft => Speaker::TopBackLeft,
        SurroundChannel::TopBackCenter => Speaker::TopBackCenter,
        SurroundChannel::TopBackRight => Speaker::TopBackRight,
        SurroundChannel::TopSideLeft => Speaker::TopSideLeft,
        SurroundChannel::TopSideRight => Speaker::TopSideRight,
        // The raw CLAP position, preserved: `u16` widens from CLAP's `u8` without loss.
        SurroundChannel::Unknown(raw) => Speaker::Unknown(u16::from(raw)),
    }
}

/// Converts a CLAP channel map into a [`ChannelTopology`].
///
/// Total, and length-preserving: the topology has exactly one position per
/// channel the plugin reported.
pub fn topology_of(map: &[SurroundChannel]) -> ChannelTopology {
    ChannelTopology::new(map.iter().copied().map(speaker_of))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::ChannelLayout;

    /// Every position CLAP names decodes to its speaker, per `clap/ext/surround.h`.
    ///
    /// Two hand-written arms could transpose a pair and still compile, so the
    /// positions are written as literals from the header (`CLAP_SURROUND_FL = 0`
    /// ... `CLAP_SURROUND_TSR = 19`), not derived from the conversion.
    #[test]
    fn every_clap_position_decodes_to_its_speaker() {
        let spec: [(u8, Speaker); 20] = [
            (0, Speaker::FrontLeft),
            (1, Speaker::FrontRight),
            (2, Speaker::FrontCenter),
            (3, Speaker::LowFrequency),
            (4, Speaker::BackLeft),
            (5, Speaker::BackRight),
            (6, Speaker::FrontLeftOfCenter),
            (7, Speaker::FrontRightOfCenter),
            (8, Speaker::BackCenter),
            (9, Speaker::SideLeft),
            (10, Speaker::SideRight),
            (11, Speaker::TopCenter),
            (12, Speaker::TopFrontLeft),
            (13, Speaker::TopFrontCenter),
            (14, Speaker::TopFrontRight),
            (15, Speaker::TopBackLeft),
            (16, Speaker::TopBackCenter),
            (17, Speaker::TopBackRight),
            (18, Speaker::TopSideLeft),
            (19, Speaker::TopSideRight),
        ];
        for (raw, speaker) in spec {
            assert_eq!(
                speaker_of(SurroundChannel::from_position(raw)),
                speaker,
                "CLAP position {raw}"
            );
        }
    }

    /// A 5.1 map converts to the engine's own channel order.
    ///
    /// CLAP's own 5.1 ordering — `FL FR FC LFE SL SR` — is the SMPTE order, so
    /// this is asserted against `ChannelTopology::smpte` rather than a literal:
    /// the claim is that the two agree, not that a list was copied correctly.
    #[test]
    fn a_51_channel_map_matches_the_engines_smpte_order() {
        let map: Vec<SurroundChannel> = [0u8, 1, 2, 3, 9, 10]
            .iter()
            .map(|&p| SurroundChannel::from_position(p))
            .collect();
        assert_eq!(
            topology_of(&map),
            ChannelTopology::smpte(ChannelLayout::from(6u16)).expect("6 has an order"),
            "CLAP FL FR FC LFE SL SR is the engine's 5.1 order"
        );
    }

    /// An unnamed position keeps its channel's slot and its raw value.
    ///
    /// Asserted on the *following* channel's index, because that is what a
    /// dropped element corrupts — the failure this crate shipped when the
    /// decoder used `filter_map`.
    #[test]
    fn an_unknown_position_keeps_its_slot_and_its_value() {
        let map = [
            SurroundChannel::FrontLeft,
            SurroundChannel::Unknown(200),
            SurroundChannel::FrontRight,
        ];
        let topology = topology_of(&map);

        assert_eq!(topology.layout(), ChannelLayout::from(3u16));
        assert_eq!(topology.positions()[1], Speaker::Unknown(200));
        assert_eq!(
            topology.index_of(Speaker::FrontRight),
            Some(2),
            "the channel after an unknown one must keep its own index"
        );
    }

    /// An empty map is an empty topology, not a failure.
    #[test]
    fn an_empty_map_converts_to_an_empty_topology() {
        let topology = topology_of(&[]);
        assert_eq!(topology.layout(), ChannelLayout::EMPTY);
    }

    /// The conversion preserves length for every map it accepts.
    ///
    /// The property everything above rests on: the topology is indexed by
    /// channel, so any length change is a routing error regardless of which
    /// speakers are named.
    #[test]
    fn conversion_preserves_the_channel_count() {
        for len in 0..=20u8 {
            let map: Vec<SurroundChannel> = (0..len).map(SurroundChannel::from_position).collect();
            assert_eq!(
                topology_of(&map).layout().count(),
                u16::from(len),
                "a {len}-channel map must produce a {len}-channel topology"
            );
        }
    }
}
