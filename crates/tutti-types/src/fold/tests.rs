use super::*;
use crate::fold_frame;
use Speaker::*;

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::smpte(ChannelLayout::from(n)).expect("an SMPTE width")
}

fn fold(s: &ChannelTopology, d: &ChannelTopology) -> Fold {
    Fold::between(s, d).expect("a fold")
}

/// A deterministic spread of finite samples, signs and magnitudes.
fn noise(n: usize, seed: u32) -> Vec<f32> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 3.0
        })
        .collect()
}

/// The response of output `o` to a unit impulse on each source channel.
fn response(f: &Fold, o: usize) -> Vec<f32> {
    let (s, d) = (usize::from(f.src), f.rows.len());
    (0..s)
        .map(|k| {
            let mut frame = vec![0.0; s];
            frame[k] = 1.0;
            let mut out = vec![f32::NAN; d];
            f.frame(&frame, &mut out);
            out[o]
        })
        .collect()
}

fn near(got: &[f32], want: &[f32], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: width");
    for (c, (g, w)) in got.iter().zip(want).enumerate() {
        assert!((g - w).abs() < 1e-6, "{what}: channel {c} is {g}, want {w}");
    }
}

const M: f32 = M3DB;

/// The hand-written coefficient table of the ITU-R BS.775 / Dolby stereo
/// downmix, per source width (LFE column always 0), for the left output; the
/// right is the mirror. `quad` is `FL FR BL BR`.
///
/// Mutation (run): the fold gain `M3DB` -> 0.5 fails every row; the LFE no
/// longer omitted (it falls through to `Unplaced`) fails with a refusal; a
/// missing centre dropped silently fails the centre entry.
#[test]
fn the_stereo_fold_is_the_itu_table() {
    let cases: [(ChannelTopology, [f32; 12], [f32; 12]); 4] = [
        (
            ChannelTopology::quad(),
            [1.0, 0.0, M, 0.0, 0., 0., 0., 0., 0., 0., 0., 0.],
            [0.0, 1.0, 0.0, M, 0., 0., 0., 0., 0., 0., 0., 0.],
        ),
        (
            smpte(6),
            [1.0, 0.0, M, 0.0, M, 0.0, 0., 0., 0., 0., 0., 0.],
            [0.0, 1.0, M, 0.0, 0.0, M, 0., 0., 0., 0., 0., 0.],
        ),
        (
            smpte(8),
            [1.0, 0.0, M, 0.0, M, 0.0, M, 0.0, 0., 0., 0., 0.],
            [0.0, 1.0, M, 0.0, 0.0, M, 0.0, M, 0., 0., 0., 0.],
        ),
        (
            smpte(12),
            [1.0, 0.0, M, 0.0, M, 0.0, M, 0.0, M, 0.0, M, 0.0],
            [0.0, 1.0, M, 0.0, 0.0, M, 0.0, M, 0.0, M, 0.0, M],
        ),
    ];
    for (src, left, right) in &cases {
        let w = src.positions().len();
        let f = fold(src, &smpte(2));
        near(&response(&f, 0), &left[..w], &format!("{w} -> L"));
        near(&response(&f, 1), &right[..w], &format!("{w} -> R"));
    }
}

/// Mono: one channel copies, a stereo pair averages, anything wider is the
/// stereo fold times −3 dB (the centre then lands at `2·M3DB² = 1`).
///
/// Mutation (run): the pair average's scale 0.5 -> 1.0 fails the 2 -> 1 row;
/// the wide mix's scale `M3DB` -> 0.5 fails 6 -> 1 and quad -> 1; the mix
/// reordered to `a + b * scale` fails the 0.212 figure.
#[test]
fn the_mono_fold_is_the_itu_table() {
    let mono = smpte(1);
    near(&response(&fold(&smpte(1), &mono), 0), &[1.0], "1 -> 1");
    near(&response(&fold(&smpte(2), &mono), 0), &[0.5, 0.5], "2 -> 1");
    near(
        &response(&fold(&smpte(6), &mono), 0),
        &[M, M, 1.0, 0.0, M * M, M * M],
        "6 -> 1",
    );
    near(
        &response(&fold(&ChannelTopology::quad(), &mono), 0),
        &[M, M, M * M, M * M],
        "quad -> 1",
    );
    // Not continuous across widths, as before.
    let mut out = [0.0];
    fold(&smpte(2), &mono).frame(&[0.1, 0.2], &mut out);
    assert!((out[0] - 0.15).abs() < 1e-7);
    let three = ChannelTopology::new([FrontLeft, FrontRight, FrontCenter]);
    Fold::between(&three, &mono)
        .unwrap()
        .frame(&[0.1, 0.2, 0.0], &mut out);
    assert!((out[0] - 0.212_132).abs() < 1e-6);
}

