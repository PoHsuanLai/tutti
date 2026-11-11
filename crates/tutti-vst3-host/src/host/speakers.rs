//! VST3 `SpeakerArrangement` → [`ChannelTopology`] decoding.
//!
//! # The one thing to know about VST3 arrangements
//!
//! A `SpeakerArrangement` is a **set** — a 64-bit mask of speaker bits
//! (`vstspeaker.h:28`). Channel order is not stored anywhere; it is *derived*.
//! The SDK's own `getSpeakerIndex` (`vstspeaker.h:689-704`) returns the popcount
//! of set bits *below* a speaker, so a bus's channel order is strictly
//! ascending bit index, and `vstcomponent.cpp:192` relies on exactly that.
//!
//! Decoding is total and lossless: walk the set bits low-to-high and the
//! channel order falls out. Nothing needs a table beyond the bit names.
//!
//! The inverse does not exist here. A [`ChannelTopology`] is an ordered list,
//! so it can describe an order VST3 cannot represent (`MPEG_5_1_B`, `L R Ls Rs
//! C LFE`, is the same six speakers as `k51` in a different order, and a mask
//! cannot tell them apart); and the host only ever has channel *counts* to
//! propose, which `instance::default_arrangement_for` serves.
//!
//! # Why width alone is not enough
//!
//! Building `(1u64 << n) - 1` for a width — the low `n` bits — discards
//! placement and names the *wrong* speakers:
//!
//! | Width | Low-bits mask | Speakers it names | The layout meant |
//! |---|---|---|---|
//! | 4 | `0b1111` | `L R C Lfe` | `k40Music` = `L R Ls Rs` |
//! | 6 | `0b111111` | `L R C Lfe Ls Rs` | `k51` — correct, by coincidence |
//! | 8 | `0b1111_1111` | `L R C Lfe Ls Rs Lc Rc` | rear surrounds, not front-centre |
//!
//! Only 5.1 comes out right, and only because bits 0–5 happen to be contiguous
//! and in SMPTE order. A quad bus would ask the plugin for a centre and an LFE.

use tutti_types::{ChannelTopology, Speaker};
// The individual speaker bits live directly in `Vst`; only the named
// arrangements (`kStereo`, `k51`, …) are in the `SpeakerArr` submodule.
use vst3::Steinberg::Vst::{
    SpeakerArrangement, kSpeakerC, kSpeakerCs, kSpeakerL, kSpeakerLc, kSpeakerLfe, kSpeakerLs,
    kSpeakerM, kSpeakerR, kSpeakerRc, kSpeakerRs, kSpeakerSl, kSpeakerSr, kSpeakerTc, kSpeakerTfc,
    kSpeakerTfl, kSpeakerTfr, kSpeakerTrc, kSpeakerTrl, kSpeakerTrr,
};

