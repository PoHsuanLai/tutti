//! [`ChannelTopology`] — *which speaker* each channel feeds, as opposed to
//! [`ChannelLayout`](crate::ChannelLayout)'s *how many*.
//!
//! The two are deliberately separate types. `ChannelLayout` is a count and says
//! so in its own module docs: foreign layout types "convert to and from this at
//! their crate boundary, losing the placement deliberately". That remains true
//! and is not being reversed — this type is what a caller reaches for when the
//! placement is the thing it needs, and nothing that only needs a width should
//! change.
//!
//! The cost of having only a count is visible in the vocabulary today:
//! [`ChannelLayout::QUAD`](crate::ChannelLayout::QUAD) is documented "quad /
//! ambisonic B-format" — one constant for two incompatible meanings, because a
//! count cannot tell four speaker feeds from a spherical-harmonic encoding.
//!
//! # Why an ordered list rather than a set of speakers
//!
//! Surveyed at the four plugin formats' own headers, because they do not agree:
//!
//! - **VST3** is a **set** — a 64-bit mask. Channel order is not stored, it is
//!   *derived*: `getSpeakerIndex` returns the popcount of set bits below a
//!   speaker, so buffer order is strictly ascending bit index. VST3 therefore
//!   cannot express two different orders of the same speaker set.
//! - **AU**, **CLAP** and **VST2** are **ordered lists**. Apple defines
//!   `MPEG_5_1_A/B/C/D` as four *different orders* of the identical six
//!   speakers; CLAP's `channel_map[i]` is literally the speaker fed by channel
//!   `i`.
//!
//! A list is strictly the more expressive of the two: every VST3 arrangement
//! converts to a list without loss, while `MPEG_5_1_B` cannot be represented as
//! a set at all. So the shared type is a list, and the one lossy direction is
//! *out* to VST3 — which is where a conversion has to decide whether to reorder
//! the caller's buffers or refuse.
//!
//! # Ambisonics is not speaker positions
//!
//! B-format `W X Y Z` is a spherical-harmonic encoding, not four speaker feeds,
//! and all three formats that carry it treat it as a separate axis. It is
//! deliberately **not** modelled here as four invented [`Speaker`]s — that
//! would recreate the exact ambiguity described above, one step further in.

use smallvec::SmallVec;

use crate::ChannelLayout;

/// Inline capacity for [`ChannelTopology`]. Eight covers every layout that can
/// reach the graph: `tutti_core::engine::MAX_ROOT_CHANNELS` is 8, and the
/// offline renderer's own ceiling is 12 — a 12-channel bed spills to the heap
/// rather than being rejected, which is the right trade for a control-thread
/// type.
const INLINE_CHANNELS: usize = 8;

/// Which speaker one channel of a bus feeds.
///
/// # Why an open catalog
///
/// The formats keep adding positions — CLAP's list grew a top-side pair, Apple
/// ships new spatial tags per release — so a closed enum would turn a plugin
/// update into a decode failure. [`Unknown`](Self::Unknown) carries the value
/// verbatim instead.
///
/// That arm is not merely for logging. A topology is **positional**, so an
/// unnameable speaker must still occupy its channel's slot: dropping it would
/// renumber every channel after it and silently re-route the tail of the bus.
/// This is not hypothetical — the CLAP host shipped exactly that bug by
/// `filter_map`ping unknown positions out of a channel map.
///
/// The variants are the positions common to more than one format. A
/// format-specific speaker (VST3's proximity pair, AU's matrix-total Lt/Rt)
/// stays `Unknown` until a second format names it too; inventing a shared name
/// for a thing one format means is how a vocabulary stops being shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Speaker {
    FrontLeft,
    FrontRight,
    FrontCenter,
    /// The `.1` — a band-limited effects channel, not a placed speaker.
    LowFrequency,
    BackLeft,
    BackRight,
    /// Front left-of-centre, between `FrontLeft` and `FrontCenter`.
    FrontLeftOfCenter,
    /// Front right-of-centre.
    FrontRightOfCenter,
    BackCenter,
    SideLeft,
    SideRight,
    TopCenter,
    TopFrontLeft,
    TopFrontCenter,
    TopFrontRight,
    TopBackLeft,
    TopBackCenter,
    TopBackRight,
    TopSideLeft,
    TopSideRight,
    /// A position this vocabulary does not name, carried verbatim so it keeps
    /// its channel's slot. The payload is the *format's* raw value and is only
    /// meaningful to the format that produced it.
    Unknown(u16),
}

