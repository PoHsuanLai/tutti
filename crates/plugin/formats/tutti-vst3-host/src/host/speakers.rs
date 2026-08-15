//! VST3 `SpeakerArrangement` ↔ [`ChannelTopology`] conversion.
//!
//! # The one thing to know about VST3 arrangements
//!
//! A `SpeakerArrangement` is a **set** — a 64-bit mask of speaker bits
//! (`vstspeaker.h:28`). Channel order is not stored anywhere; it is *derived*.
//! The SDK's own `getSpeakerIndex` (`vstspeaker.h:689-704`) returns the popcount
//! of set bits *below* a speaker, so a bus's channel order is strictly
//! ascending bit index, and `vstcomponent.cpp:192` relies on exactly that.
//!
//! Two consequences, and both shape this module:
//!
//! 1. **Decoding is total and lossless.** Walk the set bits low-to-high and the
//!    channel order falls out. Nothing needs a table.
//! 2. **Encoding can fail.** A [`ChannelTopology`] is an ordered list, so it can
//!    describe an order VST3 cannot represent — `MPEG_5_1_B`
//!    (`L R Ls Rs C LFE`) is the same six speakers as `k51` in a different
//!    order, and a mask cannot tell them apart. [`to_arrangement`] returns
//!    `None` there rather than silently proposing an arrangement whose channel
//!    order is not the caller's.
//!
//! That asymmetry is the whole reason the shared type is a list; see
//! `docs/design/007-channel-topology.md`.
//!
//! # What this replaces
//!
//! The previous conversion built `(1u64 << n) - 1` for any width above stereo —
//! the low `n` bits — and decoded by `count_ones()`. Both directions discarded
//! placement, and the encode was *wrong*, not merely lossy:
//!
//! | Width | Old mask | Speakers it names | The layout meant |
//! |---|---|---|---|
//! | 4 | `0b1111` | `L R C Lfe` | `k40Music` = `L R Ls Rs` |
//! | 6 | `0b111111` | `L R C Lfe Ls Rs` | `k51` — correct, by coincidence |
//! | 8 | `0b1111_1111` | `L R C Lfe Ls Rs Lc Rc` | rear surrounds, not front-centre |
//!
//! Only 5.1 came out right, and only because bits 0–5 happen to be contiguous
//! and in SMPTE order. A quad bus asked the plugin for a centre and an LFE.

use tutti_types::{ChannelTopology, Speaker};
// The individual speaker bits live directly in `Vst`; only the named
// arrangements (`kStereo`, `k51`, …) are in the `SpeakerArr` submodule.
use vst3::Steinberg::Vst::{
    kSpeakerC, kSpeakerCs, kSpeakerL, kSpeakerLc, kSpeakerLfe, kSpeakerLs, kSpeakerM, kSpeakerR,
    kSpeakerRc, kSpeakerRs, kSpeakerSl, kSpeakerSr, kSpeakerTc, kSpeakerTfc, kSpeakerTfl,
    kSpeakerTfr, kSpeakerTrc, kSpeakerTrl, kSpeakerTrr, SpeakerArrangement,
};

/// One VST3 speaker bit and the shared [`Speaker`] it means.
///
/// A table rather than two `match` arms because the mapping must be a
/// bijection: the decode walks it to name a bit, the encode walks it to find a
/// bit, and a value appearing in one direction but not the other is the bug
/// class a round-trip test catches. See `every_named_speaker_round_trips`.
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
    // so only one of the two names may appear here — listing both would make
    // the round trip ambiguous in the encode direction.
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
/// It is kept out of [`SPEAKER_BITS`] and handled as a special case at both
/// ends, for two reasons. In the table it would break the bijection —
/// `FrontCenter` would map to two different bits, and whichever came first
/// would silently win. And its bit index (19) is above every placement bit, so
/// a mono-plus-anything mask would sort mono *last* in channel order, which is
/// meaningless.
///
/// Getting this wrong is not cosmetic: proposing `kSpeakerC` for a mono bus
/// asks a plugin for a centre channel of a surround arrangement, which a
/// mono-only plugin is entitled to refuse outright.
const MONO_BIT: SpeakerArrangement = kSpeakerM;

