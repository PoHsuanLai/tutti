//! Speaker arrangement negotiation: [`Vst2Loaded::negotiate_arrangement`].
//!
//! `effSetSpeakerArrangement` proposes an input and an output arrangement to a suspended
//! plugin, and `effGetSpeakerArrangement` reads what it holds. The plugin may keep its own
//! layout, so the host reads back after every proposal and reports whether the plugin
//! kept the proposal. The two functions at the bottom of this file are the only mapping
//! between [`ChannelTopology`] and the ABI's [`SpeakerArrangement`].

use thiserror::Error;
use tutti_types::{ChannelTopology, Speaker};
use tutti_vst2_ffi::speaker::{
    MAX_ARRANGEMENT_CHANNELS, Speaker as Vst2Speaker, SpeakerArrangement,
};

use crate::loaded::Vst2Loaded;
use tutti_plugin_types::MainThread;

/// A proposal that was not sent.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArrangementError {
    /// The layout has a speaker VST 2 has no name for, so it cannot be proposed.
    #[error("{0:?} is a speaker VST 2 has no name for")]
    UnmappedSpeaker(Speaker),
    /// The layout is wider than a VST 2 arrangement holds.
    #[error("{0} channels are more than a VST 2 arrangement holds")]
    TooWide(usize),
    /// The plugin is resumed; arrangements are set while it is suspended.
    #[error("a speaker arrangement is set while the plugin is suspended")]
    Resumed,
}

/// How a plugin answered a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiation {
    /// The plugin took both arrangements and reads back exactly them.
    Accepted,
    /// The plugin returned 0, or took the call and reads back something else. What it
    /// holds is on the buses of [`Control::wiring`](tutti_plugin_types::Control::wiring).
    Refused,
}

/// The arrangements a plugin held after a proposal, each `None` when the plugin gave no
/// arrangement that this vocabulary can name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Kept {
    pub(crate) input: Option<ChannelTopology>,
    pub(crate) output: Option<ChannelTopology>,
}

impl Vst2Loaded {
    /// Proposes `input` and `output` to the plugin with `effSetSpeakerArrangement`, then
    /// reads back what it holds with `effGetSpeakerArrangement`.
    ///
    /// The call goes before [`activate`](tutti_plugin_types::Loaded::activate), while the
    /// plugin is suspended. A plugin that refuses keeps its own layout: the buses of
    /// [`wiring`](tutti_plugin_types::Control::wiring) carry the arrangement it kept, with
    /// no placement for a bus whose arrangement cannot be named or does not match the
    /// plugin's pin count.
    ///
    /// # Errors
    ///
    /// [`ArrangementError::UnmappedSpeaker`] or [`ArrangementError::TooWide`] when a layout
    /// cannot be written in the ABI, and [`ArrangementError::Resumed`] when the plugin is
    /// resumed. Nothing is sent in any of them.
    pub fn negotiate_arrangement(
        &mut self,
        main: &MainThread,
        input: &ChannelTopology,
        output: &ChannelTopology,
    ) -> Result<Negotiation, ArrangementError> {
        let wanted_input = arrangement_of(input)?;
        let wanted_output = arrangement_of(output)?;
        if self.instance.is_resumed() {
            return Err(ArrangementError::Resumed);
        }

        let returned_one =
            self.instance
                .set_speaker_arrangement(main, &wanted_input, &wanted_output);
        let held = self.instance.speaker_arrangement(main).ok();
        let kept = match &held {
            Some((i, o)) => Kept {
                input: topology_of(i),
                output: topology_of(o),
            },
            // A plugin that took the proposal and cannot read it back holds the proposal.
            None if returned_one => Kept {
                input: Some(input.clone()),
                output: Some(output.clone()),
            },
            None => Kept::default(),
        };
        let accepted = returned_one
            && match &held {
                Some((i, o)) => *i == wanted_input && *o == wanted_output,
                None => true,
            };
        self.kept = Some(kept);
        Ok(if accepted {
            Negotiation::Accepted
        } else {
            Negotiation::Refused
        })
    }
}