impl Speaker {
    /// Whether this is the LFE / `.1` channel.
    ///
    /// Worth a predicate rather than an `==` at each call site because the LFE
    /// is the one position with different *handling* rather than a different
    /// place: it is excluded from a consumer downmix (see
    /// [`crate::downmix`]), and it is not a VBAP-panned speaker.
    pub const fn is_lfe(self) -> bool {
        matches!(self, Self::LowFrequency)
    }
}

/// The speaker each channel of a bus feeds, in channel order.
///
/// `positions()[i]` is the speaker fed by channel `i`, so the length **is** the
/// channel count — see [`layout`](Self::layout).
///
/// # No stored width
///
/// The count is derived, never stored beside the list. A stored copy would be a
/// second owner of the same fact and would need invalidation on every mutation;
/// deriving it cannot drift. This is the same rule that keeps
/// `LoadedPlugin::multi_bus` a method rather than a flag.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChannelTopology {
    positions: SmallVec<[Speaker; INLINE_CHANNELS]>,
}

impl ChannelTopology {
    /// Build a topology from an ordered list of speakers.
    ///
    /// Takes any iterator rather than a slice so a format's decode loop can
    /// feed it directly without collecting twice.
    ///
    /// Duplicates are **not** rejected. A plugin that reports two `FrontLeft`
    /// channels is describing something this host cannot route sensibly, but
    /// refusing here would discard the whole map — including the channels that
    /// *were* readable — and a boundary layer does not get to drop data because
    /// a consumer might not like it. [`index_of`](Self::index_of) answers with
    /// the first, which is the only defensible reading of an ambiguous map.
    pub fn new(positions: impl IntoIterator<Item = Speaker>) -> Self {
        Self {
            positions: positions.into_iter().collect(),
        }
    }

    /// The speaker each channel feeds, in channel order.
    pub fn positions(&self) -> &[Speaker] {
        &self.positions
    }

