use super::*;

use tutti_types::Sink;

/// A device that supports `[min, max]` at `format`.
fn range(min: u32, max: u32, format: cpal::SampleFormat) -> cpal::SupportedStreamConfigRange {
    cpal::SupportedStreamConfigRange::new(
        2,
        cpal::SampleRate(min),
        cpal::SampleRate(max),
        cpal::SupportedBufferSize::Unknown,
        format,
    )
}

/// What a pull of an empty ring answers, whatever width the mic has.
fn verdict(mic: &mut MicIn) -> Pull {
    mic.pull(&mut [0.0; 16])
}

fn fallback(rate: u32) -> cpal::SupportedStreamConfig {
    range(rate, rate, cpal::SampleFormat::F32).with_sample_rate(cpal::SampleRate(rate))
}

/// **A device that can run at the graph's rate is opened at it**, not at
/// whatever its default happens to be.
///
/// Before this, `open_inner` took `default_input_config()` unconditionally.
/// A mic defaulting to 44.1 kHz on a 48 kHz graph opened at 44.1 and
/// drifted, because an inlet does not resample and nothing
/// compared the two rates.
#[test]
fn a_device_covering_the_graph_rate_is_opened_at_it() {
    let chosen = choose_input_config(
        [range(8_000, 96_000, cpal::SampleFormat::F32)].into_iter(),
        fallback(44_100),
        SampleRate(48_000.0),
        "Wide Range Mic",
    )
    .expect("48k is inside [8k, 96k]");
    assert_eq!(chosen.sample_rate(), cpal::SampleRate(48_000));
}

/// **F32 wins when several supported ranges cover the rate.**
///
/// Not cosmetic: `build_input` converts every other format per sample, and
/// F32 is already the graph's type.
#[test]
fn f32_is_preferred_over_an_integer_format_at_the_same_rate() {
    let chosen = choose_input_config(
        [
            range(44_100, 48_000, cpal::SampleFormat::I16),
            range(44_100, 48_000, cpal::SampleFormat::F32),
        ]
        .into_iter(),
        fallback(44_100),
        SampleRate(48_000.0),
        "Dual Format Mic",
    )
    .expect("both ranges cover 48k");
    assert_eq!(chosen.sample_format(), cpal::SampleFormat::F32);
    assert_eq!(chosen.sample_rate(), cpal::SampleRate(48_000));
}

/// An integer-only device is still accepted — preference is not a
/// requirement.
#[test]
fn an_integer_only_device_is_accepted_at_the_graph_rate() {
    let chosen = choose_input_config(
        [range(44_100, 48_000, cpal::SampleFormat::I16)].into_iter(),
        fallback(44_100),
        SampleRate(48_000.0),
        "I16 Mic",
    )
    .expect("the range covers 48k");
    assert_eq!(chosen.sample_format(), cpal::SampleFormat::I16);
    assert_eq!(chosen.sample_rate(), cpal::SampleRate(48_000));
}

/// **A device that cannot reach the graph's rate is an error, naming both
/// rates.**
///
/// The whole point: silent drift becomes a refusal a host can show.
#[test]
fn a_device_that_cannot_reach_the_graph_rate_is_refused() {
    let err = choose_input_config(
        [range(44_100, 44_100, cpal::SampleFormat::F32)].into_iter(),
        fallback(44_100),
        SampleRate(48_000.0),
        "Fixed 44k1 Mic",
    )
    .expect_err("44.1k-only cannot serve a 48k graph");

    match err {
        Error::SampleRateMismatch {
            device_name,
            device,
            graph,
        } => {
            assert_eq!(device_name, "Fixed 44k1 Mic");
            assert_eq!(device.get(), 44_100.0);
            assert_eq!(graph.get(), 48_000.0);
        }
        other => panic!("expected SampleRateMismatch, got {other:?}"),
    }
}

