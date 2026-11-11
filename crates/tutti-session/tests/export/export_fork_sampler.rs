//! A sampler voice exported through a fork: a memory voice on a 90 BPM
//! timeline, and a disk-streamed clip. The session halves of what
//! `bevy-tutti`'s export suite pinned before that crate was parked.
//!
//! The live voice plays what the streamer streams into its ring; the fork
//! cannot (the ring's one consumer is the live audio thread, and a seek
//! moves the live stream), so it reads the file the streamer's record names
//! itself, on the render's thread (tutti-sampler's `offline_read`), never
//! from the live ring.
//!
//! The streamer is hand-stepped (`DiskStreamer::manual`), so a refill is a
//! step, not a race with a render that runs faster than real time.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_export::Produced;
use tutti_graph::NodeKey;
use tutti_sampler::testing::settle;
use tutti_sampler::{
    ByHand, DiskStreamer, DiskVoice, MemorySource, Placement, Playback, Stream, StreamReader,
    Voice, VoiceNode, VoiceSource,
};
use tutti_session::BufferMaster;
use tutti_session::{Bounce, BounceError, ExportClock, Latency, Range, Session, Stem, Tail};
use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_types::ChannelLayout;
use tutti_types::{Beat, Bpm, SampleRate, Seconds};

const RATE: f64 = 48_000.0;

/// A bounce of the mix into memory for `seconds` at `rate`: stereo, trimming
/// nothing and keeping no tail, on `clock`.
fn bounce_of(seconds: f64, rate: f64, clock: ExportClock) -> Bounce<NodeKey, BufferMaster> {
    Bounce::project()
        .master_buffers()
        .channels(ChannelLayout::STEREO)
        .range(Range::Seconds(Seconds(0.0), Seconds(seconds)))
        .sample_rate(SampleRate(rate))
        .clock(clock)
        .latency(Latency::Keep)
        .tail(Tail::None)
}

fn render(session: &mut Session, bounce: Bounce<NodeKey, BufferMaster>) -> Vec<Vec<f32>> {
    session
        .bounce(bounce)
        .expect("the bounce renders")
        .into_master()
        .into_planes()
}

/// The planes of `node`'s stem of `bounce`, in memory, rendered beside the
/// master. A stem is a node the outputs hear, so the node is routed to them
/// first: the master is then the node alone, and the stem its own output.
fn render_stem(
    session: &mut Session,
    bounce: Bounce<NodeKey, BufferMaster>,
    node: NodeKey,
) -> Vec<Vec<f32>> {
    session.graph().set_outputs_from(node);
    let (_, mut stems) = session
        .bounce(bounce.stem_with(Stem::buffers(node)))
        .expect("the bounce renders")
        .into_parts();
    match stems.pop() {
        Some(Produced::Buffers(rendered)) if stems.is_empty() => rendered.into_planes(),
        other => panic!("expected one stem held in memory, got {other:?}"),
    }
}

/// A detached stereo session whose graph is at the export's rate.
fn session() -> Session {
    let mut session = Session::builder()
        .inputs(0)
        .outputs_floor(2)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session.graph().set_sample_rate(SampleRate(RATE));
    session
}

/// A timeline at `tempo` BPM from beat 0, at the export's rate.
fn timeline(tempo: f64) -> Arc<OfflineTimeline> {
    Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo_map: Arc::new(tutti_tempo::TempoMap::constant(Bpm::of(tempo)).expect("a tempo")),
        sample_rate: SampleRate(RATE),
        loop_range: None,
    }))
}

fn on(seconds: f64, clock: &Arc<OfflineTimeline>) -> Bounce<NodeKey, BufferMaster> {
    bounce_of(seconds, RATE, ExportClock::timeline(clock.clone()))
}

