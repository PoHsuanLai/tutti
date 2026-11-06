//! [`AuLayoutTag`] ↔ [`ChannelTopology`] conversion.
//!
//! # Keyed on the tag, never on the count
//!
//! This is the whole discipline of the module. Apple defines **four different
//! orders of the identical six speakers** — `MPEG_5_1_A` through `_D`
//! (`CoreAudioBaseTypes.h:1287-1290`) — plus `Emagic_Default_7_1` and `WAVE_7_1`
//! as further orders of one eight-speaker set. A channel count cannot
//! distinguish any of them, so nothing here may branch on width.
//!
//! That is not a hypothetical hazard: `AudioUnit_5_1` is `L R C LFE Ls Rs` while
//! `AudioUnit_5_0` is `L R Ls Rs C`, so the *same* index means centre in one and
//! a surround in the other. Reading a 5.1 buffer as though it were 5.0-plus-LFE
//! puts dialogue in a surround speaker at full level.
//!
//! # Apple's `Ls`/`Rs` are the engine's `SL`/`SR`
//!
//! The same naming collision the VST3 side documents. Apple's `Ls`/`Rs` in
//! `AudioUnit_5_1` are the 5.1 surround pair, which the engine's SMPTE order
//! calls `SL`/`SR` at indices 4/5; `Rls`/`Rrs` in `AudioUnit_7_1` are the rears
//! the engine calls `BL`/`BR`. Mapping by the *name* rather than the *channel*
//! swaps a 5.1 bus's surrounds into its 7.1 rear slots.
//!
//! # What has no topology
//!
//! Several tags describe a **processing** relationship rather than speaker
//! placement, and they get `None` rather than an invented pair of speakers:
//! `MatrixStereo` (Lt/Rt, a matrix encoding), `MidSide` (needs a decode before
//! anything can pan it), `XY` and `Binaural` (microphone/rendering techniques),
//! and `AmbisonicBFormat` (`W X Y Z`, a spherical-harmonic encoding, not four
//! feeds). `StereoHeadphones` is likewise not plain stereo — it tells the AU to
//! skip crosstalk compensation — so it too declines.
//!
//! Declining is the point. `MidSide` silently read as `L R` is a stereo image
//! collapsed to mono-plus-noise, and the count says nothing is wrong.

#![cfg(target_os = "macos")]

use tutti_types::{ChannelTopology, Speaker};

use crate::channel_layout::AuLayoutTag;

