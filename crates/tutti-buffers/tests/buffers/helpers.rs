//! The views' reuse helpers: `deinterleave_into`, `append_to_planes`,
//! `interleave_from` and `fold_into`, and `StereoPlanes`.

use tutti_buffers::{BufferError, Fold, Interleaved, InterleavedMut, M3DB, StereoPlanes};
use tutti_types::{ChannelLayout, Samples};

#[test]
fn deinterleave_into_clears_reused_planes_and_splits_channels() {
    let buf = [1.0f32, -1.0, 2.0, -2.0, 3.0, -3.0];
    let it = Interleaved::new(&buf, ChannelLayout::STEREO).unwrap();
    let mut planes = vec![vec![99.0f32; 7], vec![99.0f32; 7]];
    it.deinterleave_into(&mut planes);
    assert_eq!(planes[0], [1.0, 2.0, 3.0]);
    assert_eq!(planes[1], [-1.0, -2.0, -3.0]);
}

/// Stale audio in an unused channel is silent and wrong.
#[test]
fn deinterleave_into_empties_planes_beyond_the_width() {
    let buf = [1.0f32, 2.0];
    let it = Interleaved::new(&buf, ChannelLayout::STEREO).unwrap();
    let mut planes = vec![vec![99.0f32], vec![99.0f32], vec![99.0f32; 4]];
    it.deinterleave_into(&mut planes);
    assert_eq!(planes[0], [1.0]);
    assert_eq!(planes[1], [2.0]);
    assert!(planes[2].is_empty(), "the third plane keeps no old data");
}

/// A render collected block by block is the concatenation of its blocks,
/// channel by channel.
#[test]
fn append_to_planes_concatenates_blocks_per_channel() {
    let mut planes = vec![Vec::new(), Vec::new()];
    let a = [1.0f32, -1.0, 2.0, -2.0];
    let b = [3.0f32, -3.0];
    Interleaved::new(&a, ChannelLayout::STEREO)
        .unwrap()
        .append_to_planes(&mut planes);
    Interleaved::new(&b, ChannelLayout::STEREO)
        .unwrap()
        .append_to_planes(&mut planes);
    assert_eq!(planes[0], [1.0, 2.0, 3.0]);
    assert_eq!(planes[1], [-1.0, -2.0, -3.0]);
}

/// Every sample is written from its plane at `start`; a missing plane and the
/// frames past a short plane are silence, in any format.
#[test]
fn interleave_from_writes_every_sample_and_silences_what_no_plane_reaches() {
    let planes = [vec![10i16, 11, 12, 13], vec![20, 21, 22]];
    let mut buf = [7i16; 9];
    InterleavedMut::new(&mut buf, ChannelLayout::from(3u16))
        .unwrap()
        .interleave_from(&planes, Samples(1));
    assert_eq!(buf, [11, 21, 0, 12, 22, 0, 13, 0, 0]);
}

#[test]
fn interleave_from_inverts_deinterleave_into_at_every_width() {
    for width in 1u16..=8 {
        let layout = ChannelLayout::from(width);
        let data: Vec<f32> = (0..usize::from(width) * 37).map(|i| i as f32).collect();
        let mut planes = vec![Vec::new(); usize::from(width)];
        Interleaved::new(&data, layout)
            .unwrap()
            .deinterleave_into(&mut planes);
        let mut back = vec![f32::NAN; data.len()];
        InterleavedMut::new(&mut back, layout)
            .unwrap()
            .interleave_from(&planes, Samples::ZERO);
        assert_eq!(back, data, "width {width}");
    }
}

/// The analytic figures: a mono fold averages two channels and is
/// `(Lo + Ro) * M3DB` from three up; `out` is overwritten, not appended to.
#[test]
fn fold_into_writes_each_frame_through_the_fold_at_each_width() {
    let buf: Vec<f32> = (0..24).map(|i| i as f32 * 0.01).collect();
    let to_mono = |w: u16| Fold::widths(ChannelLayout::from(w), ChannelLayout::MONO).unwrap();

    let mut out = vec![99.0f32; 3];
    Interleaved::new(&buf, ChannelLayout::MONO)
        .unwrap()
        .fold_into(&to_mono(1), &mut out);
    assert_eq!(out, buf, "width 1 copies over a dirtied `out`");
    Interleaved::new(&buf, ChannelLayout::STEREO)
        .unwrap()
        .fold_into(&to_mono(2), &mut out);
    assert_eq!(out.len(), 12);
    assert_eq!(out[0], (buf[0] + buf[1]) * 0.5);
    assert_eq!(out[11], (buf[22] + buf[23]) * 0.5);

    // Quad (FL FR BL BR): Lo = FL + M3DB * BL, Ro = FR + M3DB * BR.
    Interleaved::new(&buf, ChannelLayout::QUAD)
        .unwrap()
        .fold_into(&to_mono(4), &mut out);
    assert_eq!(out.len(), 6);
    let (lo, ro) = (buf[0] + M3DB * buf[2], buf[1] + M3DB * buf[3]);
    assert_eq!(out[0], (lo + ro) * M3DB);

    let five = [0.0f32, 0.0, 1.0, 0.0, 0.0, 0.0];
    let stereo = Fold::widths(ChannelLayout::from(6u16), ChannelLayout::STEREO).unwrap();
    Interleaved::new(&five, ChannelLayout::from(6u16))
        .unwrap()
        .fold_into(&stereo, &mut out);
    assert_eq!(out, [M3DB, M3DB], "a centre-only 5.1 reaches both sides");
}

#[test]
fn folding_a_window_folds_only_its_frames() {
    let buf = [1.0f32, 1.0, 2.0, 2.0, 3.0, 3.0];
    let it = Interleaved::new(&buf, ChannelLayout::STEREO).unwrap();
    let fold = Fold::widths(ChannelLayout::STEREO, ChannelLayout::MONO).unwrap();
    let mut out = Vec::new();
    it.window(Samples(1)..Samples(3)).fold_into(&fold, &mut out);
    assert_eq!(out, [2.0, 3.0]);
}

#[test]
#[should_panic(expected = "the fold reads another width")]
fn fold_into_refuses_a_fold_of_another_width() {
    let buf = [0.0f32; 4];
    let fold = Fold::widths(ChannelLayout::MONO, ChannelLayout::STEREO).unwrap();
    Interleaved::new(&buf, ChannelLayout::STEREO)
        .unwrap()
        .fold_into(&fold, &mut Vec::new());
}

#[test]
fn stereo_planes_refuse_a_length_mismatch_naming_the_right_plane() {
    let err = StereoPlanes::new(&[0.0f32; 3], &[0.0; 2]).unwrap_err();
    assert_eq!(
        err,
        BufferError::RaggedPlane {
            plane: 1,
            expected: Samples(3),
            found: Samples(2),
        }
    );
    let empty: [f32; 0] = [];
    assert!(StereoPlanes::new(&empty, &empty).unwrap().is_empty());
}