/// The arrangement that places each channel of `topology` at the speaker VST 2 names it.
///
/// # Errors
///
/// [`ArrangementError::UnmappedSpeaker`] for a speaker with no VST 2 name
/// ([`Speaker::Unknown`] and the top-side pair), and [`ArrangementError::TooWide`] past
/// [`MAX_ARRANGEMENT_CHANNELS`].
pub(crate) fn arrangement_of(
    topology: &ChannelTopology,
) -> Result<SpeakerArrangement, ArrangementError> {
    let positions = topology.positions();
    if positions.len() > MAX_ARRANGEMENT_CHANNELS {
        return Err(ArrangementError::TooWide(positions.len()));
    }
    // The engine's one-channel bus is the centre, and VST 2's is `kSpeakerM`.
    if positions == [Speaker::FrontCenter] {
        return Ok(SpeakerArrangement::new(vec![Vst2Speaker::Mono]));
    }
    positions
        .iter()
        .map(|&speaker| match speaker {
            Speaker::FrontLeft => Ok(Vst2Speaker::Left),
            Speaker::FrontRight => Ok(Vst2Speaker::Right),
            Speaker::FrontCenter => Ok(Vst2Speaker::Center),
            Speaker::LowFrequency => Ok(Vst2Speaker::Lfe),
            // VST 2's `Ls`/`Rs` are the surround pair of 5.1, which the engine's
            // order calls side, and its `Sl`/`Sr` the pair beyond it, the engine's back.
            Speaker::SideLeft => Ok(Vst2Speaker::LeftSurround),
            Speaker::SideRight => Ok(Vst2Speaker::RightSurround),
            Speaker::BackLeft => Ok(Vst2Speaker::SideLeft),
            Speaker::BackRight => Ok(Vst2Speaker::SideRight),
            Speaker::FrontLeftOfCenter => Ok(Vst2Speaker::LeftOfCenter),
            Speaker::FrontRightOfCenter => Ok(Vst2Speaker::RightOfCenter),
            Speaker::BackCenter => Ok(Vst2Speaker::Surround),
            Speaker::TopCenter => Ok(Vst2Speaker::TopMiddle),
            Speaker::TopFrontLeft => Ok(Vst2Speaker::TopFrontLeft),
            Speaker::TopFrontCenter => Ok(Vst2Speaker::TopFrontCenter),
            Speaker::TopFrontRight => Ok(Vst2Speaker::TopFrontRight),
            Speaker::TopBackLeft => Ok(Vst2Speaker::TopRearLeft),
            Speaker::TopBackCenter => Ok(Vst2Speaker::TopRearCenter),
            Speaker::TopBackRight => Ok(Vst2Speaker::TopRearRight),
            Speaker::Unassigned => Ok(Vst2Speaker::Undefined),
            Speaker::TopSideLeft | Speaker::TopSideRight | Speaker::Unknown(_) => {
                Err(ArrangementError::UnmappedSpeaker(speaker))
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(SpeakerArrangement::new)
}

/// The topology an arrangement spells, or `None` when a speaker in it has no name in the
/// shared vocabulary (`kSpeakerLfe2`), so the bus cannot be routed by speaker.
pub(crate) fn topology_of(arrangement: &SpeakerArrangement) -> Option<ChannelTopology> {
    if arrangement.speakers() == [Vst2Speaker::Mono] {
        return Some(ChannelTopology::new([Speaker::FrontCenter]));
    }
    arrangement
        .speakers()
        .iter()
        .map(|&speaker| match speaker {
            Vst2Speaker::Mono => Some(Speaker::FrontCenter),
            Vst2Speaker::Left => Some(Speaker::FrontLeft),
            Vst2Speaker::Right => Some(Speaker::FrontRight),
            Vst2Speaker::Center => Some(Speaker::FrontCenter),
            Vst2Speaker::Lfe => Some(Speaker::LowFrequency),
            Vst2Speaker::LeftSurround => Some(Speaker::SideLeft),
            Vst2Speaker::RightSurround => Some(Speaker::SideRight),
            Vst2Speaker::SideLeft => Some(Speaker::BackLeft),
            Vst2Speaker::SideRight => Some(Speaker::BackRight),
            Vst2Speaker::LeftOfCenter => Some(Speaker::FrontLeftOfCenter),
            Vst2Speaker::RightOfCenter => Some(Speaker::FrontRightOfCenter),
            Vst2Speaker::Surround => Some(Speaker::BackCenter),
            Vst2Speaker::TopMiddle => Some(Speaker::TopCenter),
            Vst2Speaker::TopFrontLeft => Some(Speaker::TopFrontLeft),
            Vst2Speaker::TopFrontCenter => Some(Speaker::TopFrontCenter),
            Vst2Speaker::TopFrontRight => Some(Speaker::TopFrontRight),
            Vst2Speaker::TopRearLeft => Some(Speaker::TopBackLeft),
            Vst2Speaker::TopRearCenter => Some(Speaker::TopBackCenter),
            Vst2Speaker::TopRearRight => Some(Speaker::TopBackRight),
            Vst2Speaker::Undefined => Some(Speaker::Unassigned),
            Vst2Speaker::Lfe2 => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(ChannelTopology::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::ChannelLayout;
    use tutti_vst2_ffi::api::SpeakerArrangementType as Kind;

    /// The engine's own orders for 1, 2, 6, 8 and 12 channels map to the SDK's named
    /// arrangements: `kSpeakerArrMono`, `Stereo`, `51`, `Music71`, and a user-defined one for
    /// 7.1.4, which no `kSpeakerArr*` names.
    #[test]
    fn the_engines_orders_are_the_sdks_named_arrangements() {
        let kind = |width: u16| {
            let topology = ChannelTopology::smpte(ChannelLayout::from(width)).expect("an order");
            arrangement_of(&topology).expect("mapped").kind()
        };
        assert_eq!(kind(1), Kind::Mono);
        assert_eq!(kind(2), Kind::Stereo);
        assert_eq!(kind(6), Kind::Surround51);
        assert_eq!(kind(8), Kind::Music71);
        assert_eq!(kind(12), Kind::Custom);
    }

    /// A layout with a speaker VST 2 has no name for is refused, naming the speaker, and
    /// no arrangement is built around it.
    #[test]
    fn a_speaker_without_a_vst2_name_is_refused() {
        for speaker in [
            Speaker::TopSideLeft,
            Speaker::TopSideRight,
            Speaker::Unknown(3),
        ] {
            let topology = ChannelTopology::new([Speaker::FrontLeft, speaker]);
            assert_eq!(
                arrangement_of(&topology).unwrap_err(),
                ArrangementError::UnmappedSpeaker(speaker)
            );
        }
    }

    /// A layout wider than an arrangement holds is refused with its width.
    #[test]
    fn a_layout_past_the_arrangement_width_is_refused() {
        let wide = ChannelTopology::new(std::iter::repeat_n(Speaker::Unassigned, 65));
        assert_eq!(
            arrangement_of(&wide).unwrap_err(),
            ArrangementError::TooWide(65)
        );
        let widest = ChannelTopology::new(std::iter::repeat_n(Speaker::Unassigned, 64));
        assert!(arrangement_of(&widest).is_ok());
    }

    /// Each way is the other's inverse over every layout both can name, and `kSpeakerLfe2`,
    /// which the shared vocabulary has no speaker for, reads back as no topology.
    #[test]
    fn the_two_mappings_are_inverse_and_lfe2_is_unnamed() {
        let orders = [
            vec![Speaker::FrontCenter],
            vec![Speaker::FrontLeft, Speaker::FrontRight],
            vec![Speaker::FrontRight, Speaker::FrontLeft],
            vec![
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::FrontCenter,
                Speaker::LowFrequency,
                Speaker::SideLeft,
                Speaker::SideRight,
                Speaker::FrontLeftOfCenter,
                Speaker::FrontRightOfCenter,
                Speaker::BackCenter,
                Speaker::BackLeft,
                Speaker::BackRight,
                Speaker::TopCenter,
                Speaker::TopFrontLeft,
                Speaker::TopFrontCenter,
                Speaker::TopFrontRight,
                Speaker::TopBackLeft,
                Speaker::TopBackCenter,
                Speaker::TopBackRight,
                Speaker::Unassigned,
            ],
        ];
        for order in orders {
            let topology = ChannelTopology::new(order);
            let arrangement = arrangement_of(&topology).expect("mapped");
            assert_eq!(topology_of(&arrangement), Some(topology));
        }
        let ten_two = SpeakerArrangement::new(vec![
            Vst2Speaker::Left,
            Vst2Speaker::Right,
            Vst2Speaker::Lfe2,
        ]);
        assert_eq!(topology_of(&ten_two), None);
    }

    /// 5.1 is `L R C Lfe Ls Rs` on the wire (`kSpeakerArr51`), and the engine's 5.1 puts
    /// its side pair where VST 2 puts `Ls`/`Rs`: pinned as the SDK's numbers, not through
    /// the mapping under test.
    #[test]
    fn five_one_is_the_sdks_speaker_numbers() {
        let topology = ChannelTopology::smpte(ChannelLayout::from(6u16)).expect("5.1");
        let words = arrangement_of(&topology).expect("mapped").encode();
        let speakers: Vec<u32> = (0..6).map(|i| words[2 + 28 * i + 20]).collect();
        assert_eq!(speakers, [1, 2, 3, 4, 5, 6]);
        assert_eq!(words[0], 15, "kSpeakerArr51");
    }
}