/// The channel order for a tag, or `None` when the tag names no speaker
/// placement.
///
/// Every arm's order is Apple's own, quoted from `CoreAudioBaseTypes.h` at the
/// line given. The header comments *are* the table — there is no algorithm.
pub fn topology_of(tag: AuLayoutTag) -> Option<ChannelTopology> {
    use Speaker::*;
    let positions: &[Speaker] = match tag {
        // `:1260` — "a standard mono stream".
        AuLayoutTag::Mono => &[FrontCenter],
        // `:1261` — "(L R)".
        AuLayoutTag::Stereo => &[FrontLeft, FrontRight],
        // `:1269` — "L R Ls Rs". Apple's `Ls`/`Rs` are the surround pair.
        AuLayoutTag::Quadraphonic => &[FrontLeft, FrontRight, SideLeft, SideRight],
        // `:1270` — "L R Ls Rs C". Note the centre is *last*, not third.
        AuLayoutTag::Pentagonal => &[FrontLeft, FrontRight, SideLeft, SideRight, FrontCenter],
        // `:1271` — "L R Ls Rs C Cs".
        AuLayoutTag::Hexagonal => &[
            FrontLeft,
            FrontRight,
            SideLeft,
            SideRight,
            FrontCenter,
            BackCenter,
        ],
        // `:1343` — "L R Ls Rs C" (`MPEG_5_0_B`). Same order as Pentagonal, a
        // different tag; both are listed rather than aliased because Apple
        // gives them distinct values and an AU may publish either.
        AuLayoutTag::AudioUnit5_0 => &[FrontLeft, FrontRight, SideLeft, SideRight, FrontCenter],
        // `:1287,1347` — "L R C LFE Ls Rs" (`MPEG_5_1_A`). LFE at index 3, which
        // is also the engine's SMPTE order, so this tag needs no remapping.
        AuLayoutTag::AudioUnit5_1 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            SideLeft,
            SideRight,
        ],
        // `:1344` — "L R Ls Rs C Cs".
        AuLayoutTag::AudioUnit6_0 => &[
            FrontLeft,
            FrontRight,
            SideLeft,
            SideRight,
            FrontCenter,
            BackCenter,
        ],
        // `:1345` — "L R Ls Rs C Rls Rrs".
        AuLayoutTag::AudioUnit7_0 => &[
            FrontLeft,
            FrontRight,
            SideLeft,
            SideRight,
            FrontCenter,
            BackLeft,
            BackRight,
        ],
        // `:1291,1349` — "L R C LFE Ls Rs Rls Rrs" (`MPEG_7_1_C`). The engine's
        // SMPTE 7.1 order, so likewise no remapping.
        AuLayoutTag::AudioUnit7_1 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            SideLeft,
            SideRight,
            BackLeft,
            BackRight,
        ],

        // Tags that name a processing relationship rather than placement, plus
        // the two "look elsewhere" sentinels and anything this crate does not
        // name. See the module docs for why each declines.
        AuLayoutTag::StereoHeadphones
        | AuLayoutTag::MatrixStereo
        | AuLayoutTag::MidSide
        | AuLayoutTag::XY
        | AuLayoutTag::Binaural
        | AuLayoutTag::AmbisonicBFormat
        | AuLayoutTag::UseChannelDescriptions
        | AuLayoutTag::UseChannelBitmap
        | AuLayoutTag::Unknown(_) => return None,

        // Two placement layouts this vocabulary cannot yet spell, declining for
        // a different reason than the group above — the order is known, the
        // *speakers* are not.
        //
        // `Octagonal` (`:1272`) is "L R Ls Rs C Cs Lw Rw", and `Lw`/`Rw` are
        // the wide pair (`kAudioChannelLabel_LeftWide`, `:1002`), which
        // `Speaker` does not name. `Cube` (`:1273`) is documented only as
        // "left, right, rear left, rear right" for eight channels — four names
        // for eight slots, so its upper half is unstated in the header itself.
        //
        // Naming six of eight and inventing the rest would be worse than
        // declining: a partly-right order routes most channels correctly and
        // hides the two that are wrong. They become expressible when `Speaker`
        // grows a wide pair, which is a `tutti-types` decision.
        AuLayoutTag::Octagonal | AuLayoutTag::Cube => return None,
    };
    Some(ChannelTopology::new(positions.iter().copied()))
}

/// The tag whose channel order is exactly `topology`, or `None` if no tag this
/// crate names describes it.
///
/// The inverse of [`topology_of`], found by search rather than by a second
/// table — one table cannot disagree with itself. `None` is a real answer: a
/// caller proposing a layout the AU has no tag for should propose nothing
/// rather than the nearest tag, since "nearest" here means a different speaker
/// order.
///
/// Where two tags share an order (`Pentagonal` and `AudioUnit5_0` are both
/// `L R Ls Rs C`), the first match in [`SEARCH_ORDER`] wins. That is a real
/// choice rather than an accident of enum ordering, which is why the list is
/// written out.
pub fn tag_for(topology: &ChannelTopology) -> Option<AuLayoutTag> {
    SEARCH_ORDER
        .iter()
        .copied()
        .find(|tag| topology_of(*tag).as_ref() == Some(topology))
}