/// Decode a VST3 arrangement into the channel order the plugin will use.
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

/// Encode a topology as a VST3 arrangement, or `None` if VST3 cannot express
/// this channel order.
///
/// Fails — rather than proposing a near-miss — in three cases, all of which
/// would otherwise put the plugin's channels somewhere the caller did not
/// intend:
///
/// - **An order VST3 cannot represent.** A mask has no order of its own, so it
///   can only describe a topology already in ascending-bit-index order.
///   `MPEG_5_1_B` has the same speakers as `k51` in a different order and is
///   simply not expressible.
/// - **A speaker with no VST3 bit**, including any [`Speaker::Unknown`] — the
///   payload is the *source format's* raw value, which is meaningless here.
/// - **A duplicate speaker**, which a set cannot represent at all.
///
/// `None` is a real answer for a caller to act on: `setBusArrangements` is a
/// proposal, so the honest move is to propose nothing and take what the plugin
/// reports back.
/// Callerless in production today, and deliberately so: `negotiate_bus_arrangements`
/// only has channel *counts* to work from, so it uses `instance::default_arrangement_for`
/// — the width-only fallback whose own docs name this function as its successor
/// "once a caller has a real [`ChannelTopology`] to offer". Not deleted, because
/// `SPEAKER_BITS` must stay a bijection and `every_named_speaker_round_trips` is
/// the only thing that checks it — and that test exercises the decode direction
/// production *does* use. See docs/design/007-channel-topology.md.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn to_arrangement(topology: &ChannelTopology) -> Option<SpeakerArrangement> {
    // The inverse of the mono case in `from_arrangement`: a single-channel bus
    // is `kMono`, not `kSpeakerC`. See [`MONO_BIT`].
    if topology.positions() == [Speaker::FrontCenter] {
        return Some(MONO_BIT);
    }

    let mut mask: SpeakerArrangement = 0;
    let mut previous_bit: Option<u32> = None;

    for speaker in topology.positions() {
        let bit = SPEAKER_BITS
            .iter()
            .find(|(_, s)| s == speaker)
            .map(|(b, _)| *b)?;

        // A set holds each speaker once; a repeat means the list said something
        // a mask cannot.
        if mask & bit != 0 {
            return None;
        }

        // Ascending order is what makes the mask's *derived* order equal the
        // caller's *stated* one. Out of order, the plugin would receive the
        // right speakers with the channels permuted — silent, and audible.
        let index = bit.trailing_zeros();
        if previous_bit.is_some_and(|p| index <= p) {
            return None;
        }
        previous_bit = Some(index);

        mask |= bit;
    }

    Some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::ChannelLayout;
    use vst3::Steinberg::Vst::{kSpeakerLfe2, SpeakerArr};

    /// Every speaker in the table survives a round trip through the mask.
    ///
    /// The table is the bijection this module rests on: the decode looks up by
    /// bit, the encode by speaker. An entry reachable one way but not the other
    /// compiles and silently mis-routes, so it is checked rather than reviewed.
    #[test]
    fn every_named_speaker_round_trips() {
        for (bit, speaker) in SPEAKER_BITS {
            let decoded = from_arrangement(*bit);
            assert_eq!(
                decoded.positions(),
                &[*speaker],
                "bit {} decoded wrong",
                bit.trailing_zeros()
            );

            // A lone `FrontCenter` is the engine's *mono* bus, which encodes to
            // `kMono` rather than `kSpeakerC` — see `MONO_BIT`. Pairing it with
            // another speaker keeps this a test of the table rather than of
            // that deliberate special case, which
            // `mono_maps_to_the_mono_marker_not_the_centre_speaker` covers.
            let (probe, expected) = if *speaker == Speaker::FrontCenter {
                (
                    ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontCenter]),
                    kSpeakerL | *bit,
                )
            } else {
                (decoded, *bit)
            };
            assert_eq!(
                to_arrangement(&probe),
                Some(expected),
                "{speaker:?} did not encode back to its own bit"
            );
        }
    }

    /// No two table entries share a bit or a speaker.
    ///
    /// `kSpeakerCs` aliases `kSpeakerS` (`vstspeaker.h:50`), and VST3 has
    /// several such pairs. Listing both names of one bit would make the encode
    /// direction ambiguous — whichever came first would win, silently.
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
    /// The defect this module fixes. `(1<<4)-1` is `L R C Lfe`: it asked the
    /// plugin for a centre and an LFE where the caller wanted a rear/side pair,
    /// so a quad bus was negotiated as a broken 3.1. Pinned by absolute
    /// speakers, not by width — the old code got the width right, which is
    /// exactly why nothing caught it.
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
        assert_eq!(to_arrangement(&decoded), Some(SpeakerArr::k40Music));

        // What the old conversion produced, named so the difference is explicit.
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

        // The engine's quad is expressible in VST3 — as a *different* mask.
        assert_ne!(
            to_arrangement(&ChannelTopology::quad()),
            Some(SpeakerArr::k40Music),
            "the engine's rear-pair quad must not encode as the side-pair layout"
        );
    }

    /// `k71Music` decodes to the engine's own 7.1 order.
    ///
    /// `k71Music` is the 7.1 whose extra pair sits beyond the 5.1 core
    /// (`vstspeaker.h:170-173`), which is what the engine's SMPTE 7.1 describes.
    /// Pinned against `smpte` rather than a literal so the two orders are
    /// asserted to *agree*, which is what makes 7.1 need no channel remapping.
    ///
    /// The other width the old mask got wrong: `(1<<8)-1` added the front
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
        assert_eq!(to_arrangement(&decoded), Some(SpeakerArr::kEmpty));
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

    /// Mono round-trips as `kMono`, not as the centre speaker.
    ///
    /// VST3's `kMono` is `kSpeakerM` (bit 19) — a marker above the whole
    /// placement range — while the engine's one-channel bus is `FrontCenter`.
    /// Encoding mono as `kSpeakerC` would ask a plugin for the centre channel
    /// of a surround arrangement, which a mono-only plugin may refuse outright.
    #[test]
    fn mono_maps_to_the_mono_marker_not_the_centre_speaker() {
        let engine_mono = ChannelTopology::smpte(ChannelLayout::MONO).expect("mono has an order");
        assert_eq!(to_arrangement(&engine_mono), Some(SpeakerArr::kMono));
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

    /// A topology VST3 cannot order is refused, not approximated.
    ///
    /// `MPEG_5_1_B` is `k51`'s speakers in a different order. Encoding it as
    /// `k51` would hand the plugin the right speakers with the channels
    /// permuted — centre where the LFE belongs — which is silent and audible.
    #[test]
    fn an_order_vst3_cannot_express_is_refused() {
        // The same six speakers as `k51` — note `SideLeft`/`SideRight`, which
        // are VST3's bits 4/5 — with centre and LFE moved to the end.
        let mpeg_51_b = ChannelTopology::new([
            Speaker::FrontLeft,
            Speaker::FrontRight,
            Speaker::SideLeft,
            Speaker::SideRight,
            Speaker::FrontCenter,
            Speaker::LowFrequency,
        ]);
        assert_eq!(
            to_arrangement(&mpeg_51_b),
            None,
            "a mask cannot carry this order, so proposing one would permute the channels"
        );

        // Same speakers, ascending order: expressible, and it is exactly k51.
        let smpte = ChannelTopology::smpte(ChannelLayout::from(6u16)).expect("6 has an order");
        assert_eq!(to_arrangement(&smpte), Some(SpeakerArr::k51));
    }

    /// A speaker with no VST3 bit refuses the whole encode.
    #[test]
    fn a_speaker_vst3_does_not_define_is_refused() {
        let with_unknown =
            ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight, Speaker::Unknown(7)]);
        assert_eq!(
            to_arrangement(&with_unknown),
            None,
            "an Unknown payload is another format's value and names no VST3 bit"
        );
    }

    /// A repeated speaker refuses, since a set cannot hold one twice.
    #[test]
    fn a_duplicate_speaker_is_refused() {
        let doubled = ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontLeft]);
        assert_eq!(to_arrangement(&doubled), None);
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