/// Invariant: for every width pair the width folds had a matrix or a copy for,
/// `Fold` writes the same bits as `fold_frame`, on frames of every sign and
/// magnitude, interleaved and planar. (The planar width fold, deleted with the
/// port of `DownmixNode`, was compared the same way before it went.)
///
/// Mutation (run), each failing this test: overhead grouping off (the 12 -> 2 and
/// 12 -> 1 pairs differ in the last bit); the mono mix reordered to
/// `a + b * scale`; the direct term moved after the folded ones; the lone-mono
/// branch off (1 -> 2 at -3 dB); the interleaved and planar lone-term `+=`
/// written `=`; the pair scale and the wide-mix scale changed.
#[test]
fn it_is_bit_identical_to_the_width_folds() {
    let widths = [1u16, 2, 4, 6, 8, 12];
    let topo = |w: u16| {
        if w == 4 {
            ChannelTopology::quad()
        } else {
            smpte(w)
        }
    };
    let mut pairs = 0;
    for &sw in &widths {
        for &dw in &widths {
            // 1 -> 6 and 1 -> 8 and 1 -> 12 put the mono in the centre (see
            // `a_lone_centre_feeds_the_centre`), and the narrowing pairs past
            // 2 had no matrix (`narrowing_below_surround_is_by_position`).
            let had_matrix = dw <= 2 || (dw >= sw && sw > 1);
            // A quad copied into a wider bus put its rears in the centre and
            // the LFE: a placement the width could not state.
            if !had_matrix || (sw == 1 && dw > 2) || (sw == 4 && dw > 2) {
                continue;
            }
            let f = fold(&topo(sw), &topo(dw));
            let (s, d) = (usize::from(sw), usize::from(dw));
            let frames = 64;
            let data = noise(s * frames, 7 + u32::from(sw) * 31 + u32::from(dw));
            let mut want = vec![0.0f32; d * frames];
            let mut got = vec![0.0f32; d * frames];
            for (a, b) in data.chunks_exact(s).zip(want.chunks_exact_mut(d)) {
                fold_frame(a, b);
            }
            f.interleaved(
                Interleaved::new(&data, ChannelLayout::from(sw)),
                InterleavedMut::new(&mut got, ChannelLayout::from(dw)),
            );
            let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&got), bits(&want), "{sw} -> {dw} interleaved");

            let planes: Vec<Vec<f32>> = (0..s)
                .map(|c| data.chunks_exact(s).map(|fr| fr[c]).collect())
                .collect();
            for c in 0..d {
                let mut a = vec![f32::NAN; frames];
                f.planar_channel(|k| &planes[k][..], c, &mut a);
                let col: Vec<f32> = want.chunks_exact(d).map(|fr| fr[c]).collect();
                assert_eq!(bits(&a), bits(&col), "{sw} -> {dw} planar vs frame {c}");
            }
            pairs += 1;
        }
    }
    assert!(pairs >= 20, "the loop skipped too much: {pairs}");
}

/// The same speakers in another order is a permutation: every output is a
/// plain copy (gain exactly 1.0), bit for bit, including negative zero, NaN
/// payloads, infinities and subnormals.
///
/// Mutation (run): the direct hit's gain 1.0 -> `M3DB` fails the bit check.
#[test]
fn the_same_speakers_in_another_order_is_a_bit_exact_permutation() {
    let film = smpte(6);
    let dts = ChannelTopology::new([
        FrontLeft,
        FrontCenter,
        FrontRight,
        SideLeft,
        SideRight,
        LowFrequency,
    ]);
    let f = fold(&film, &dts);
    assert!(f.rows.iter().all(|r| matches!(r, Row::One(Expr::Copy(_)))));
    let special = [
        -0.0f32,
        f32::from_bits(0x7fc0_1234),
        f32::INFINITY,
        f32::from_bits(1),
        1.0e-30,
        -3.5,
    ];
    let mut out = [0.0f32; 6];
    f.frame(&special, &mut out);
    let want = [
        special[0], special[2], special[1], special[4], special[5], special[3],
    ];
    assert_eq!(out.map(f32::to_bits), want.map(f32::to_bits));
    // And back again.
    let back = fold(&dts, &film);
    let mut round = [0.0f32; 6];
    back.frame(&out, &mut round);
    assert_eq!(round.map(f32::to_bits), special.map(f32::to_bits));
    // Every response is a one-hot with gain exactly 1.
    for o in 0..6 {
        let r = response(&f, o);
        assert_eq!(r.iter().filter(|&&g| g == 1.0).count(), 1);
        assert_eq!(r.iter().filter(|&&g| g == 0.0).count(), 5);
    }
}