/// A device that advertises nothing but whose default already matches is
/// accepted — some backends report an empty supported-config list.
#[test]
fn a_default_that_already_matches_is_accepted_with_no_ranges() {
    let chosen = choose_input_config(
        std::iter::empty(),
        fallback(48_000),
        SampleRate(48_000.0),
        "Silent About Its Configs",
    )
    .expect("the default already runs at the graph rate");
    assert_eq!(chosen.sample_rate(), cpal::SampleRate(48_000));
}

/// A `MicIn` over an empty ring and a fresh fault record, with no device.
fn mic_without_a_device() -> MicIn {
    mic_with_ring(2, 8).0
}

/// [`mic_without_a_device`] `width` wide, and the writer its callback would
/// hold.
fn mic_with_ring(width: u16, frames: usize) -> (MicIn, TapWriter) {
    let (reader, writer) = TapReader::pair(TapId(0), usize::from(width), Samples(frames));
    let mic = MicIn {
        tap: reader,
        sample_rate: SampleRate(48_000.0),
        device_inputs: ChannelLayout::from(width),
        faults: Arc::new(StreamFaults::new()),
        _stream: StreamHandle(Owned::spawn("test-input", || Ok(())).expect("spawn")),
    };
    (mic, writer)
}

/// A full ring drops the callback's frames and the source counts them; the
/// count clears when read. (The stream that fills the ring needs a device; the
/// writer's refusal is the same one `Capture::block` hits.)
#[test]
fn a_full_ring_counts_the_frames_the_callback_dropped() {
    let (mut mic, mut writer) = mic_with_ring(2, 2);
    for i in 0..5 {
        writer.push_interleaved(&[i as f32, 0.0]);
    }
    assert_eq!(Source::take_lost(&mut mic), Samples(3));
    assert_eq!(Source::take_lost(&mut mic), Samples(0), "reading clears");
}

/// A capture of `device` inputs, selecting `inputs`, over rings of room for
/// `frames` frames; the mic end and the monitor's.
fn capture(device: u16, inputs: &[usize], frames: usize) -> (Capture, MicIn, TapReader) {
    let select = Fold::select(ChannelLayout::from(device), inputs).expect("a selection");
    let width = select.dst_layout();
    let (reader, rec) = TapReader::pair(TapId(0), usize::from(width.count()), Samples(frames));
    let (ring, mon) = TapReader::pair(TapId(0), usize::from(width.count()), Samples(frames));
    let capture = Capture::new(select, ChannelLayout::from(device), rec, Some(mon));
    let mic = MicIn {
        tap: reader,
        sample_rate: SampleRate(48_000.0),
        device_inputs: ChannelLayout::from(device),
        faults: Arc::new(StreamFaults::new()),
        _stream: StreamHandle(Owned::spawn("test-input", || Ok(())).expect("spawn")),
    };
    (capture, mic, ring)
}

/// Inputs 3 and 4 of 16 reach the recording as its two channels, in that
/// order, and the monitor ring carries the same two; inputs beyond the first
/// two are reachable, which the stereo fold never allowed.
///
/// The device frames are `100 * frame + input`, so every sample says where it
/// came from.
#[test]
fn inputs_three_and_four_of_sixteen_are_the_recording() {
    let (mut cap, mut mic, ring) = capture(16, &[2, 3], 64);
    let data: Vec<f32> = (0..3)
        .flat_map(|f| (0..16).map(move |c| (100 * f + c) as f32))
        .collect();
    cap.block(&data);
    assert_eq!(Source::layout(&mic), ChannelLayout::STEREO);
    assert_eq!(
        mic.topology().positions(),
        [tutti_types::Speaker::Unassigned; 2]
    );
    assert_eq!(mic.device_inputs(), ChannelLayout::from(16u16));
    let mut out = [0.0f32; 8];
    assert_eq!(mic.pull(&mut out), Pull::Frames(Samples(3)));
    assert_eq!(out[..6], [2.0, 3.0, 102.0, 103.0, 202.0, 203.0]);

    let mut node = tutti_graph::contract::prepared(
        tutti_graph::InletNode::new(ring),
        SampleRate(48_000.0),
        64,
    );
    let env = tutti_graph::Env {
        frame: tutti_types::Frame(0),
        sample_rate: SampleRate(48_000.0),
        block_len: Samples(3),
        transport: tutti_graph::Transport::default(),
        changes: tutti_graph::TransportChanges::NONE,
        play: tutti_graph::PlayClock::ZERO,
    };
    let heard = tutti_graph::contract::drive_input(&mut node, &env, &[], &[], &[]).audio;
    assert_eq!(heard[0], [2.0, 102.0, 202.0]);
    assert_eq!(heard[1], [3.0, 103.0, 203.0]);
}