/// One VST3 speaker bit and the shared [`Speaker`] it means.
///
/// A table the decode walks to name a bit; `every_named_bit_decodes_to_its_speaker`
/// pins it against the SDK's bit positions.
///
/// **Bit order is load-bearing.** This is listed ascending, and the decode
/// depends on that to produce channel order — see the module docs.
const SPEAKER_BITS: &[(SpeakerArrangement, Speaker)] = &[
    (kSpeakerL, Speaker::FrontLeft),
    (kSpeakerR, Speaker::FrontRight),
    (kSpeakerC, Speaker::FrontCenter),
    (kSpeakerLfe, Speaker::LowFrequency),
    // `Ls`/`Rs` are VST3's **5.1 surrounds** — bits 4/5, the pair in `k51`
    // (`vstspeaker.h:154-156`). The name reads "Left Surround", but the channels
    // they denote are the ones the engine's SMPTE order calls `SL`/`SR` at
    // indices 4/5, so that is what they map to. Reading the name as "rear" and
    // mapping to `BackLeft` puts a 5.1 bus's surrounds where the engine expects
    // its 7.1 rears, which is silent and wrong.
    (kSpeakerLs, Speaker::SideLeft),
    (kSpeakerRs, Speaker::SideRight),
    (kSpeakerLc, Speaker::FrontLeftOfCenter),
    (kSpeakerRc, Speaker::FrontRightOfCenter),
    // `kSpeakerCs` is an *alias* of `kSpeakerS` (both bit 8, `vstspeaker.h:50`),
    // so only one of the two names may appear here.
    (kSpeakerCs, Speaker::BackCenter),
    // `Sl`/`Sr` are the *additional* pair `k71Music` adds on top of `k51`'s
    // `Ls`/`Rs` (`vstspeaker.h:170-173`), so within a 7.1 bus they are the
    // channels beyond the 5.1 core — which the engine's SMPTE 7.1 order calls
    // `BL`/`BR` at indices 6/7. VST3's names and the engine's are swapped
    // relative to each other here; the *channels* are what must line up.
    (kSpeakerSl, Speaker::BackLeft),
    (kSpeakerSr, Speaker::BackRight),
    (kSpeakerTc, Speaker::TopCenter),
    (kSpeakerTfl, Speaker::TopFrontLeft),
    (kSpeakerTfc, Speaker::TopFrontCenter),
    (kSpeakerTfr, Speaker::TopFrontRight),
    (kSpeakerTrl, Speaker::TopBackLeft),
    (kSpeakerTrc, Speaker::TopBackCenter),
    (kSpeakerTrr, Speaker::TopBackRight),
];

/// VST3's mono marker, `kSpeakerM` — bit 19, and **not** a placed speaker.
///
/// `kMono` is `kSpeakerM` (`vstspeaker.h:123`), a dedicated "this bus is mono"
/// bit sitting above the whole placement range, rather than the centre speaker.
/// The engine's own mono is `FrontCenter` ([`ChannelTopology::smpte`] of width
/// 1), so the two vocabularies disagree about what one channel means.
///
/// It is kept out of [`SPEAKER_BITS`] and handled as a special case in
/// the decode, for two reasons. In the table `FrontCenter` would be named by two
/// bits. And its bit index (19) is above every placement bit, so
/// a mono-plus-anything mask would sort mono *last* in channel order, which is
/// meaningless.
///
/// Getting this wrong is not cosmetic: proposing `kSpeakerC` for a mono bus
/// asks a plugin for a centre channel of a surround arrangement, which a
/// mono-only plugin is entitled to refuse outright.
const MONO_BIT: SpeakerArrangement = kSpeakerM;