/// **A node export on a 90 BPM timeline plays where 90 BPM puts it.** A
/// sampler voice placed at beat 3 on the live transport, exported on its own
/// against an offline timeline at 90 BPM from beat 0: at 48 kHz beat 3 is
/// frame 96 000 (at the default 120 BPM it would be 72 000). Silent before,
/// the clip's own samples from there.
///
/// The voice reads the render's `Env`, not the fork's offline timeline;
/// `a_frozen_exports_timeline_is_stopped_at_beat_zero` pins the frozen export's
/// timeline.
#[test]
fn a_node_export_follows_a_90_bpm_timeline() {
    let tone = |i: usize| (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin();

    let mut session = session();
    let mut wave = tutti_codec::Wave::new(1, SampleRate::new(RATE));
    for i in 0..RATE as usize {
        wave.push_frame(&[tone(i)]);
    }
    let voice = {
        let source =
            MemorySource::placed(Arc::new(wave), Beat(3.0), None).expect("a placeable wave");
        let (node, _params) = session.graph().insert(source);
        session.graph().set_outputs_from(node);
        node
    };
    let zero = {
        let mut wave = tutti_codec::Wave::new(1, SampleRate::new(RATE));
        for i in 0..RATE as usize {
            wave.push_frame(&[tone(i)]);
        }
        let source =
            MemorySource::placed(Arc::new(wave), Beat(0.0), None).expect("a placeable wave");
        session.graph().insert(source).0
    };

    let planes = render_stem(&mut session, on(2.5, &timeline(90.0)), voice);
    // Beat 3 at 90 BPM is frame 96 000.
    const AT: usize = 96_000;
    assert!(
        planes[0][..AT].iter().all(|&s| s == 0.0),
        "the voice sounded before beat 3"
    );
    // From there the clip's frame `k` is the render's frame `AT + k`, from
    // the first: the voice reads the render's transport from its blocks'
    // `Env`, per frame, and frame 96 000 reads exactly beat 3, since the
    // timeline counts frames and derives the beat. (An accumulating timeline
    // would read a hair under 3 there.)
    for k in 0..4_000 {
        let got = planes[0][AT + k];
        assert!(
            (got - tone(k)).abs() < 1e-3,
            "frame {k} of the clip read {got}, want {}",
            tone(k)
        );
    }

    // A frozen clock hands the voice a transport stopped at beat 0: it
    // plays nothing, rather than loop its first block against a rolling
    // playhead nothing advances (what a default timeline did before
    // `ExportClock`). Placed at beat 0 here, where a rolling one would sound.
    let planes = render_stem(
        &mut session,
        bounce_of(0.1, RATE, ExportClock::frozen()),
        zero,
    );
    assert!(
        planes[0].iter().all(|&s| s == 0.0),
        "a frozen export played the voice"
    );
}

// ---------------------------------------------------------------------------
// A disk-streamed clip through a fork
// ---------------------------------------------------------------------------

/// Frame `i` of every test file: distinct per frame, and exactly what an
/// f32 WAV hands back.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

/// The file frame a sample of [`value`] came from.
fn frame_of(s: f32) -> i64 {
    (s / 1e-5).round() as i64 - 1
}

/// A stereo f32 WAV of `frames` at `rate`: `value(i)` left, `-value(i)`
/// right.
fn write_ramp(path: &Path, rate: u32, frames: usize) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..frames {
        w.write_sample(value(i)).expect("writes");
        w.write_sample(-value(i)).expect("writes");
    }
    w.finalize().expect("writes");
}

/// A hand-stepped streamer at the export's rate streaming `path`, primed, and
/// the stream's handle and reader. The streamer and the handle are held by the
/// test: a fork reads the file through the streamer's cache while the stream
/// lives, and dropping the handle ends it.
fn streamer_on(path: &Path) -> ((DiskStreamer<ByHand>, Stream), StreamReader) {
    let mut streamer = DiskStreamer::manual(SampleRate(RATE), Default::default()).expect("builds");
    let opening = streamer.open(path, Placement::HEAD);
    let (stream, reader) = settle(&mut streamer, opening).expect("the file opens");
    assert!(
        streamer.step_until_settled(1_000) < 1_000,
        "the ring primes"
    );
    ((streamer, stream), reader)
}

/// The live voice of the stream `reader` reads, placed at `beat`.
fn disk_voice(reader: StreamReader, beat: f64) -> DiskVoice {
    DiskVoice::from_reader(reader, Beat(beat), None)
}

fn node_of(source: VoiceSource) -> VoiceNode {
    VoiceNode::with_channels(
        Voice {
            source,
            play: Playback::default(),
        },
        2u16,
    )
}