    /// The channel count, as a [`ChannelLayout`].
    ///
    /// The bridge back to the count-only vocabulary: every existing signature
    /// keeps taking a width, and this is how a topology satisfies one.
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::from(self.positions.len() as u16)
    }

    /// Which channel feeds `speaker`, or `None` if this bus has none.
    ///
    /// The first match when a speaker appears twice (see [`new`](Self::new)).
    pub fn index_of(&self, speaker: Speaker) -> Option<usize> {
        self.positions.iter().position(|&p| p == speaker)
    }

    /// The LFE channel's index, if this bus has one.
    ///
    /// Named because it is the lookup with a consequence: the LFE is dropped
    /// from a consumer downmix and is fed by a separate bass-management send
    /// rather than a panner, so "which index is it" is asked by code that will
    /// treat that channel differently.
    pub fn lfe_index(&self) -> Option<usize> {
        self.positions.iter().position(|p| p.is_lfe())
    }

    /// Whether every position is one this vocabulary names.
    ///
    /// A caller that intends to *route* by speaker needs this: an
    /// [`Unknown`](Speaker::Unknown) channel keeps its slot, but nothing can be
    /// said about where it should go.
    pub fn is_fully_named(&self) -> bool {
        !self
            .positions
            .iter()
            .any(|p| matches!(p, Speaker::Unknown(_)))
    }

    /// The engine's own channel order for a width, or `None` for a width with
    /// no defined one.
    ///
    /// This is the SMPTE / WAV `WAVEFORMATEXTENSIBLE` order that
    /// [`crate::downmix`] already folds by and that the spatial panner already
    /// maps to — `FL FR C LFE SL SR [BL BR]`. It is written down here because
    /// it was previously implicit in a `match` on channel count in three
    /// separate places, which is a convention nothing could state or check.
    ///
    /// `None` for a width the engine has no order for — including **4**, which
    /// is genuinely ambiguous: quad `FL FR BL BR` and ambisonic B-format
    /// `W X Y Z` are both four channels and are not interchangeable. Returning
    /// a quad guess would be the [`ChannelLayout::QUAD`] ambiguity again, so
    /// the caller is made to say which it has.
    ///
    /// [`ChannelLayout::QUAD`]: crate::ChannelLayout::QUAD
    pub fn smpte(layout: ChannelLayout) -> Option<Self> {
        use Speaker::*;
        let positions: &[Speaker] = match layout.count() {
            1 => &[FrontCenter],
            2 => &[FrontLeft, FrontRight],
            6 => &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
            ],
            8 => &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
                BackLeft,
                BackRight,
            ],
            _ => return None,
        };
        Some(Self::new(positions.iter().copied()))
    }

    /// Quad — `FL FR BL BR`, four speaker feeds.
    ///
    /// Separate from [`smpte`](Self::smpte) because a bare width of 4 cannot
    /// say whether it means this or B-format; a caller that *knows* it has quad
    /// says so by naming it.
    pub fn quad() -> Self {
        use Speaker::*;
        Self::new([FrontLeft, FrontRight, BackLeft, BackRight])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The width is the list's length, and is never stored separately.
    #[test]
    fn the_layout_is_derived_from_the_position_count() {
        let t = ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight]);
        assert_eq!(t.layout(), ChannelLayout::STEREO);
        assert_eq!(t.positions().len(), 2);
    }

    /// An empty topology is a real answer (a disabled bus), not a failure.
    #[test]
    fn an_empty_topology_is_an_empty_layout() {
        let t = ChannelTopology::new([]);
        assert_eq!(t.layout(), ChannelLayout::EMPTY);
        assert_eq!(t.lfe_index(), None);
    }

    /// The engine's 5.1 order puts the LFE at index 3.
    ///
    /// Pinned as an absolute index rather than by round-tripping through
    /// `lfe_index`, which would agree with any consistent-but-wrong order. 3 is
    /// the number `downmix.rs` skips and `spatial::lfe_channel` returns.
    #[test]
    fn the_smpte_51_order_puts_lfe_at_index_three() {
        let t = ChannelTopology::smpte(ChannelLayout::from(6u16)).expect("6 has an order");
        assert_eq!(
            t.positions(),
            &[
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::FrontCenter,
                Speaker::LowFrequency,
                Speaker::SideLeft,
                Speaker::SideRight,
            ]
        );
        assert_eq!(t.lfe_index(), Some(3));
    }

    /// 7.1 keeps the LFE at 3 and appends the rear pair.
    ///
    /// The property that matters is that widening does not *move* the shared
    /// prefix — a 7.1 bus and a 5.1 bus agree channel-for-channel up to index 5.
    #[test]
    fn the_smpte_71_order_extends_51_rather_than_reshuffling_it() {
        let five_one = ChannelTopology::smpte(ChannelLayout::from(6u16)).expect("6 has an order");
        let seven_one = ChannelTopology::smpte(ChannelLayout::from(8u16)).expect("8 has an order");
        assert_eq!(&seven_one.positions()[..6], five_one.positions());
        assert_eq!(seven_one.lfe_index(), Some(3));
        assert_eq!(
            &seven_one.positions()[6..],
            &[Speaker::BackLeft, Speaker::BackRight]
        );
    }

    /// A width of 4 has no inferable order.
    ///
    /// Quad and ambisonic B-format are both 4 channels and mean different
    /// things, so `smpte` declines rather than guessing. This is the one arm
    /// most likely to be "helpfully" filled in later — it is a deliberate
    /// refusal, and `quad()` is the way to say you have quad.
    #[test]
    fn a_width_of_four_is_ambiguous_and_declines() {
        assert_eq!(ChannelTopology::smpte(ChannelLayout::QUAD), None);
        assert_eq!(ChannelTopology::quad().layout(), ChannelLayout::QUAD);
        assert_eq!(ChannelTopology::quad().lfe_index(), None);
    }

    /// Widths the engine has no order for decline rather than guessing.
    #[test]
    fn an_unhandled_width_has_no_smpte_order() {
        for width in [3u16, 5, 7, 9, 12] {
            assert_eq!(
                ChannelTopology::smpte(ChannelLayout::from(width)),
                None,
                "width {width} should have no inferred order"
            );
        }
    }

    /// An unknown position keeps its slot instead of collapsing the list.
    ///
    /// The property the `Unknown` arm exists for: the list is indexed by
    /// channel, so a dropped entry would renumber every later channel. Asserted
    /// on the *following* channel's index, since that is what a drop corrupts —
    /// a length check alone would pass if two entries were dropped and one
    /// invented.
    #[test]
    fn an_unknown_position_holds_its_channel_slot() {
        let t = ChannelTopology::new([
            Speaker::FrontLeft,
            Speaker::Unknown(4242),
            Speaker::FrontRight,
        ]);
        assert_eq!(t.layout(), ChannelLayout::from(3u16));
        assert_eq!(
            t.index_of(Speaker::FrontRight),
            Some(2),
            "the channel after an unknown one must keep its own index"
        );
        assert!(!t.is_fully_named());
    }

    /// A fully-named topology says so.
    ///
    /// The positive half of the test above: without it, an `is_fully_named`
    /// that always returned `false` would satisfy the negative assertion.
    #[test]
    fn a_named_topology_reports_itself_fully_named() {
        assert!(ChannelTopology::smpte(ChannelLayout::STEREO)
            .expect("stereo has an order")
            .is_fully_named());
    }

    /// A speaker this bus does not carry is absent, not index 0.
    #[test]
    fn a_missing_speaker_has_no_index() {
        let t = ChannelTopology::smpte(ChannelLayout::STEREO).expect("stereo has an order");
        assert_eq!(t.index_of(Speaker::LowFrequency), None);
        assert_eq!(t.index_of(Speaker::FrontLeft), Some(0));
    }

    /// A topology survives a serde round trip, including an unknown position.
    ///
    /// It rides the plugin-load IPC reply, so this is a wire path rather than a
    /// convenience. The `Unknown` payload is included deliberately: it is the
    /// one arm carrying data, so a serialization that flattened it to a unit
    /// variant would lose the raw value while still round-tripping *length*.
    #[cfg(feature = "serde")]
    #[test]
    fn a_topology_survives_a_serde_round_trip() {
        let original = ChannelTopology::new([
            Speaker::FrontLeft,
            Speaker::Unknown(4242),
            Speaker::LowFrequency,
        ]);
        let json = serde_json::to_string(&original).expect("serialize");
        let back: ChannelTopology = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, original);
        assert_eq!(back.positions()[1], Speaker::Unknown(4242));
        assert_eq!(back.lfe_index(), Some(2));
    }

    /// Only the LFE answers `is_lfe`.
    ///
    /// Guards the predicate against being widened to "any non-placed channel" —
    /// `TopCenter` and `BackCenter` are placed speakers and must not match.
    #[test]
    fn only_the_lfe_is_the_lfe() {
        assert!(Speaker::LowFrequency.is_lfe());
        for other in [
            Speaker::FrontCenter,
            Speaker::TopCenter,
            Speaker::BackCenter,
            Speaker::Unknown(3),
        ] {
            assert!(!other.is_lfe(), "{other:?} must not report as the LFE");
        }
    }
}