/// The tags [`tag_for`] searches, in preference order.
///
/// `AudioUnit*` variants come first: they are what an AU is most likely to
/// accept, and where two tags describe one order the `AudioUnit` spelling is
/// the one Apple documents for units. Explicit rather than derived from the
/// enum so that adding a variant cannot silently reorder the preference.
const SEARCH_ORDER: &[AuLayoutTag] = &[
    AuLayoutTag::Mono,
    AuLayoutTag::Stereo,
    AuLayoutTag::Quadraphonic,
    AuLayoutTag::AudioUnit5_0,
    AuLayoutTag::AudioUnit5_1,
    AuLayoutTag::AudioUnit6_0,
    AuLayoutTag::AudioUnit7_0,
    AuLayoutTag::AudioUnit7_1,
    AuLayoutTag::Pentagonal,
    AuLayoutTag::Hexagonal,
];

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::ChannelLayout;

    /// A decoded tag's width matches the count Apple packs into it.
    ///
    /// The tag's low 16 bits are its channel count, so this checks the table
    /// against Apple's own arithmetic rather than against itself — a
    /// transcription that dropped or doubled a speaker fails here.
    #[test]
    fn every_topology_matches_the_tags_own_channel_count() {
        for tag in SEARCH_ORDER {
            let topology = topology_of(*tag).expect("every searched tag has a topology");
            assert_eq!(
                Some(topology.layout().count()),
                tag.channel_count(),
                "{tag:?} topology width disagrees with the count in its tag"
            );
        }
    }

    /// Every tag with a topology round-trips back to a tag with the same order.
    ///
    /// Not necessarily the *same* tag: `Pentagonal` and `AudioUnit5_0` share an
    /// order, so one resolves to the other. What must hold is that the order
    /// survives, which is what a caller depends on.
    #[test]
    fn every_topology_round_trips_to_an_equivalent_tag() {
        for tag in SEARCH_ORDER {
            let topology = topology_of(*tag).expect("searched tags have topologies");
            let back = tag_for(&topology).expect("a table entry must be findable");
            assert_eq!(
                topology_of(back),
                Some(topology),
                "{tag:?} round-tripped to {back:?}, which is a different order"
            );
        }
    }

    /// 5.1 and 7.1 agree with the engine's own SMPTE order.
    ///
    /// The two layouts where AU and the engine happen to coincide, which is why
    /// they need no channel remapping. Asserted against `smpte` rather than a
    /// literal so the claim is about *agreement*, not about a copied list.
    #[test]
    fn the_audiounit_surround_tags_match_the_engines_smpte_order() {
        assert_eq!(
            topology_of(AuLayoutTag::AudioUnit5_1),
            ChannelTopology::smpte(ChannelLayout::from(6u16)),
            "AudioUnit_5_1 is L R C LFE Ls Rs, the engine's own 5.1 order"
        );
        assert_eq!(
            topology_of(AuLayoutTag::AudioUnit7_1),
            ChannelTopology::smpte(ChannelLayout::from(8u16)),
            "AudioUnit_7_1 is L R C LFE Ls Rs Rls Rrs, the engine's own 7.1 order"
        );
    }

    /// 5.0 and 5.1 put different speakers at the same index.
    ///
    /// The concrete reason this module keys on the tag: `AudioUnit_5_0` is
    /// `L R Ls Rs C` and `AudioUnit_5_1` is `L R C LFE Ls Rs`, so index 2 is a
    /// surround in one and the centre in the other. A width-based mapping
    /// cannot see the difference, and getting it wrong puts dialogue in a
    /// surround speaker.
    #[test]
    fn the_50_and_51_tags_disagree_about_what_each_index_means() {
        let five_oh = topology_of(AuLayoutTag::AudioUnit5_0).expect("5.0 has an order");
        let five_one = topology_of(AuLayoutTag::AudioUnit5_1).expect("5.1 has an order");

        assert_eq!(five_oh.positions()[2], Speaker::SideLeft);
        assert_eq!(five_one.positions()[2], Speaker::FrontCenter);
        assert_eq!(five_oh.index_of(Speaker::FrontCenter), Some(4));
        assert_eq!(five_one.index_of(Speaker::FrontCenter), Some(2));
    }

    /// Hexagonal and AudioUnit_6_0 are six channels in the same order.
    ///
    /// Both are `L R Ls Rs C Cs` (`:1271`, `:1344`). Recorded because it looks
    /// like a transcription slip until checked against the header.
    #[test]
    fn hexagonal_and_audiounit_60_share_an_order() {
        assert_eq!(
            topology_of(AuLayoutTag::Hexagonal),
            topology_of(AuLayoutTag::AudioUnit6_0)
        );
    }

    /// Tags naming a processing relationship have no topology.
    ///
    /// Each of these has a channel *count* and no speaker placement, so a
    /// count-based mapping would invent one. `MidSide` read as `L R` is the
    /// worst case: a stereo image collapsed to mono plus noise, with nothing to
    /// indicate a problem.
    #[test]
    fn a_tag_that_names_no_placement_declines() {
        for tag in [
            AuLayoutTag::StereoHeadphones,
            AuLayoutTag::MatrixStereo,
            AuLayoutTag::MidSide,
            AuLayoutTag::XY,
            AuLayoutTag::Binaural,
            AuLayoutTag::AmbisonicBFormat,
            AuLayoutTag::UseChannelDescriptions,
            AuLayoutTag::UseChannelBitmap,
            AuLayoutTag::Unknown(0xDEAD_BEEF),
        ] {
            assert_eq!(
                topology_of(tag),
                None,
                "{tag:?} names no speaker placement and must not be given one"
            );
        }
    }

    /// Octagonal and Cube decline for want of vocabulary, not want of an order.
    ///
    /// Distinct from the group above: these *are* speaker layouts. `Octagonal`
    /// needs a wide pair `Speaker` does not name, and `Cube`'s own header
    /// comment lists four names for eight channels. Declining beats naming six
    /// of eight, which would route most channels correctly and hide the rest.
    ///
    /// Pinned so that adding a wide pair to `Speaker` surfaces here as a
    /// failing test rather than being forgotten.
    #[test]
    fn the_wide_and_cube_layouts_decline_for_want_of_speaker_names() {
        assert_eq!(topology_of(AuLayoutTag::Octagonal), None);
        assert_eq!(topology_of(AuLayoutTag::Cube), None);
        assert_eq!(AuLayoutTag::Octagonal.channel_count(), Some(8));
        assert_eq!(AuLayoutTag::Cube.channel_count(), Some(8));
    }

    /// B-format is refused even though it is four channels.
    ///
    /// The positive half of the ambiguity `ChannelLayout::QUAD` papers over:
    /// `Ambisonic_B_Format` and `Quadraphonic` are both width 4, and only one
    /// of them is speaker feeds.
    #[test]
    fn b_format_and_quadraphonic_are_both_four_channels_but_only_one_has_speakers() {
        assert_eq!(
            AuLayoutTag::AmbisonicBFormat.channel_count(),
            AuLayoutTag::Quadraphonic.channel_count()
        );
        assert_eq!(topology_of(AuLayoutTag::AmbisonicBFormat), None);
        assert!(topology_of(AuLayoutTag::Quadraphonic).is_some());
    }

    /// An order no AU tag describes finds no tag.
    #[test]
    fn an_order_no_tag_describes_is_refused() {
        // The engine's quad — `FL FR BL BR` — is a rear pair, where Apple's
        // Quadraphonic is a surround pair. No tag names it.
        assert_eq!(tag_for(&ChannelTopology::quad()), None);
    }

    /// The search list holds no duplicates.
    ///
    /// A repeat would make the "first match wins" preference depend on where
    /// the duplicate sits, which is exactly the kind of ordering accident the
    /// explicit list exists to prevent.
    #[test]
    fn the_search_order_has_no_duplicates() {
        for (i, a) in SEARCH_ORDER.iter().enumerate() {
            for b in &SEARCH_ORDER[i + 1..] {
                assert_ne!(a, b, "{a:?} appears twice in SEARCH_ORDER");
            }
        }
    }
}