/// **A disk-streamed clip exports its file, from the frame its beat falls
/// on**, as the master and as a node: placed at beat 3 of a 90 BPM render
/// (frame 96 000), silent before, then the file's own frame `k` on render
/// frame `96 000 + k`, exactly, on both channels, and silent after its
/// last. The master is a bare `DiskVoice`; the node a `VoiceNode` holding
/// one, which reads it a frame at a time. **And it is the same clip as a
/// memory voice**: a node export of the file's frames in memory, placed
/// the same, renders the same planes bit for bit.
#[test]
fn a_disk_voice_exports_its_file_from_its_beat_and_matches_memory() {
    const AT: usize = 96_000;
    const LEN: usize = 48_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, RATE as u32, LEN);
    // A stream serves one live voice, so each of the two has its own.
    let ((_streamer, reader), (_other, other_reader)) = (streamer_on(&path), streamer_on(&path));

    let mut session = session();
    let (disk_node, memory_node): (NodeKey, NodeKey) = {
        let bare = disk_voice(reader, 3.0);
        let wrapped = node_of(VoiceSource::Disk(disk_voice(other_reader, 3.0)));
        let mut wave = tutti_codec::Wave::new(2, SampleRate::new(RATE));
        for i in 0..LEN {
            wave.push_frame(&[value(i), -value(i)]);
        }
        let memory = node_of(VoiceSource::Memory(
            MemorySource::placed(Arc::new(wave), Beat(3.0), None).expect("a placeable wave"),
        ));
        let graph = session.graph();
        let (bare, _) = graph.insert(bare);
        graph.set_outputs_from(bare);
        (graph.insert(wrapped).0, graph.insert(memory).0)
    };

    let master = render(&mut session, on(3.0, &timeline(90.0)));
    let disk = render_stem(&mut session, on(3.0, &timeline(90.0)), disk_node);
    for (what, planes) in [("master", &master), ("node", &disk)] {
        assert!(
            planes[0][..AT].iter().all(|&s| s == 0.0),
            "{what}: sounded before beat 3"
        );
        for k in 0..LEN {
            assert_eq!(planes[0][AT + k], value(k), "{what}: left, file frame {k}");
            assert_eq!(
                planes[1][AT + k],
                -value(k),
                "{what}: right, file frame {k}"
            );
        }
        assert!(
            planes[0][AT + LEN..].iter().all(|&s| s == 0.0),
            "{what}: sounded past the file's end"
        );
    }

    let memory = render_stem(&mut session, on(3.0, &timeline(90.0)), memory_node);
    for c in 0..2 {
        if let Some(i) =
            (0..disk[c].len()).find(|&i| disk[c][i].to_bits() != memory[c][i].to_bits())
        {
            panic!(
                "channel {c} parts from memory at frame {i}: disk {} memory {}",
                disk[c][i], memory[c][i]
            );
        }
    }
}

/// **A node export of a disk voice plays its file.**
#[test]
fn a_node_export_of_a_disk_voice_plays_its_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, RATE as u32, 4_800);
    let (_streamer, reader) = streamer_on(&path);

    let mut session = session();
    let voice = {
        let voice = node_of(VoiceSource::Disk(disk_voice(reader, 1.0)));
        session.graph().insert(voice).0
    };
    let planes = render_stem(&mut session, on(1.0, &timeline(120.0)), voice);
    assert!(
        planes[0][..24_000].iter().all(|&s| s == 0.0),
        "sounded before beat 1"
    );
    for k in 0..4_800 {
        assert_eq!(planes[0][24_000 + k], value(k), "file frame {k}");
    }
}

/// **A clip whose file is at another rate is resampled to the export's**:
/// a 24 kHz file exported at 48 kHz reads file frame `n / 2` on render
/// frame `n` of the clip (a ramp, which the cubic kernel reproduces
/// between frames up to rounding), and its one second of file lasts
/// 48 000 render frames.
#[test]
fn a_disk_voice_at_another_rate_exports_resampled() {
    const AT: usize = 24_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp_24k.wav");
    write_ramp(&path, 24_000, 24_000);
    let (_streamer, reader) = streamer_on(&path);

    let mut session = session();
    {
        let voice = disk_voice(reader, 1.0);
        let (node, _) = session.graph().insert(voice);
        session.graph().set_outputs_from(node);
    }
    let planes = render(&mut session, on(2.0, &timeline(120.0)));
    assert!(
        planes[0][..AT].iter().all(|&s| s == 0.0),
        "sounded before beat 1"
    );
    assert_eq!(planes[0][AT], value(0), "the clip's first frame on beat 1");
    // From frame 4: the first frames' taps clamp at the file's start.
    for n in 4..47_990 {
        let want = (n as f32 / 2.0 + 1.0) * 1e-5;
        let got = planes[0][AT + n];
        assert!(
            (got - want).abs() < 1e-6,
            "render frame {n} of the clip read {got}, want {want}"
        );
    }
    assert!(
        planes[0][AT + 48_000..].iter().all(|&s| s == 0.0),
        "sounded past the file's end"
    );
}