/// A pair with no matrix before: 5.1 to quad keeps the centre out of the
/// rears and moves the surrounds across at full level.
///
/// Mutation (run): the reposition gain forced to `M3DB` fails the `1.0`
/// entries; the direct gain changed or the centre dropped when missing fails
/// the FL and FR entries.
#[test]
fn five_one_to_quad_places_by_position() {
    let f = fold(&smpte(6), &ChannelTopology::quad());
    //                 FL   FR   C    LFE  SL   SR
    near(&response(&f, 0), &[1.0, 0.0, M, 0.0, 0.0, 0.0], "FL");
    near(&response(&f, 1), &[0.0, 1.0, M, 0.0, 0.0, 0.0], "FR");
    near(&response(&f, 2), &[0.0, 0.0, 0.0, 0.0, 1.0, 0.0], "BL");
    near(&response(&f, 3), &[0.0, 0.0, 0.0, 0.0, 0.0, 1.0], "BR");
}

/// 7.1 to 5.1 sums the rears into the sides at −3 dB; 5.1 to 7.1 zero-fills the
/// rears; 7.1.4 to 7.1 folds the heights into the front pair at −3 dB.
///
/// Mutation (run): the `BackLeft` fallback reduced to the front pair fails the
/// SL entry of 7.1 -> 5.1; a destination speaker nothing feeds given channel 0
/// instead of silence (`Expr::Zero` -> `Copy(0)`) fails the zero-fill rows.
#[test]
fn narrowing_below_surround_is_by_position() {
    let f = fold(&smpte(8), &smpte(6));
    near(&response(&f, 4), &[0., 0., 0., 0., 1.0, 0.0, M, 0.0], "SL");
    near(&response(&f, 0), &[1., 0., 0., 0., 0., 0., 0., 0.], "FL");
    let up = fold(&smpte(6), &smpte(8));
    near(&response(&up, 6), &[0.0; 6], "BL is silent");
    near(&response(&up, 7), &[0.0; 6], "BR is silent");
    near(&response(&up, 5), &[0., 0., 0., 0., 0., 1.0], "SR");
    let heights = fold(&smpte(12), &smpte(8));
    let l = response(&heights, 0);
    near(
        &l,
        &[1., 0., 0., 0., 0., 0., 0., 0., M, 0., M, 0.],
        "FL with heights",
    );
}

/// The formats disagree on side versus back for four speakers (the engine's
/// quad is `FL FR BL BR`, VST3's `k40Music` and AU's quadraphonic decode to
/// `FL FR SL SR`): a fold between them is a pure reposition at gain 1.
///
/// Mutation (run): the reposition gain forced to `M3DB`, and the `BackLeft`
/// fallback reduced to the front pair, each fail this.
#[test]
fn quad_and_side_surround_quad_are_the_same_four_feeds() {
    let side_quad = ChannelTopology::new([FrontLeft, FrontRight, SideLeft, SideRight]);
    let f = fold(&ChannelTopology::quad(), &side_quad);
    for o in 0..4 {
        let r = response(&f, o);
        assert_eq!(r.iter().filter(|&&g| g == 1.0).count(), 1, "{r:?}");
        assert_eq!(r.iter().filter(|&&g| g == 0.0).count(), 3, "{r:?}");
    }
    assert_eq!(response(&f, 2), [0.0, 0.0, 1.0, 0.0]);
}

/// A mono source feeds the centre of a destination that has one, else the
/// front pair at gain 1 (never at −3 dB, never in an LFE or a surround).
///
/// Mutation (run): the lone-mono branch off makes mono -> stereo -3 dB per
/// side and fails the 1.0 entries; a silent zero row replaced by `Copy(0)`
/// fails the 5.1 channels.
#[test]
fn a_lone_centre_feeds_the_centre() {
    let mono = smpte(1);
    assert_eq!(response(&fold(&mono, &smpte(2)), 0), [1.0]);
    assert_eq!(response(&fold(&mono, &smpte(2)), 1), [1.0]);
    let six = fold(&mono, &smpte(6));
    for (o, want) in [0.0, 0.0, 1.0, 0.0, 0.0, 0.0].iter().enumerate() {
        assert_eq!(response(&six, o), [*want], "5.1 channel {o}");
    }
    let no_front = ChannelTopology::new([FrontLeft, LowFrequency]);
    assert_eq!(
        Fold::between(&mono, &no_front),
        Err(FoldError::Unplaced {
            channel: 0,
            speaker: FrontCenter
        })
    );
}

