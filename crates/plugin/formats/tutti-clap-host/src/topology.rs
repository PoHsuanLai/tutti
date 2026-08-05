//! [`SurroundChannel`] ↔ [`ChannelTopology`] conversion.
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
//! # Why both directions are total
//!
//! Neither direction can fail. [`SurroundChannel`] carries an `Unknown(u8)` arm
//! and [`Speaker`] an `Unknown(u16)`, so a position either vocabulary cannot
//! name still occupies its channel's slot. That matters more here than
//! anywhere: the map is positional, so a dropped element renumbers every
//! channel after it — the bug this crate shipped when `decode_surround_channel_map`
//! used `filter_map`.
//!
//! The two `Unknown` payloads are **not interchangeable**. A
//! `SurroundChannel::Unknown(n)` holds a raw CLAP position, and a
//! `Speaker::Unknown(n)` holds whatever the *source* format meant. Round-tripping
//! a CLAP map preserves the value because this module is both ends of it; a
//! `Speaker::Unknown` that arrived from VST3 or AU is a different namespace and
//! must not be handed to a CLAP plugin as though it were a CLAP position. See
//! [`channel_map_of`].

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
        // The raw CLAP position, preserved. `u16` widens from CLAP's `u8`
        // without loss; the narrowing back is checked in `channel_of`.
        SurroundChannel::Unknown(raw) => Speaker::Unknown(u16::from(raw)),
    }
}

/// The CLAP position a shared [`Speaker`] means, or `None` if CLAP has no
/// position for it.
///
/// `None` for a `Speaker::Unknown` whose payload is not a valid CLAP position —
/// including any value above `u8::MAX`, and any that came from another format's
/// namespace. A VST3 bit index and a CLAP position are different numbering
/// spaces, and coercing one into the other would name a real-but-wrong speaker.
fn channel_of(speaker: Speaker) -> Option<SurroundChannel> {
    Some(match speaker {
        Speaker::FrontLeft => SurroundChannel::FrontLeft,
        Speaker::FrontRight => SurroundChannel::FrontRight,
        Speaker::FrontCenter => SurroundChannel::FrontCenter,
        Speaker::LowFrequency => SurroundChannel::LowFrequency,
        Speaker::BackLeft => SurroundChannel::BackLeft,
        Speaker::BackRight => SurroundChannel::BackRight,
        Speaker::FrontLeftOfCenter => SurroundChannel::FrontLeftCenter,
        Speaker::FrontRightOfCenter => SurroundChannel::FrontRightCenter,
        Speaker::BackCenter => SurroundChannel::BackCenter,
        Speaker::SideLeft => SurroundChannel::SideLeft,
        Speaker::SideRight => SurroundChannel::SideRight,
        Speaker::TopCenter => SurroundChannel::TopCenter,
        Speaker::TopFrontLeft => SurroundChannel::TopFrontLeft,
        Speaker::TopFrontCenter => SurroundChannel::TopFrontCenter,
        Speaker::TopFrontRight => SurroundChannel::TopFrontRight,
        Speaker::TopBackLeft => SurroundChannel::TopBackLeft,
        Speaker::TopBackCenter => SurroundChannel::TopBackCenter,
        Speaker::TopBackRight => SurroundChannel::TopBackRight,
        Speaker::TopSideLeft => SurroundChannel::TopSideLeft,
        Speaker::TopSideRight => SurroundChannel::TopSideRight,
        // Only a payload that fits CLAP's `u8` position space can be one.
        // `from_position` then decides whether it is a position CLAP names or
        // an `Unknown` of its own — either way the slot is kept.
        Speaker::Unknown(raw) => SurroundChannel::from_position(u8::try_from(raw).ok()?),
    })
}

/// Convert a CLAP channel map into a [`ChannelTopology`].
///
/// Total, and length-preserving: the topology has exactly one position per
/// channel the plugin reported.
pub fn topology_of(map: &[SurroundChannel]) -> ChannelTopology {
    ChannelTopology::new(map.iter().copied().map(speaker_of))
}

/// Convert a [`ChannelTopology`] into a CLAP channel map, or `None` if any
/// position has no CLAP spelling.
///
/// All-or-nothing rather than per-channel: a map with one element silently
/// replaced would route that channel to the wrong speaker, and a *shorter* map
/// would renumber the rest. Refusing hands the caller a decision it can act on.
pub fn channel_map_of(topology: &ChannelTopology) -> Option<Vec<SurroundChannel>> {
    topology
        .positions()
        .iter()
        .copied()
        .map(channel_of)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::ChannelLayout;

    /// Every position CLAP names survives a round trip through the shared
    /// vocabulary.
    ///
    /// Two hand-written match arms, so nothing but a round trip stops one from
    /// drifting: a transposed pair still compiles and still converts
    /// "successfully", just to the wrong speaker.
    #[test]
    fn every_clap_position_round_trips() {
        for raw in 0u8..=19 {
            let channel = SurroundChannel::from_position(raw);
            let speaker = speaker_of(channel);
            assert_ne!(
                speaker,
                Speaker::Unknown(u16::from(raw)),
                "position {raw} is named by CLAP and must map to a named speaker"
            );
            assert_eq!(
                channel_of(speaker),
                Some(channel),
                "position {raw} did not survive the round trip"
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
        let map = vec![
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
        assert_eq!(channel_map_of(&topology), Some(map));
    }

    /// A speaker outside CLAP's position space refuses the whole map.
    ///
    /// `Speaker::Unknown` payloads are the *source* format's namespace — a VST3
    /// bit index, say — so a value that cannot be a CLAP position must not be
    /// coerced into one. 300 does not fit `u8` at all.
    #[test]
    fn a_speaker_outside_claps_position_space_refuses_the_map() {
        let topology = ChannelTopology::new([Speaker::FrontLeft, Speaker::Unknown(300)]);
        assert_eq!(
            channel_map_of(&topology),
            None,
            "a payload that cannot be a CLAP position must not be coerced into one"
        );
    }

    /// Refusal is all-or-nothing, not a shortened map.
    ///
    /// The positive half of the test above: a partial map would be worse than
    /// no map, since the surviving channels would be renumbered.
    #[test]
    fn a_refused_map_yields_nothing_rather_than_a_shorter_one() {
        let topology = ChannelTopology::new([
            Speaker::FrontLeft,
            Speaker::Unknown(65_000),
            Speaker::FrontRight,
        ]);
        assert!(channel_map_of(&topology).is_none());

        // And the same topology without the offending position converts whole.
        let ok = ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight]);
        assert_eq!(channel_map_of(&ok).map(|m| m.len()), Some(2));
    }

    /// An empty map is an empty topology, not a failure.
    #[test]
    fn an_empty_map_converts_to_an_empty_topology() {
        let topology = topology_of(&[]);
        assert_eq!(topology.layout(), ChannelLayout::EMPTY);
        assert_eq!(channel_map_of(&topology), Some(Vec::new()));
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
