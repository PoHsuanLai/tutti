//! `convert`, `interleave` and `deinterleave`: figures and mismatch errors.

use tutti_buffers::{
    BufferError, Interleaved, InterleavedMut, Planes, PlanesMut, convert, deinterleave, interleave,
};
use tutti_types::{ChannelLayout, Samples};

const ST: ChannelLayout = ChannelLayout::STEREO;

#[test]
fn convert_applies_the_sample_rule_to_every_sample() {
    let src = [0.5f32, -1.0, 2.0, f32::NAN, 0.0, -2.0];
    let mut dst = [7i16; 6];
    convert(
        Interleaved::new(&src, ST).unwrap(),
        InterleavedMut::new(&mut dst, ST).unwrap(),
    )
    .unwrap();
    assert_eq!(dst, [16384, -32768, 32767, 0, 0, -32768]);
}

#[test]
fn convert_int_to_float_and_back_restores_the_integers() {
    let pcm = [i16::MIN, -1, 0, 1, 12345, i16::MAX];
    let mut f = [0.0f32; 6];
    let mut back = [0i16; 6];
    let l = ChannelLayout::from(3u16);
    convert(
        Interleaved::new(&pcm, l).unwrap(),
        InterleavedMut::new(&mut f, l).unwrap(),
    )
    .unwrap();
    assert_eq!(f[0], -1.0);
    convert(
        Interleaved::new(&f, l).unwrap(),
        InterleavedMut::new(&mut back, l).unwrap(),
    )
    .unwrap();
    assert_eq!(back, pcm);
}

#[test]
fn convert_refuses_a_width_mismatch_and_writes_nothing() {
    let src = [0.5f32; 4];
    let mut dst = [7i16; 4];
    let err = convert(
        Interleaved::new(&src, ST).unwrap(),
        InterleavedMut::new(&mut dst, ChannelLayout::MONO).unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BufferError::WidthMismatch {
            src: ST,
            dst: ChannelLayout::MONO
        }
    );
    assert_eq!(dst, [7; 4]);
}

#[test]
fn convert_refuses_a_length_mismatch_instead_of_copying_short() {
    let src = [0.5f32; 6];
    let mut dst = [7i16; 4];
    let err = convert(
        Interleaved::new(&src, ST).unwrap(),
        InterleavedMut::new(&mut dst, ST).unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BufferError::LengthMismatch {
            src: Samples(3),
            dst: Samples(2)
        }
    );
    assert_eq!(dst, [7; 4]);
}

#[test]
fn interleave_places_frame_i_channel_c_at_i_times_stride_plus_c() {
    let (a, b, c) = ([1i16, 2], [3, 4], [5, 6]);
    let mut out = [0i16; 6];
    interleave(
        Planes::new(&[&a, &b, &c]).unwrap(),
        InterleavedMut::new(&mut out, ChannelLayout::from(3u16)).unwrap(),
    )
    .unwrap();
    assert_eq!(out, [1, 3, 5, 2, 4, 6]);
}

#[test]
fn deinterleave_places_frame_i_channel_c_in_plane_c_at_i() {
    let src = [1.0f64, 3.0, 5.0, 2.0, 4.0, 6.0];
    let (mut a, mut b, mut c) = ([0.0; 2], [0.0; 2], [0.0; 2]);
    let mut slices = [&mut a[..], &mut b[..], &mut c[..]];
    deinterleave(
        Interleaved::new(&src, ChannelLayout::from(3u16)).unwrap(),
        PlanesMut::new(&mut slices).unwrap(),
    )
    .unwrap();
    assert_eq!((a, b, c), ([1.0, 2.0], [3.0, 4.0], [5.0, 6.0]));
}

#[test]
fn interleave_of_a_window_writes_only_that_window() {
    let (l, r) = ([1i32, 2, 3, 4], [5, 6, 7, 8]);
    let slices = [&l[..], &r[..]];
    let p = Planes::new(&slices).unwrap().window(Samples(1)..Samples(3));
    let mut out = [0i32; 4];
    interleave(p, InterleavedMut::new(&mut out, ST).unwrap()).unwrap();
    assert_eq!(out, [2, 6, 3, 7]);
}

#[test]
fn interleave_refuses_mismatches_and_writes_nothing() {
    let (l, r) = ([1.0f32; 3], [2.0f32; 3]);
    let mut out = [9.0f32; 4];
    assert_eq!(
        interleave(
            Planes::new(&[&l, &r]).unwrap(),
            InterleavedMut::new(&mut out, ST).unwrap()
        ),
        Err(BufferError::LengthMismatch {
            src: Samples(3),
            dst: Samples(2)
        })
    );
    assert_eq!(
        interleave(
            Planes::new(&[&l]).unwrap(),
            InterleavedMut::new(&mut out, ST).unwrap()
        ),
        Err(BufferError::WidthMismatch {
            src: ChannelLayout::MONO,
            dst: ST
        })
    );
    assert_eq!(out, [9.0; 4]);
}

#[test]
fn deinterleave_refuses_mismatches_and_writes_nothing() {
    let src = [1.0f32; 6];
    let (mut a, mut b) = ([9.0f32; 2], [9.0f32; 2]);
    let mut slices = [&mut a[..], &mut b[..]];
    assert_eq!(
        deinterleave(
            Interleaved::new(&src, ST).unwrap(),
            PlanesMut::new(&mut slices).unwrap()
        ),
        Err(BufferError::LengthMismatch {
            src: Samples(3),
            dst: Samples(2)
        })
    );
    let mut slices = [&mut a[..]];
    assert_eq!(
        deinterleave(
            Interleaved::new(&src[..2], ST).unwrap(),
            PlanesMut::new(&mut slices).unwrap()
        ),
        Err(BufferError::WidthMismatch {
            src: ST,
            dst: ChannelLayout::MONO
        })
    );
    assert_eq!((a, b), ([9.0; 2], [9.0; 2]));
}

#[test]
fn interleave_then_deinterleave_returns_the_planes() {
    let (l, r) = ([0.1f32, 0.2, 0.3], [-0.1, -0.2, -0.3]);
    let mut flat = [0.0f32; 6];
    interleave(
        Planes::new(&[&l, &r]).unwrap(),
        InterleavedMut::new(&mut flat, ST).unwrap(),
    )
    .unwrap();
    let (mut l2, mut r2) = ([0.0f32; 3], [0.0f32; 3]);
    let mut slices = [&mut l2[..], &mut r2[..]];
    deinterleave(
        Interleaved::new(&flat, ST).unwrap(),
        PlanesMut::new(&mut slices).unwrap(),
    )
    .unwrap();
    assert_eq!((l2, r2), (l, r));
}

#[test]
fn buffer_errors_say_what_was_refused() {
    assert!(
        BufferError::ZeroWidth
            .to_string()
            .contains("at least one channel")
    );
    let e = BufferError::PartialFrame { len: 5, width: ST };
    assert!(e.to_string().contains("5 samples") && e.to_string().contains("Stereo"));
    let e = BufferError::TooWide { channels: 70_000 };
    assert!(e.to_string().contains("70000"));
}
