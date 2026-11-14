//! `Planes` and `PlanesMut`: construction errors, windows, disjointness.

use tutti_buffers::{BufferError, Planes, PlanesMut};
use tutti_types::{ChannelLayout, Samples};

#[test]
fn planes_refuse_no_planes_as_a_zero_width() {
    let none: [&[f32]; 0] = [];
    assert_eq!(Planes::new(&none).unwrap_err(), BufferError::ZeroWidth);
    let mut none: [&mut [f32]; 0] = [];
    assert_eq!(
        PlanesMut::new(&mut none).unwrap_err(),
        BufferError::ZeroWidth
    );
}

#[test]
fn planes_refuse_a_ragged_plane_and_name_it() {
    let (a, b, c) = ([0.0f32; 4], [0.0f32; 4], [0.0f32; 3]);
    assert_eq!(
        Planes::new(&[&a, &b, &c]).unwrap_err(),
        BufferError::RaggedPlane {
            plane: 2,
            expected: Samples(4),
            found: Samples(3)
        }
    );
    let (mut a, mut b) = ([0i16; 2], [0i16; 5]);
    let mut planes = [&mut a[..], &mut b[..]];
    assert_eq!(
        PlanesMut::new(&mut planes).unwrap_err(),
        BufferError::RaggedPlane {
            plane: 1,
            expected: Samples(2),
            found: Samples(5)
        }
    );
}

#[test]
fn planes_report_width_as_the_plane_count_and_length_in_frames() {
    let data = [[0.0f32; 6]; 3];
    let slices = [&data[0][..], &data[1][..], &data[2][..]];
    let p = Planes::new(&slices).unwrap();
    assert_eq!(p.layout(), ChannelLayout::from(3u16));
    assert_eq!(p.len(), Samples(6));
    assert!(!p.is_empty());
    assert_eq!(p.iter_planes().count(), 3);
}

#[test]
fn a_plane_of_zero_frames_is_a_valid_empty_view() {
    let e: [f32; 0] = [];
    let slices = [&e[..], &e[..]];
    let p = Planes::new(&slices).unwrap();
    assert!(p.is_empty());
    assert_eq!(p.len(), Samples::ZERO);
}

#[test]
fn planes_window_applies_one_frame_range_to_every_plane() {
    let (l, r) = ([0i32, 1, 2, 3, 4], [10, 11, 12, 13, 14]);
    let slices = [&l[..], &r[..]];
    let p = Planes::new(&slices).unwrap();
    let w = p.window(Samples(1)..Samples(4));
    assert_eq!(w.len(), Samples(3));
    assert_eq!(w.plane(0).samples(), &[1, 2, 3]);
    assert_eq!(w.plane(1).samples(), &[11, 12, 13]);
    let ww = w.window(Samples(1)..Samples(2));
    assert_eq!(
        ww.plane(1).samples(),
        &[12],
        "a window of a window is relative"
    );
}

#[test]
#[should_panic]
fn planes_window_past_the_end_panics() {
    let l = [0.0f32; 3];
    let _ = Planes::new(&[&l]).unwrap().window(Samples(0)..Samples(4));
}

#[test]
fn planes_mut_yields_disjoint_writable_planes_that_land_in_their_own_slices() {
    let (mut l, mut r, mut c) = ([0.0f32; 3], [0.0f32; 3], [0.0f32; 3]);
    {
        let mut slices = [&mut l[..], &mut r[..], &mut c[..]];
        let mut p = PlanesMut::new(&mut slices).unwrap();
        assert_eq!(p.layout(), ChannelLayout::from(3u16));
        assert_eq!(p.len(), Samples(3));
        // All three are live at once, which only holds if they are disjoint.
        let mut all: Vec<_> = p.planes_mut().collect();
        for (i, plane) in all.iter_mut().enumerate() {
            plane.samples_mut().fill(i as f32 + 1.0);
        }
        drop(all);
        p.plane_mut(1).samples_mut()[2] = 9.0;
        assert_eq!(p.plane(1).samples(), &[2.0, 2.0, 9.0]);
    }
    assert_eq!(l, [1.0; 3]);
    assert_eq!(r, [2.0, 2.0, 9.0]);
    assert_eq!(c, [3.0; 3]);
}