/// Discrete channels map one to one and never meet a matrix; they zero-fill
/// when widened, refuse to narrow, and relabel only at equal width.
///
/// Mutation (run): the equal-width check (`s != d`) disabled lets 6 discrete
/// into stereo through; the narrowing check (`s > d`) disabled silently drops
/// channels. Each fails here.
#[test]
fn discrete_channels_map_one_to_one() {
    let d = |n: u16| ChannelTopology::discrete(ChannelLayout::from(n));
    let f = fold(&d(6), &d(6));
    for o in 0..6 {
        let mut want = [0.0; 6];
        want[o] = 1.0;
        assert_eq!(response(&f, o), want);
    }
    let up = fold(&d(2), &d(4));
    assert_eq!(response(&up, 1), [0.0, 1.0]);
    assert_eq!(response(&up, 3), [0.0, 0.0]);
    assert_eq!(
        Fold::between(&d(4), &d(2)),
        Err(FoldError::Unplaced {
            channel: 2,
            speaker: Discrete
        })
    );
    assert_eq!(
        Fold::between(&d(6), &smpte(2)),
        Err(FoldError::Discrete {
            src: ChannelLayout::from(6u16),
            dst: ChannelLayout::STEREO
        })
    );
    assert_eq!(
        Fold::between(&smpte(2), &d(6)),
        Err(FoldError::Discrete {
            src: ChannelLayout::STEREO,
            dst: ChannelLayout::from(6u16)
        })
    );
    // Equal width relabels by index: two inputs are a stereo pair.
    let pair = fold(&d(2), &smpte(2));
    assert_eq!(response(&pair, 0), [1.0, 0.0]);
    assert_eq!(response(&pair, 1), [0.0, 1.0]);
}

/// Every refusal names what failed.
///
/// Mutation (run): the empty, unnamed and duplicate checks each disabled fail
/// their own assertion.
#[test]
fn what_cannot_be_placed_is_an_error() {
    let empty = ChannelTopology::new([]);
    assert_eq!(
        Fold::between(&empty, &smpte(2)),
        Err(FoldError::Empty(FoldEnd::Source))
    );
    assert_eq!(
        Fold::between(&smpte(2), &empty),
        Err(FoldError::Empty(FoldEnd::Destination))
    );
    let odd = ChannelTopology::new([FrontLeft, Unknown(9)]);
    assert_eq!(
        Fold::between(&odd, &smpte(2)),
        Err(FoldError::Unnamed {
            end: FoldEnd::Source,
            channel: 1
        })
    );
    assert_eq!(
        Fold::between(&smpte(2), &odd),
        Err(FoldError::Unnamed {
            end: FoldEnd::Destination,
            channel: 1
        })
    );
    // Identical topologies are an identity even when a channel is unknown.
    assert!(Fold::between(&odd, &odd).is_ok());
    let twice = ChannelTopology::new([FrontLeft, FrontLeft]);
    assert_eq!(
        Fold::between(&twice, &smpte(2)),
        Err(FoldError::Duplicate {
            end: FoldEnd::Source,
            speaker: FrontLeft
        })
    );
    assert_eq!(
        Fold::between(&smpte(2), &twice),
        Err(FoldError::Duplicate {
            end: FoldEnd::Destination,
            speaker: FrontLeft
        })
    );
    let centre_lfe = ChannelTopology::new([FrontCenter, LowFrequency]);
    assert_eq!(
        Fold::between(&smpte(2), &centre_lfe),
        Err(FoldError::Unplaced {
            channel: 0,
            speaker: FrontLeft
        })
    );
}

/// `assuming_smpte` is the width convenience; 4 has no order, and its error
/// says which width.
///
/// Mutation (run): a `4 => FL FR BL BR` arm in `smpte` fails the refusal here
/// and `a_width_of_four_is_ambiguous_and_declines`.
#[test]
fn assuming_smpte_declines_what_a_width_cannot_say() {
    let four = ChannelLayout::QUAD;
    assert_eq!(
        Fold::assuming_smpte(four, ChannelLayout::STEREO),
        Err(FoldError::NoSmpteOrder(four))
    );
    assert_eq!(
        Fold::assuming_smpte(ChannelLayout::from(6u16), ChannelLayout::STEREO).unwrap(),
        fold(&smpte(6), &smpte(2))
    );
    assert_eq!(
        Fold::assuming_smpte(ChannelLayout::from(3u16), ChannelLayout::STEREO),
        Err(FoldError::NoSmpteOrder(ChannelLayout::from(3u16)))
    );
}