/// A six-input interface delivers six unassigned channels when asked, and is no
/// longer folded as 5.1: the sixth input arrives untouched beside the first.
/// One input may feed both sides (`&[0, 0]`), which is what `open` does for a
/// mono device.
#[test]
fn a_six_input_interface_is_six_inputs_not_five_point_one() {
    let (mut cap, mut mic, _ring) = capture(6, &[0, 1, 2, 3, 4, 5], 64);
    cap.block(&[0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6]);
    let mut out = [0.0f32; 6];
    assert_eq!(mic.pull(&mut out), Pull::Frames(Samples(1)));
    assert_eq!(out, [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);

    let (mut cap, mut mic, _ring) = capture(1, &[0, 0], 64);
    cap.block(&[0.25f32, -0.5]);
    let mut out = [0.0f32; 4];
    assert_eq!(mic.pull(&mut out), Pull::Frames(Samples(2)));
    assert_eq!(out, [0.25, 0.25, -0.5, -0.5]);
}

/// A callback larger than the chunk runs in chunks, in order, and an integer
/// device converts per sample.
///
/// 150 frames of `i16` input 1 (of 2) are 64 + 64 + 22: the frames come out
/// whole and in order.
#[test]
fn a_big_callback_runs_in_chunks_and_integers_convert() {
    let (mut cap, mut mic, _ring) = capture(2, &[1], 512);
    let data: Vec<i16> = (0..150).flat_map(|f| [0, f as i16 * 100]).collect();
    cap.block(&data);
    let mut out = vec![0.0f32; 200];
    let n = mic.pull(&mut out);
    assert_eq!(n, Pull::Frames(Samples(150)));
    for (f, &s) in out[..150].iter().enumerate() {
        let want = f32::from(f as i16 * 100) / 32768.0;
        assert!((s - want).abs() < 1e-6, "frame {f}: {s} vs {want}");
    }
}

/// The default selection is the first two inputs, or a lone input twice.
#[test]
fn the_default_is_the_first_two_inputs_or_the_lone_one_twice() {
    assert_eq!(default_inputs(ChannelLayout::MONO), [0, 0]);
    assert_eq!(default_inputs(ChannelLayout::STEREO), [0, 1]);
    assert_eq!(default_inputs(ChannelLayout::from(16u16)), [0, 1]);
}

/// An input the device does not have is an `Error::Inputs`, with `FoldError`'s
/// reason; so is an empty selection.
#[test]
fn an_input_past_the_devices_last_is_an_error() {
    let err = Error::from(Fold::select(ChannelLayout::from(4u16), &[2, 4]).unwrap_err());
    assert!(
        matches!(
            err,
            Error::Inputs(tutti_types::FoldError::NoSuchChannel { channel: 4, .. })
        ),
        "{err}"
    );
    assert!(matches!(
        Error::from(Fold::select(ChannelLayout::STEREO, &[]).unwrap_err()),
        Error::Inputs(tutti_types::FoldError::Empty(_))
    ));
}

/// A disconnect reported through the shared fault record fails the source, and
/// `faults()` is that same record. Another backend error does not: it may be
/// transient. cpal's error callback is `move |err| faults.record(&err)`; the
/// registration itself needs a device and is not exercised here.
#[test]
fn a_disconnect_fails_the_mic_and_a_transient_error_does_not() {
    let mut mic = mic_without_a_device();
    assert_eq!(
        verdict(&mut mic),
        Pull::Starved,
        "a live mic with an empty ring is late, not finished"
    );

    mic.faults().record(&cpal::StreamError::BackendSpecific {
        err: cpal::BackendSpecificError {
            description: "xrun".into(),
        },
    });
    assert!(mic.faults().is_faulted());
    assert_eq!(
        verdict(&mut mic),
        Pull::Starved,
        "a backend error is not a death"
    );

    mic.faults().record(&cpal::StreamError::DeviceNotAvailable);
    assert_eq!(verdict(&mut mic), Pull::Failed);
}

/// A mic whose device disconnects still hands over the frames its ring held,
/// and then reads as failed, so a pump that drains before it checks
/// (`Source::pull`'s contract: the fault flag is read before the ring and
/// `Failed` needs an empty ring; `tutti-session`'s `PumpLoop` is the pump that
/// relies on it, and cannot be a dependency here) writes them and ends the take
/// as failed.
#[test]
fn a_disconnected_mic_hands_over_its_buffered_frames_then_reads_failed() {
    let (mut mic, mut prod) = mic_with_ring(2, 8);
    let faults = mic.faults();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mic-fault.wav");
    let mut sink = mic
        .matching_sink(&path, Format::Wav(tutti_codec::Pcm::Float32))
        .unwrap();

    prod.push_interleaved(&[0.5, -0.5]);
    faults.record(&cpal::StreamError::DeviceNotAvailable);
    // The pump's pass: the frame the ring held is written first.
    let mut scratch = [0.0f32; 8];
    let Pull::Frames(n) = mic.pull(&mut scratch) else {
        panic!("the buffered frame is delivered before the failure");
    };
    assert_eq!(n, Samples(1));
    sink.write(&scratch[..n.interleaved_len(mic.layout())])
        .expect("the file takes it");
    // Then nothing more comes, and the source says why.
    assert_eq!(
        mic.pull(&mut scratch),
        Pull::Failed,
        "the take is over: the device is gone"
    );
    Sink::finalize(sink).expect("the prefix closes cleanly");
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 2);
}

