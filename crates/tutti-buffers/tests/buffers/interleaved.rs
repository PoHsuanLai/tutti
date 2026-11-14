//! The views' construction errors, counts and sub-views.

use tutti_buffers::{BufferError, I24, Interleaved, InterleavedMut, Mono, MonoMut, Sample};
use tutti_types::{ChannelLayout, Samples};

#[test]
fn interleaved_refuses_a_zero_width() {
    let data = [0.0f32; 4];
    assert_eq!(
        Interleaved::new(&data, ChannelLayout::EMPTY).unwrap_err(),
        BufferError::ZeroWidth
    );
    let mut data = [0.0f32; 4];
    assert_eq!(
        InterleavedMut::new(&mut data, ChannelLayout::EMPTY).unwrap_err(),
        BufferError::ZeroWidth
    );
}

#[test]
fn interleaved_refuses_a_length_that_is_not_whole_frames_and_does_not_truncate() {
    let data = [0.0f32; 5];
    assert_eq!(
        Interleaved::new(&data, ChannelLayout::STEREO).unwrap_err(),
        BufferError::PartialFrame {
            len: 5,
            width: ChannelLayout::STEREO
        }
    );
    let mut data = [0i16; 7];
    assert_eq!(
        InterleavedMut::new(&mut data, ChannelLayout::from(3u16)).unwrap_err(),
        BufferError::PartialFrame {
            len: 7,
            width: ChannelLayout::from(3u16)
        }
    );
}

#[test]
fn interleaved_counts_frames_not_samples() {
    let data = [0.0f32; 12];
    let buf = Interleaved::new(&data, ChannelLayout::from(3u16)).unwrap();
    assert_eq!(buf.len(), Samples(4));
    assert_eq!(buf.stride(), 3);
    assert_eq!(buf.samples().len(), 12);
    assert_eq!(buf.layout(), ChannelLayout::from(3u16));
    assert!(!buf.is_empty());
}

#[test]
fn an_empty_slice_is_a_valid_empty_interleaved_view() {
    let buf = Interleaved::<f32>::new(&[], ChannelLayout::STEREO).unwrap();
    assert!(buf.is_empty());
    assert_eq!(buf.len(), Samples::ZERO);
    assert_eq!(buf.iter_frames().count(), 0);
}

#[test]
fn interleaved_window_is_denominated_in_frames() {
    let data: Vec<i16> = (0..12).collect();
    let buf = Interleaved::new(&data, ChannelLayout::from(3u16)).unwrap();
    let w = buf.window(Samples(1)..Samples(3));
    assert_eq!(w.len(), Samples(2));
    assert_eq!(w.samples(), &[3, 4, 5, 6, 7, 8]);
    assert_eq!(w.layout(), buf.layout());
}

#[test]
fn interleaved_frame_and_iter_frames_yield_stride_long_frames() {
    let data = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
    let buf = Interleaved::new(&data, ChannelLayout::STEREO).unwrap();
    assert_eq!(buf.frame(Samples(2)), &[5.0, 6.0]);
    let frames: Vec<&[f32]> = buf.iter_frames().collect();
    assert_eq!(frames, [&[1.0, 2.0][..], &[3.0, 4.0], &[5.0, 6.0]]);
}

#[test]
#[should_panic]
fn interleaved_frame_past_the_end_panics() {
    let data = [0.0f32; 4];
    let _ = Interleaved::new(&data, ChannelLayout::STEREO)
        .unwrap()
        .frame(Samples(2));
}

#[test]
fn interleaved_mut_writes_frames_windows_and_reborrows() {
    let mut data = [0i32; 8];
    let mut buf = InterleavedMut::new(&mut data, ChannelLayout::from(4u16)).unwrap();
    buf.frame_mut(Samples(1)).copy_from_slice(&[1, 2, 3, 4]);
    {
        let mut w = buf.window_mut(Samples(0)..Samples(1));
        assert_eq!(w.len(), Samples(1));
        w.frame_mut(Samples(0)).fill(9);
    }
    for f in buf.reborrow().iter_frames_mut() {
        f[3] += 100;
    }
    assert_eq!(buf.as_ref().samples(), &[9, 9, 9, 109, 1, 2, 3, 104]);
    assert_eq!(buf.samples_mut().len(), 8);
}

#[test]
fn interleaved_views_hold_every_storage_format() {
    let d = [I24::of(1), I24::of(-2)];
    assert_eq!(
        Interleaved::new(&d, ChannelLayout::MONO).unwrap().len(),
        Samples(2)
    );
    let d = [0.5f64; 4];
    assert_eq!(
        Interleaved::new(&d, ChannelLayout::STEREO).unwrap().len(),
        Samples(2)
    );
    assert_eq!(<i8 as Sample>::SILENCE, 0);
}

#[test]
fn mono_reads_as_a_slice_and_windows_and_splits_by_frames() {
    let data = [1i16, 2, 3, 4, 5];
    let m = Mono::new(&data);
    assert_eq!(m.len(), Samples(5));
    assert_eq!(m.as_ref(), &data[..]);
    assert_eq!(m.window(Samples(1)..Samples(4)).samples(), &[2, 3, 4]);
    let (a, b) = m.split_at(Samples(2));
    assert_eq!((a.samples(), b.samples()), (&[1, 2][..], &[3, 4, 5][..]));
    assert!(Mono::<f32>::new(&[]).is_empty());
}

#[test]
fn mono_view_is_the_size_of_a_slice() {
    assert_eq!(size_of::<Mono<'_, f32>>(), size_of::<&[f32]>());
    assert_eq!(size_of::<MonoMut<'_, i16>>(), size_of::<&mut [i16]>());
}

#[test]
fn mono_mut_split_yields_disjoint_writable_halves() {
    let mut data = [0.0f32; 4];
    let mut m = MonoMut::new(&mut data);
    let (mut a, mut b) = m.split_at_mut(Samples(1));
    a.samples_mut().fill(1.0);
    b.samples_mut().fill(2.0);
    assert_eq!(m.len(), Samples(4));
    m.window_mut(Samples(3)..Samples(4)).samples_mut()[0] = 3.0;
    assert_eq!(m.as_ref().samples(), &[1.0, 2.0, 2.0, 3.0]);
    assert_eq!(m.reborrow().len(), Samples(4));
}