/// Every pair of common topologies either refuses or places every non-LFE
/// source channel: an impulse on it reaches some output, and nothing is NaN.
///
/// Mutation (run): a missing centre omitted like the LFE leaves the 5.1 centre
/// unreached and fails; the LFE no longer omitted turns narrowing pairs into
/// refusals and fails `built`.
#[test]
fn no_source_channel_is_silently_dropped() {
    let named = [
        ("mono", smpte(1)),
        ("stereo", smpte(2)),
        (
            "lcr",
            ChannelTopology::new([FrontLeft, FrontRight, FrontCenter]),
        ),
        ("quad", ChannelTopology::quad()),
        (
            "5.0",
            ChannelTopology::new([FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight]),
        ),
        ("5.1", smpte(6)),
        (
            "6.1",
            ChannelTopology::new([
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
                BackCenter,
            ]),
        ),
        ("7.1", smpte(8)),
        ("7.1.4", smpte(12)),
    ];
    let mut built = 0;
    for (sn, s) in &named {
        for (dn, d) in &named {
            let Ok(f) = Fold::between(s, d) else {
                // Only a destination with no home for a source speaker refuses.
                assert!(
                    matches!(Fold::between(s, d), Err(FoldError::Unplaced { .. })),
                    "{sn} -> {dn}"
                );
                continue;
            };
            built += 1;
            for (k, sp) in s.positions().iter().enumerate() {
                if sp.is_lfe() && d.lfe_index().is_none() {
                    continue;
                }
                let reached = (0..d.positions().len()).any(|o| response(&f, o)[k] != 0.0);
                assert!(reached, "{sn} -> {dn}: channel {k} ({sp:?}) vanished");
            }
        }
    }
    assert!(built >= 70, "built {built}");
}

/// Planar and block-interleaved output are the frame output, bit for bit, for
/// every built pair, so a node and an edge cannot disagree.
///
/// Mutation (run): `planar_to_interleaved` writing every chunk at frame 0, or
/// adding 1.0 to each sample, fails; the planar lone-term and group-term `+=` written `=`, the
/// interleaved lone-term `+=` written `=`, and the mono mix reordered each fail.
#[test]
fn planar_is_the_frame_fold() {
    let all = [
        smpte(1),
        smpte(2),
        ChannelTopology::quad(),
        smpte(6),
        smpte(8),
        smpte(12),
    ];
    for s in &all {
        for d in &all {
            let Ok(f) = Fold::between(s, d) else { continue };
            let (sw, dw) = (s.positions().len(), d.positions().len());
            let frames = 137;
            let data = noise(sw * frames, 11);
            let planes: Vec<Vec<f32>> = (0..sw)
                .map(|c| data.chunks_exact(sw).map(|fr| fr[c]).collect())
                .collect();
            let mut out = vec![0.0; dw];
            for c in 0..dw {
                let mut plane = vec![f32::NAN; frames];
                f.planar_channel(|k| &planes[k][..], c, &mut plane);
                for (i, fr) in data.chunks_exact(sw).enumerate() {
                    f.frame(fr, &mut out);
                    assert_eq!(plane[i].to_bits(), out[c].to_bits(), "{sw}->{dw} c{c} f{i}");
                }
            }
            let mut device = vec![f32::NAN; dw * frames];
            f.planar_to_interleaved(
                |k| &planes[k][..],
                InterleavedMut::new(&mut device, d.layout()),
            );
            for (i, fr) in data.chunks_exact(sw).enumerate() {
                f.frame(fr, &mut out);
                let got = &device[i * dw..(i + 1) * dw];
                assert_eq!(
                    got.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                    out.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                    "{sw}->{dw} device frame {i}"
                );
            }
            let mut block = vec![f32::NAN; dw * frames];
            f.interleaved(
                Interleaved::new(&data, s.layout()),
                InterleavedMut::new(&mut block, d.layout()),
            );
            for (i, fr) in data.chunks_exact(sw).enumerate() {
                f.frame(fr, &mut out);
                let got = &block[i * dw..(i + 1) * dw];
                assert_eq!(
                    got.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                    out.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                    "{sw}->{dw} block frame {i}"
                );
            }
        }
    }
}

#[test]
#[should_panic(expected = "destination width")]
fn the_block_fold_checks_widths() {
    let f = fold(&smpte(6), &smpte(2));
    let src = [0.0f32; 12];
    let mut dst = [0.0f32; 6];
    f.interleaved(
        Interleaved::new(&src, ChannelLayout::from(6u16)),
        InterleavedMut::new(&mut dst, ChannelLayout::from(3u16)),
    );
}