/// Decodes a VST3 arrangement into the channel order the plugin will use.
///
/// Total and lossless: the set bits are walked low-to-high, which *is* VST3's
/// channel order (module docs). A bit this vocabulary does not name becomes
/// [`Speaker::Unknown`] carrying the bit index, so it keeps its channel slot —
/// dropping it would renumber every later channel.
///
/// The resulting width always equals the mask's popcount, so this agrees with
/// the count-only reading it replaces.
pub(crate) fn from_arrangement(arr: SpeakerArrangement) -> ChannelTopology {
    // `kMono` alone is the engine's one-channel bus. Handled before the general
    // walk because `kSpeakerM` is a marker rather than a placement — see
    // [`MONO_BIT`]. Only when it is the *whole* arrangement: combined with
    // placement bits it means something this host cannot interpret, and the
    // walk below reports it as unnamed rather than guessing.
    if arr == MONO_BIT {
        return ChannelTopology::new([Speaker::FrontCenter]);
    }

    ChannelTopology::new((0..64).filter(|bit| arr & (1u64 << bit) != 0).map(|bit| {
        let mask = 1u64 << bit;
        SPEAKER_BITS
            .iter()
            .find(|(b, _)| *b == mask)
            .map(|(_, s)| *s)
            .unwrap_or(Speaker::Unknown(bit as u16))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::ChannelLayout;
    use vst3::Steinberg::Vst::{SpeakerArr, kSpeakerLfe2};

    /// Every speaker bit decodes to its speaker, with the bit positions written
    /// as literals from `vstspeaker.h` (`kSpeakerL = 1 << 0` ... `kSpeakerTrr =
    /// 1 << 17`), not read back from the table or the SDK constants.
    ///
    /// Mutation (run): swapping the `kSpeakerTfl` and `kSpeakerTfr` rows of
    /// `SPEAKER_BITS` fails this at bits 12 and 14.
    #[test]
    fn every_named_bit_decodes_to_its_speaker() {
        let spec: [(u32, Speaker); 18] = [
            (0, Speaker::FrontLeft),
            (1, Speaker::FrontRight),
            (2, Speaker::FrontCenter),
            (3, Speaker::LowFrequency),
            (4, Speaker::SideLeft),
            (5, Speaker::SideRight),
            (6, Speaker::FrontLeftOfCenter),
            (7, Speaker::FrontRightOfCenter),
            (8, Speaker::BackCenter),
            (9, Speaker::BackLeft),
            (10, Speaker::BackRight),
            (11, Speaker::TopCenter),
            (12, Speaker::TopFrontLeft),
            (13, Speaker::TopFrontCenter),
            (14, Speaker::TopFrontRight),
            (15, Speaker::TopBackLeft),
            (16, Speaker::TopBackCenter),
            (17, Speaker::TopBackRight),
        ];
        for (bit, speaker) in spec {
            assert_eq!(
                from_arrangement(1u64 << bit).positions(),
                &[speaker],
                "bit {bit} decoded wrong"
            );
        }
        assert_eq!(
            SPEAKER_BITS.len(),
            spec.len(),
            "a bit the spec does not list"
        );
    }

    /// No two table entries share a bit or a speaker.
    ///
    /// `kSpeakerCs` aliases `kSpeakerS` (`vstspeaker.h:50`), and VST3 has
    /// several such pairs. Listing both names of one bit would make the decode
    /// ambiguous — whichever came first would win, silently.
    #[test]
    fn the_speaker_table_has_no_duplicate_bits_or_speakers() {
        for (i, (bit_a, spk_a)) in SPEAKER_BITS.iter().enumerate() {
            for (bit_b, spk_b) in &SPEAKER_BITS[i + 1..] {
                assert_ne!(bit_a, bit_b, "{spk_a:?} and {spk_b:?} share a bit");
                assert_ne!(spk_a, spk_b, "two bits both claim {spk_a:?}");
            }
        }
    }

    /// The table is in ascending bit order, which the decode depends on.
    #[test]
    fn the_speaker_table_is_in_ascending_bit_order() {
        for pair in SPEAKER_BITS.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "{:?} must precede {:?}",
                pair[0].1,
                pair[1].1
            );
        }
    }

    /// 5.1 decodes to the engine's own channel order.
    ///
    /// Pinned against `ChannelTopology::smpte` rather than a hand-written list:
    /// the point is that VST3's derived order and the engine's SMPTE order
    /// *agree* for this layout, which is why 5.1 needs no channel remapping.
    #[test]
    fn the_51_arrangement_decodes_to_the_engines_own_order() {
        let decoded = from_arrangement(SpeakerArr::k51);
        assert_eq!(
            decoded,
            ChannelTopology::smpte(ChannelLayout::from(6u16)).expect("6 has an order"),
            "VST3's k51 and the engine's SMPTE 5.1 must be the same channel order"
        );
        assert_eq!(decoded.lfe_index(), Some(3));
    }

    /// A four-channel bus is a surround pair — *not* the four low bits.
    ///
    /// `(1<<4)-1` is `L R C Lfe`: it would ask the plugin for a centre and an
    /// LFE where the caller wanted a rear/side pair, negotiating a quad bus as
    /// a broken 3.1. Pinned by absolute speakers, not by width — a low-bits mask
    /// gets the width right, which is exactly why a width check cannot catch it.
    #[test]
    fn a_four_channel_bus_is_a_surround_pair_not_the_low_four_bits() {
        let decoded = from_arrangement(SpeakerArr::k40Music);
        assert_eq!(
            decoded.positions(),
            &[
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::SideLeft,
                Speaker::SideRight,
            ],
            "k40Music is L R Ls Rs, and VST3's Ls/Rs are the 5.1 surround pair"
        );

        // What a low-bits conversion produces, named so the difference is explicit.
        let old_mask: SpeakerArrangement = (1u64 << 4) - 1;
        assert_ne!(
            old_mask,
            SpeakerArr::k40Music,
            "the low-four-bits mask is not a four-channel surround layout"
        );
        assert_eq!(
            from_arrangement(old_mask).positions(),
            &[
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::FrontCenter,
                Speaker::LowFrequency,
            ],
            "the old mask asked for a centre and an LFE"
        );
    }

    /// VST3's four-channel layout is not the engine's quad, and is not silently
    /// treated as it.
    ///
    /// `ChannelTopology::quad()` is `FL FR BL BR` — the order `downmix.rs`
    /// folds a 4-channel buffer by. `k40Music` is `L R Ls Rs`, whose surround
    /// pair is VST3 bits 4/5, and those are the channels the engine calls
    /// `SL`/`SR`. Same width, different speakers.
    ///
    /// Recorded rather than reconciled: making them equal would require
    /// deciding that VST3's `Ls`/`Rs` mean rears, which would then put a 5.1
    /// bus's surrounds in the wrong channels. The width-4 ambiguity is real and
    /// belongs to the caller, which is why `smpte()` declines width 4 at all.
    #[test]
    fn the_vst3_four_channel_layout_is_not_the_engines_quad() {
        let vst3_four = from_arrangement(SpeakerArr::k40Music);
        assert_eq!(
            vst3_four.layout(),
            ChannelTopology::quad().layout(),
            "both are four channels"
        );
        assert_ne!(
            vst3_four,
            ChannelTopology::quad(),
            "but they name different speakers, and conflating them would move audio"
        );
    }

    /// `k71Music` decodes to the engine's own 7.1 order.
    ///
    /// `k71Music` is the 7.1 whose extra pair sits beyond the 5.1 core
    /// (`vstspeaker.h:170-173`), which is what the engine's SMPTE 7.1 describes.
    /// Pinned against `smpte` rather than a literal so the two orders are
    /// asserted to *agree*, which is what makes 7.1 need no channel remapping.
    ///
    /// The other width a low-bits mask gets wrong: `(1<<8)-1` adds the front
    /// left/right-of-centre pair instead.
    #[test]
    fn the_71_music_arrangement_decodes_to_the_engines_own_order() {
        let decoded = from_arrangement(SpeakerArr::k71Music);
        assert_eq!(
            decoded,
            ChannelTopology::smpte(ChannelLayout::from(8u16)).expect("8 has an order"),
            "k71Music and the engine's SMPTE 7.1 must be the same channel order"
        );
        assert_eq!(decoded.lfe_index(), Some(3));

        let old_mask: SpeakerArrangement = (1u64 << 8) - 1;
        assert!(
            from_arrangement(old_mask)
                .positions()
                .contains(&Speaker::FrontLeftOfCenter),
            "the old 8-channel mask named a front-of-centre pair"
        );
        assert!(
            !decoded.positions().contains(&Speaker::FrontLeftOfCenter),
            "a real 7.1 arrangement has no front-of-centre speakers"
        );
    }

    /// `k71CineFullRear` is a *different* 7.1 and is not the engine's order.
    ///
    /// Same width as `k71Music`, but its extra pair is `Lcs`/`Rcs` — back
    /// left/right *of centre*, bits 26/27 — rather than the side pair. It
    /// decodes to speakers this vocabulary does not name, so it reports as not
    /// fully named instead of being quietly conflated with `k71Music`.
    ///
    /// Recorded because "8 channels with an LFE at 3" describes both, so a
    /// width-based check cannot tell them apart — which is the whole failure
    /// mode this module exists to end.
    #[test]
    fn the_cine_full_rear_71_is_not_the_same_layout_as_music_71() {
        let cine = from_arrangement(SpeakerArr::k71CineFullRear);
        let music = from_arrangement(SpeakerArr::k71Music);

        assert_eq!(cine.layout(), music.layout(), "both are 8 channels");
        assert_ne!(cine, music, "but they are not the same channel order");
        assert!(
            !cine.is_fully_named(),
            "Lcs/Rcs have no name in this vocabulary yet, so they must report unnamed \
             rather than being conflated with another speaker"
        );
    }

    /// An empty arrangement is an empty topology, not a failure.
    #[test]
    fn an_empty_arrangement_decodes_to_no_channels() {
        let decoded = from_arrangement(SpeakerArr::kEmpty);
        assert_eq!(decoded.layout(), ChannelLayout::EMPTY);
    }

    /// An unnamed bit keeps its channel slot rather than vanishing.
    ///
    /// Asserted on the *following* channel's position, because that is what a
    /// dropped entry corrupts. A width check alone would pass if the decode
    /// dropped one bit and invented another.
    #[test]
    fn an_unnamed_bit_holds_its_channel_slot() {
        // Bit 18 is `kSpeakerLfe2`, a real VST3 speaker this vocabulary does
        // not name. Paired with stereo it must still occupy channel 2 rather
        // than shortening the bus.
        let arr = SpeakerArr::kStereo | kSpeakerLfe2;
        let decoded = from_arrangement(arr);
        assert_eq!(decoded.layout(), ChannelLayout::from(3u16));
        assert_eq!(decoded.positions()[2], Speaker::Unknown(18));
        assert!(!decoded.is_fully_named());
    }

    /// `kMono` decodes to the engine's mono, not to a placed centre speaker.
    ///
    /// VST3's `kMono` is `kSpeakerM` (bit 19) — a marker above the whole
    /// placement range — while the engine's one-channel bus is `FrontCenter`.
    #[test]
    fn the_mono_marker_decodes_to_the_engines_mono() {
        let engine_mono = ChannelTopology::smpte(ChannelLayout::MONO).expect("mono has an order");
        assert_ne!(
            SpeakerArr::kMono,
            kSpeakerC,
            "kMono is the mono marker, not the centre speaker"
        );

        let decoded = from_arrangement(SpeakerArr::kMono);
        assert_eq!(decoded, engine_mono);
        assert_eq!(decoded.layout(), ChannelLayout::MONO);
    }

    /// The mono marker mixed with placement bits is not silently read as mono.
    ///
    /// `kSpeakerM` sits at bit 19, above every placement bit, so a
    /// mono-plus-anything mask has no sensible channel order. Reporting the
    /// marker as unnamed is the honest answer; treating the whole thing as mono
    /// would drop real channels.
    #[test]
    fn the_mono_marker_beside_placements_is_not_read_as_mono() {
        let odd = SpeakerArr::kStereo | kSpeakerM;
        let decoded = from_arrangement(odd);
        assert_eq!(decoded.layout(), ChannelLayout::from(3u16));
        assert_eq!(decoded.positions()[2], Speaker::Unknown(19));
    }

    /// Decoded width always equals the mask's popcount.
    ///
    /// The compatibility property: everything upstream still sizes buffers from
    /// a count, and this is what guarantees the richer decode cannot disagree
    /// with the count-only reading it replaced.
    #[test]
    fn the_decoded_width_is_the_masks_popcount() {
        for arr in [
            SpeakerArr::kEmpty,
            SpeakerArr::kMono,
            SpeakerArr::kStereo,
            SpeakerArr::k40Music,
            SpeakerArr::k51,
            SpeakerArr::k71CineFullRear,
            SpeakerArr::kStereo | (1u64 << 19),
        ] {
            assert_eq!(
                u32::from(from_arrangement(arr).layout().count()),
                arr.count_ones(),
                "width disagreed with popcount for {arr:#x}"
            );
        }
    }
}
