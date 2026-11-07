use super::*;

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

fn fallback(rate: u32) -> cpal::SupportedStreamConfig {
    range(rate, rate, cpal::SampleFormat::F32).with_sample_rate(cpal::SampleRate(rate))
}

/// **A device that can run at the graph's rate is opened at it**, not at
/// whatever its default happens to be.
///
/// Before this, `open_inner` took `default_input_config()` unconditionally.
/// A mic defaulting to 44.1 kHz on a 48 kHz graph opened at 44.1 and
/// drifted, because `MicMonitorNode` does not resample and nothing
/// compared the two rates.
///
/// Mutation-checked: returning `fallback` unconditionally fails this.
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
///
/// Mutation-checked: returning `Ok(fallback)` here fails this.
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
    let (reader, writer) = TapReader::standalone(TapId(0), usize::from(width), Samples(frames));
    let mic = MicIn {
        tap: TapIn::new(reader),
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
///
/// Mutation (run, recorded here): `take_lost` returning `Samples(0)` fails.
#[test]
fn a_full_ring_counts_the_frames_the_callback_dropped() {
    let (mut mic, mut writer) = mic_with_ring(2, 2);
    for i in 0..5 {
        writer.push_interleaved(&[i as f32, 0.0]);
    }
    assert_eq!(AudioIn::take_lost(&mut mic), Samples(3));
    assert_eq!(AudioIn::take_lost(&mut mic), Samples(0), "reading clears");
}

/// A capture of `device` inputs, selecting `inputs`, over rings of room for
/// `frames` frames; the mic end and the monitor's.
fn capture(device: u16, inputs: &[usize], frames: usize) -> (Capture, MicIn, tutti_io::MicRing) {
    let select = Fold::select(ChannelLayout::from(device), inputs).expect("a selection");
    let width = select.dst_layout();
    let (reader, rec) =
        TapReader::standalone(TapId(0), usize::from(width.count()), Samples(frames));
    let (ring, mon) = tutti_io::MicRing::pair(width, Samples(frames));
    let capture = Capture::new(select, ChannelLayout::from(device), rec, Some(mon));
    let mic = MicIn {
        tap: TapIn::new(reader),
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
///
/// Mutation (run): `Fold::select(.., &[2, 3])` read as `&[0, 1]` in the test's
/// setup (the old stereo rule) fails the figures; the callback reading
/// `select.frame(&self.frame[..2], ..)` panics on the width assert; a chunk
/// that skipped `self.mon` fails the monitor assertion.
#[test]
fn inputs_three_and_four_of_sixteen_are_the_recording() {
    let (mut cap, mut mic, ring) = capture(16, &[2, 3], 64);
    let data: Vec<f32> = (0..3)
        .flat_map(|f| (0..16).map(move |c| (100 * f + c) as f32))
        .collect();
    cap.block(&data);
    assert_eq!(AudioIn::layout(&mic), ChannelLayout::STEREO);
    assert_eq!(
        mic.topology().positions(),
        [tutti_types::Speaker::Discrete; 2]
    );
    assert_eq!(mic.device_inputs(), ChannelLayout::from(16u16));
    let mut out = [0.0f32; 8];
    assert_eq!(mic.poll_into(&mut out), Samples(3));
    assert_eq!(out[..6], [2.0, 3.0, 102.0, 103.0, 202.0, 203.0]);

    let mut node = tutti_graph::contract::prepared(
        tutti_io::MicMonitorNode::new(ring),
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

/// A six-input interface delivers six discrete channels when asked, and is no
/// longer folded as 5.1: the sixth input arrives untouched beside the first.
/// One input may feed both sides (`&[0, 0]`), which is what `open` does for a
/// mono device.
///
/// Mutation (run): routing the selection through `Fold::assuming_smpte` (the
/// old width fold) puts the LFE-position input (3) at `0.0` and the centre at
/// -3 dB on the sides, failing the figures.
#[test]
fn a_six_input_interface_is_six_inputs_not_five_point_one() {
    let (mut cap, mut mic, _ring) = capture(6, &[0, 1, 2, 3, 4, 5], 64);
    cap.block(&[0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6]);
    let mut out = [0.0f32; 6];
    assert_eq!(mic.poll_into(&mut out), Samples(1));
    assert_eq!(out, [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);

    let (mut cap, mut mic, _ring) = capture(1, &[0, 0], 64);
    cap.block(&[0.25f32, -0.5]);
    let mut out = [0.0f32; 4];
    assert_eq!(mic.poll_into(&mut out), Samples(2));
    assert_eq!(out, [0.25, 0.25, -0.5, -0.5]);
}

/// A callback larger than the chunk runs in chunks, in order, and an integer
/// device converts per sample.
///
/// 150 frames of `i16` input 1 (of 2) are 64 + 64 + 22: the frames come out
/// whole and in order.
///
/// Mutation (run): the chunk loop resetting `frames` into the same slot
/// (`chunks_exact_mut` zipped from the start per frame, not per chunk) is what
/// this guards; dropping the `data.chunks` loop for one chunk loses all but
/// the first 64 frames and fails the length.
#[test]
fn a_big_callback_runs_in_chunks_and_integers_convert() {
    let (mut cap, mut mic, _ring) = capture(2, &[1], 512);
    let data: Vec<i16> = (0..150).flat_map(|f| [0, f as i16 * 100]).collect();
    cap.block(&data);
    let mut out = vec![0.0f32; 200];
    let n = mic.poll_into(&mut out);
    assert_eq!(n, Samples(150));
    for (f, &s) in out[..150].iter().enumerate() {
        let want = f32::from(f as i16 * 100) / 32768.0;
        assert!((s - want).abs() < 1e-6, "frame {f}: {s} vs {want}");
    }
}

/// The default selection is the first two inputs, or a lone input twice.
///
/// Mutation (run): `usize::from(width.count() > 1)` written as `1` selects
/// input 1 of a mono device, which `Fold::select` then refuses, and a wide
/// device gets `[0, 1]` either way; written as `0` gives a stereo device the
/// left input twice: fails the second row.
#[test]
fn the_default_is_the_first_two_inputs_or_the_lone_one_twice() {
    assert_eq!(default_inputs(ChannelLayout::MONO), [0, 0]);
    assert_eq!(default_inputs(ChannelLayout::STEREO), [0, 1]);
    assert_eq!(default_inputs(ChannelLayout::from(16u16)), [0, 1]);
}

/// An input the device does not have is an `Error::Inputs`, with `FoldError`'s
/// reason; so is an empty selection.
///
/// Mutation (run): removing the `#[from]` conversion does not compile; the
/// `NoSuchChannel` arm is `Fold::select`'s, pinned in `tutti-types`.
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
///
/// Mutation: `is_failed` returning `false`, or reading `is_faulted()` instead
/// of `is_disconnected()`, fails the first or last assertion respectively.
#[test]
fn a_disconnect_fails_the_mic_and_a_transient_error_does_not() {
    let mic = mic_without_a_device();
    assert!(!AudioIn::is_failed(&mic));

    mic.faults().record(&cpal::StreamError::BackendSpecific {
        err: cpal::BackendSpecificError {
            description: "xrun".into(),
        },
    });
    assert!(mic.faults().is_faulted());
    assert!(!AudioIn::is_failed(&mic), "a backend error is not a death");

    mic.faults().record(&cpal::StreamError::DeviceNotAvailable);
    assert!(AudioIn::is_failed(&mic));
}

/// A recorder over a mic whose device disconnects ends the take as failed,
/// after writing the frames the ring still held.
///
/// Mutation: dropping the `is_failed` override in `MicIn` makes the last pass
/// `Starved` and fails the `Failed` assertion.
#[test]
fn a_recorder_on_a_disconnected_mic_fails_the_take() {
    let (mic, mut prod) = mic_with_ring(2, 8);
    let faults = mic.faults();
    let path = std::env::temp_dir().join(format!("tutti-mic-fault-{}.wav", std::process::id()));
    let sink = mic.matching_sink(&path, Pcm::Float32).unwrap();
    let (driver, pump) = tutti_io::ManualDriver::new();
    let mut rec = tutti_io::Recorder::start_with(mic, sink, driver).unwrap();

    prod.push_interleaved(&[0.5, -0.5]);
    faults.record(&cpal::StreamError::DeviceNotAvailable);
    assert_eq!(
        pump.pump_once(),
        Some(tutti_io::PumpPass::Wrote(Samples(1)))
    );
    assert_eq!(pump.pump_once(), Some(tutti_io::PumpPass::Failed));
    assert!(rec.failed());
    assert!(
        matches!(rec.finish_in_place(), Some(Err(_))),
        "the take reports the failed source"
    );
    let _ = std::fs::remove_file(&path);
}

/// The ALSA shape of an unplugged mic, a backend error repeated with no audio,
/// fails the source.
///
/// Mutation: dropping `delivering.delivered()` is not covered here (it needs the
/// callback); dropping the promotion in `record_at` fails this.
#[test]
fn a_mic_whose_backend_keeps_failing_without_audio_is_failed() {
    let mic = mic_without_a_device();
    let faults = mic.faults();
    let t0 = faults.epoch;
    let err = cpal::StreamError::BackendSpecific {
        err: cpal::BackendSpecificError {
            description: "`alsa::poll()` returned POLLERR".into(),
        },
    };
    faults.record_at(&err, t0);
    assert!(!AudioIn::is_failed(&mic));
    faults.record_at(&err, t0 + crate::faults::STALL_GRACE);
    assert!(AudioIn::is_failed(&mic));
}
