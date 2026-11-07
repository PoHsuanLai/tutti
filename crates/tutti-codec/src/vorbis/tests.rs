use super::*;
use std::io::{Cursor, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_types::SampleRate;

/// A sink that is slow to write and says when it is dropped.
struct Watched {
    inner: Cursor<Vec<u8>>,
    dropped: Arc<AtomicBool>,
}

impl Write for Watched {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::thread::sleep(std::time::Duration::from_millis(15));
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Watched {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Dropping the writer stops its thread before `drop` returns, so the sink (a
/// file, in a staged encoder) is closed by then. The sink is slow, so the
/// thread is still working through its queue when the drop starts.
///
/// Mutation (run): deleting the `Drop` impl returns from `drop` with the thread
/// still encoding, the sink not yet dropped, and fails the assertion.
#[test]
fn a_dropped_writer_has_stopped_its_thread() {
    let dropped = Arc::new(AtomicBool::new(false));
    let spec = Spec::new(
        crate::Format::Vorbis {
            quality: Quality::LOWEST,
        },
        ChannelLayout::STEREO,
        SampleRate(44_100.0),
    );
    let sink = Watched {
        inner: Cursor::new(Vec::new()),
        dropped: Arc::clone(&dropped),
    };
    let mut w = VorbisWriter::create(&spec, Quality::LOWEST, sink).unwrap();
    let samples: Vec<f32> = (0..8_192 * 2)
        .map(|i| ((i * 7) % 255) as f32 / 255.0 - 0.5)
        .collect();
    for _ in 0..4 {
        w.write(Interleaved::new(&samples, ChannelLayout::STEREO))
            .unwrap();
    }
    drop(w);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the thread outlived the drop"
    );
}

fn vorbis_spec(width: u16, topology: Option<ChannelTopology>) -> Spec {
    Spec {
        topology,
        ..Spec::new(
            crate::Format::Vorbis {
                quality: Quality::HIGHEST,
            },
            ChannelLayout::from(width),
            SampleRate(44_100.0),
        )
    }
}

fn names(speakers: &[Speaker]) -> ChannelTopology {
    ChannelTopology::new(speakers.iter().copied())
}

/// Stream channel `i` of the engine's 7.1 (`FL FR FC LFE SL SR BL BR`) is
/// Vorbis I's `FL FC FR SL SR RL RR LFE`, and 5.1 (`FL FR FC LFE SL SR`) is
/// `FL FC FR RL RR LFE` with the sides standing for the rears. Pinned as the
/// permutation itself, against the table in the specification (4.3.9).
///
/// Mutation (run): `FrontRight` and `FrontCenter` exchanged in the 3-channel
/// row of `vorbis_speakers` fails the 3-channel round trip and this test;
/// matching stand-ins before exact speakers (`[true, false]`) fails this test,
/// the next and the round trip.
#[test]
fn the_engines_order_is_permuted_to_vorbis_i() {
    use Speaker::*;
    let eight = vorbis_order(&vorbis_spec(8, None)).unwrap().unwrap();
    assert_eq!(&*eight, &[0, 2, 1, 4, 5, 6, 7, 3]);
    let six = vorbis_order(&vorbis_spec(6, None)).unwrap().unwrap();
    assert_eq!(&*six, &[0, 2, 1, 4, 5, 3]);
    // Already in Vorbis's order: nothing to do.
    let own = names(&[FrontLeft, FrontCenter, FrontRight]);
    assert!(vorbis_order(&vorbis_spec(3, Some(own))).unwrap().is_none());
}

/// A side takes a side and a rear a rear whenever the file has them; the
/// stand-in is only for the speaker that is missing. A file with both pairs
/// and no `LFE` still cannot be placed.
///
/// Mutation (run): matching stand-ins in the first pass puts the engine's
/// `SL SR` on Vorbis's `RL RR` slots of 7.1 and fails the first assertion; an
/// unplaced slot falling back to its own index fails the last one.
#[test]
fn a_rear_stands_in_only_for_a_missing_side() {
    use Speaker::*;
    let t = names(&[
        FrontLeft,
        FrontRight,
        FrontCenter,
        LowFrequency,
        BackLeft,
        BackRight,
        SideLeft,
        SideRight,
    ]);
    let order = vorbis_order(&vorbis_spec(8, Some(t))).unwrap().unwrap();
    // Vorbis: FL FC FR SL SR RL RR LFE.
    assert_eq!(&*order, &[0, 2, 1, 6, 7, 4, 5, 3]);
    let no_lfe = names(&[
        FrontLeft,
        FrontRight,
        FrontCenter,
        BackLeft,
        BackRight,
        SideLeft,
        SideRight,
        SideRight,
    ]);
    assert!(vorbis_order(&vorbis_spec(8, Some(no_lfe))).is_err());
}

/// A width Vorbis defines is refused when the file cannot say which speakers it
/// holds (3, 4, 5 and 7 have no engine default), or says speakers that are not
/// Vorbis's layout; the topology of another width is refused as for WAV.
///
/// Mutation (run): treating a missing topology as the identity order
/// (`return Ok(None)`) fails the first loop; an unplaced slot falling back to
/// its own index fails the discrete row.
#[test]
fn a_width_with_no_vorbis_layout_is_refused() {
    for width in [3u16, 4, 5, 7] {
        let err = vorbis_order(&vorbis_spec(width, None)).unwrap_err();
        assert!(
            matches!(err, Error::Spec(SpecError::VorbisLayout { channels, .. }) if channels == width),
            "{width}: {err}"
        );
    }
    let discrete = ChannelTopology::discrete(ChannelLayout::from(6u16));
    assert!(matches!(
        vorbis_order(&vorbis_spec(6, Some(discrete))),
        Err(Error::Spec(SpecError::VorbisLayout { channels: 6, .. }))
    ));
    assert!(matches!(
        vorbis_order(&vorbis_spec(6, Some(ChannelTopology::quad()))),
        Err(Error::Spec(SpecError::TopologyWidth { .. }))
    ));
}

/// Mono, stereo and a width past 8 (whose order the application defines) are
/// written as they come, with or without a topology.
#[test]
fn mono_stereo_and_past_eight_are_not_permuted() {
    for width in [1u16, 2, 9, 12] {
        assert!(vorbis_order(&vorbis_spec(width, None)).unwrap().is_none());
    }
    let t = ChannelTopology::discrete(ChannelLayout::from(10u16));
    assert!(vorbis_order(&vorbis_spec(10, Some(t))).unwrap().is_none());
}

#[cfg(feature = "decode-ogg")]
mod round_trip {
    use super::*;

    /// Level of source channel `c`: each 1.5 times the last, so a channel on
    /// another plane is off by at least 1.5, past the 25% the codec's loss
    /// (and libvorbis's gain on a bass channel) moves a level.
    fn level(c: usize) -> f32 {
        0.03 * 1.5f32.powi(c as i32)
    }

    /// One row: the file's channels (`None` is the engine's default order) and
    /// the speaker each decoded plane is (symphonia places a Vorbis stream by
    /// its channel bits: `FL FR FC LFE RC RL RR SL SR`).
    struct Case {
        source: Option<&'static [Speaker]>,
        width: u16,
        planes: &'static [Speaker],
    }

    fn rms(plane: &[f32]) -> f32 {
        // Past the codec's priming and short of the final packet; 7 350
        // frames is five cycles of the 30 Hz tone, so its RMS is not
        // skewed by a part cycle.
        let body = &plane[4_096..4_096 + 7_350];
        (body.iter().map(|x| x * x).sum::<f32>() / body.len() as f32).sqrt()
    }

    /// Encodes a sine of `level(c)` on file channel `c`, reads the Ogg back
    /// with `Wave::read`, and asserts each decoded plane carries the level of
    /// the file channel for the speaker that plane is.
    ///
    /// Mutation (run): `order` forced to `None` in `create` (the old writer)
    /// fails this test (the 3-channel row first; the quad row has the same
    /// order in both and would pass alone, by design); the `FC`/`FR` exchange in
    /// the 3-channel table fails it too.
    #[test]
    fn each_channel_lands_on_its_own_plane() {
        use Speaker::*;
        let cases = [
            Case {
                source: Some(&[FrontRight, FrontLeft, FrontCenter]),
                width: 3,
                planes: &[FrontLeft, FrontRight, FrontCenter],
            },
            Case {
                source: Some(&[FrontLeft, FrontRight, BackLeft, BackRight]),
                width: 4,
                planes: &[FrontLeft, FrontRight, BackLeft, BackRight],
            },
            Case {
                source: Some(&[BackRight, FrontCenter, FrontLeft, BackLeft, FrontRight]),
                width: 5,
                planes: &[FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight],
            },
            Case {
                source: None,
                width: 6,
                planes: &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    LowFrequency,
                    SideLeft,
                    SideRight,
                ],
            },
            Case {
                source: Some(&[
                    LowFrequency,
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    SideLeft,
                    SideRight,
                    BackCenter,
                ]),
                width: 7,
                planes: &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    LowFrequency,
                    BackCenter,
                    SideLeft,
                    SideRight,
                ],
            },
            Case {
                source: None,
                width: 8,
                planes: &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    LowFrequency,
                    BackLeft,
                    BackRight,
                    SideLeft,
                    SideRight,
                ],
            },
        ];
        for case in cases {
            let topology = case.source.map(names);
            let spec = vorbis_spec(case.width, topology.clone());
            let source: Vec<Speaker> = match case.source {
                Some(s) => s.to_vec(),
                None => ChannelTopology::smpte(spec.layout)
                    .unwrap()
                    .positions()
                    .to_vec(),
            };
            let width = usize::from(case.width);
            let frames = 16_384;
            let source_ref = &source;
            let samples: Vec<f32> = (0..frames)
                .flat_map(|n| {
                    (0..width).map(move |c| {
                        // libvorbis band-limits the LFE channel, so it
                        // carries a 30 Hz tone; the rest a 1 kHz one.
                        let hz = if source_ref[c].is_lfe() {
                            30.0
                        } else {
                            1_000.0
                        };
                        level(c)
                            * std::f32::consts::SQRT_2
                            * (std::f32::consts::TAU * hz * n as f32 / 44_100.0).sin()
                    })
                })
                .collect();
            let mut w =
                VorbisWriter::create(&spec, Quality::HIGHEST, Cursor::new(Vec::new())).unwrap();
            for block in samples.chunks(1_024 * width) {
                w.write(Interleaved::new(block, spec.layout)).unwrap();
            }
            let bytes = w.finish().unwrap().into_inner();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("case.ogg");
            std::fs::write(&path, bytes).unwrap();
            let wave = crate::Wave::read(&path).unwrap();
            assert_eq!(wave.channels(), width, "{width}");
            for (plane, &speaker) in case.planes.iter().enumerate() {
                // The file channel for this speaker; a rear and a side stand
                // for each other, as in the writer.
                let from = source
                    .iter()
                    .position(|&s| s == speaker)
                    .or_else(|| source.iter().position(|&s| counterpart(s) == Some(speaker)))
                    .unwrap();
                let got = rms(wave.channel(plane));
                assert!(
                    (got / level(from) - 1.0).abs() < 0.25,
                    "{width} channels, plane {plane} ({speaker:?}): level {got}, file channel {from} has {}",
                    level(from)
                );
            }
        }
    }
}