/// **Live disk playback is untouched while its voice exports.** The live
/// graph renders on its own thread (the streamer stepped between its
/// blocks, its clock moved after each), from before the export starts
/// until after it reports, while a master export forks the same voice.
/// The live voice plays the file's frames one after another the whole
/// time: a render that popped its ring, or asked its streamer to seek,
/// would show as a jump. The export plays the clip from its first frame.
///
/// A fork has no live path to take, so the live-side assertion pins an outcome the
/// structure guarantees.
#[test]
fn live_disk_playback_is_untouched_while_its_voice_exports() {
    const BLOCK: usize = 256;
    // Past the entry seek the live voice raises on its first block.
    const SETTLED: usize = 16 * BLOCK;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("long.wav");
    // Long enough that the live side, paced below, cannot run out of
    // file while the export renders.
    let len = 30 * RATE as usize;
    write_ramp(&path, RATE as u32, len);
    let ((mut streamer, _stream), reader) = streamer_on(&path);
    let live_clock = timeline(120.0);

    let mut session = session();
    let (node, _) = session.graph().insert(disk_voice(reader, 0.0));
    session.graph().set_outputs_from(node);
    session.render_frame(&mut [0.0, 0.0]);
    let mut live = session.graph().take_audio_side();

    let (settled, done) = (AtomicBool::new(false), AtomicBool::new(false));
    let (heard, rendered) = std::thread::scope(|s| {
        let device = s.spawn(|| {
            let mut heard = Vec::new();
            let mut block = vec![Vec::new(), Vec::new()];
            let mut after = 0;
            while after < 8 {
                let _ = streamer.step_until_settled(64);
                live.set_transport(tutti_graph::Transport::new(
                    true,
                    Bpm::of(120.0),
                    live_clock.beat(),
                    None,
                ));
                live.render(BLOCK, BLOCK, &mut block);
                live_clock.advance(BLOCK);
                heard.extend_from_slice(&block[0]);
                assert!(heard.len() < len, "the live side ran out of file first");
                if heard.len() > SETTLED {
                    settled.store(true, Ordering::Release);
                }
                if done.load(Ordering::Acquire) {
                    after += 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            heard
        });
        // The export starts once the live voice has settled, so every
        // frame it could disturb is one the assertions below check.
        while !settled.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let rendered = render(&mut session, on(1.0, &timeline(120.0)));
        done.store(true, Ordering::Release);
        (device.join().expect("the live side"), rendered)
    });

    let first = frame_of(heard[SETTLED]);
    assert!(first >= 0, "the live voice is playing by frame {SETTLED}");
    for (i, &s) in heard[SETTLED..].iter().enumerate() {
        assert_eq!(
            frame_of(s),
            first + i as i64,
            "the live voice jumped at frame {} while the export ran",
            SETTLED + i
        );
    }
    for (k, &s) in rendered[0].iter().enumerate() {
        assert_eq!(s, value(k), "the export's frame {k}");
    }
}

/// Open file descriptors of this process that name `path` (Linux:
/// `/proc/self/fd`).
#[cfg(target_os = "linux")]
fn open_handles(path: &Path) -> usize {
    let path = path.canonicalize().expect("the file exists");
    std::fs::read_dir("/proc/self/fd")
        .expect("procfs")
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|target| *target == path)
        .count()
}

/// **An export leaves nothing open behind it.** The fork's only resource
/// is the file it reads (it registers no streamer stream: it holds no
/// command handle to register one with), and it is closed once the
/// export has reported: the process holds the same handles on the file
/// as before, the live streamer's. That the fork opened it at all is the
/// export playing the clip.
///
/// Linux only: it counts handles through `/proc/self/fd`, which the other
/// platforms do not have. What it pins is platform-independent.
#[cfg(target_os = "linux")]
#[test]
fn an_export_leaves_nothing_open_behind_it() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path, RATE as u32, 4_800);
    let (_streamer, reader) = streamer_on(&path);

    let mut session = session();
    {
        let voice = disk_voice(reader, 0.0);
        let (node, _) = session.graph().insert(voice);
        session.graph().set_outputs_from(node);
    }
    let before = open_handles(&path);
    let planes = render(&mut session, on(0.2, &timeline(120.0)));
    assert_eq!(planes[0][100], value(100), "the export played the clip");
    assert_eq!(
        open_handles(&path),
        before,
        "the export left a handle on the file open"
    );
}

/// **A disk voice whose file cannot be read fails the export by its node's
/// key**, with the file, rather than write its silence as a success. The file
/// is removed after the stream started (the live streamer keeps its own
/// handle); the fork re-opens it by its path, cannot, and its render is
/// `Error::ForkFailed` naming the voice's key.
#[test]
fn a_disk_voice_whose_file_cannot_be_read_fails_the_export_by_name() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("vanished.wav");
    write_ramp(&path, RATE as u32, 4_800);
    let (_streamer, reader) = streamer_on(&path);

    let mut session = session();
    let clip = {
        let voice = disk_voice(reader, 0.0);
        let (node, _) = session.graph().insert(voice);
        session.graph().set_outputs_from(node);
        node
    };
    std::fs::remove_file(&path).expect("removes");
    match session.bounce(on(0.2, &timeline(120.0))) {
        Err(BounceError::Export(tutti_export::Error::ForkFailed { key, kind, cause })) => {
            assert_eq!(key, clip);
            assert_eq!(kind, tutti_graph::ForkFaultKind::Failed);
            assert!(
                cause.to_string().contains("vanished.wav"),
                "names the file: {cause}"
            );
        }
        other => panic!("expected a named failure, got {other:?}"),
    }
}