/// The ALSA shape of an unplugged mic, a backend error repeated with no audio,
/// fails the source.
#[test]
fn a_mic_whose_backend_keeps_failing_without_audio_is_failed() {
    let mut mic = mic_without_a_device();
    let faults = mic.faults();
    let t0 = faults.epoch;
    let err = cpal::StreamError::BackendSpecific {
        err: cpal::BackendSpecificError {
            description: "`alsa::poll()` returned POLLERR".into(),
        },
    };
    faults.record_at(&err, t0);
    assert_eq!(verdict(&mut mic), Pull::Starved);
    faults.record_at(&err, t0 + crate::faults::STALL_GRACE);
    assert_eq!(verdict(&mut mic), Pull::Failed);
}

// `Capture` is crate-private, so its allocation gate lives in the library's own
// test binary rather than in `tests/` (where the output gates are). Nothing
// else in this binary allocates inside an `assert_no_alloc` section.
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// The capture callback does not allocate: blocks of `f32` and `i16`, wider than
/// a chunk, with the recording ring and the monitor ring both filling and
/// then refusing (the full-ring branch), run under a disabled allocator.
#[test]
fn the_capture_callback_does_not_allocate() {
    let (mut cap, mut mic, _ring) = capture(8, &[2, 3], 256);
    let f32s = vec![0.25f32; 8 * 150];
    let i16s = vec![1000i16; 8 * 150];
    // Warm both conversions, outside the gate.
    cap.block(&f32s);
    cap.block(&i16s);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            cap.block(&f32s);
            cap.block(&i16s);
        }
    });
    // The rings filled and refused whole frames; nothing was lost silently.
    assert!(Source::take_lost(&mut mic).get() > 0);
}
