//! **How a heavy session's blocks sit against their deadline**, serial and on
//! the worker pool, read from the engine's own load meter
//! (`Session::load_meter`).
//!
//! Render-ahead (an anticipative partition rendered a block or more early)
//! only helps when blocks miss their deadline because a *few* cost far more
//! than the rest, not because the average is too high. This harness reports
//! both, per block: mean, p99 and max load, blocks over budget, and max/mean
//! (the spike indicator). It is a percentile harness, not a criterion
//! benchmark, for the reason `tutti-plugin`'s `vst2_in_process` bench gives:
//! criterion's outlier rejection discards the tail, which is the point here.
//!
//! # The scene
//!
//! An offline session (`SessionBuilder::build_offline`), 48 kHz stereo,
//! rolling:
//!
//! - 8 `PolySynth` instruments, saw through a Moog ladder at 1.2 kHz, 16
//!   notes held on each (128 voices), each into a `BusStripNode`;
//! - 4 sampler tracks, each a `VoicePool` of 8 memory voices on a 20 s
//!   tone, each voice transposed by its own interval (-347 to +428 cents),
//!   each into a `BusStripNode`;
//! - one send per track (a second `BusStripNode` at -12 dB), summed into a
//!   stereo `ConvolverNode` reverb (2 s noise-tail IRs, the node's default
//!   512-frame partition) and its return strip;
//! - the 12 track strips and the return summed (`ChannelSumNode`) into a
//!   master strip and a `LimiterNode`.
//!
//! Four variants, each measured on its own:
//!
//! - **`varispeed`**, the main scene: the sampler voices transpose by
//!   varispeed (`Playback::speed`), a resampled read.
//! - **`vocoder`**: the same intervals as a pitch shift (`Playback::pitch`),
//!   which runs each voice through the sampler's phase vocoder (a 2048-point
//!   FFT every 512 output frames): 32 pitch shifters.
//! - **`no-reverb`**: the main scene without the convolver and its send
//!   bus, the control for what the reverb costs.
//! - **`vocoder-dry`**: `vocoder` without the convolver and its send bus,
//!   the vocoder's cost on its own.
//!
//! Each configuration renders 1 s to warm up (notes attack, voices start),
//! then 10 s measured, block by block, at 64 and 128 frames, serial
//! (`render_workers(1)`) and on a pool of one thread per core
//! (`render_workers(0)`).
//!
//! # Where the spikes land
//!
//! The convolver and the vocoder both do their FFT work once per 512
//! frames. The harness prints, per configuration, the mean load at each
//! block position within the 512-frame cycle (`phase`: position 0 is the
//! block starting at a multiple of 512). Measured here, the convolver's work
//! lands on the block ending at a multiple of 512 (the last position) and,
//! with the voices starting at frame 0, the vocoder's on the block ending
//! 256 frames into the cycle, so the two show at different positions. The
//! variants separate them too: `varispeed` against `no-reverb` is the
//! convolver's share, `vocoder-dry` against `no-reverb` the vocoder's, and
//! `vocoder` both at once.
//!
//! The last column is the 1-minute load average after the configuration
//! (the bench's own threads included), so a figure taken beside other work
//! says so.
//!
//! # What it does not measure
//!
//! - **OS scheduling jitter of a real device thread**: this renders on an
//!   ordinary thread, back to back, with no real-time priority and no wait
//!   for a device; a device callback's wake-up and preemption are not here.
//! - **Out-of-process plugins**: their IPC round trip and the server's
//!   scheduling are not in the scene.
//! - **Third-party plugin spikes**: every node is tutti's own, whose
//!   per-block work is bounded and allocation-free.
//!
//! Offline, a block "over budget" only means its render took longer than
//! its frames last at 48 kHz; nothing drops out.
//!
//! ```text
//! cargo bench -p tutti-session --bench render_load --features synth,sampler,convolution,midi
//! ```

use std::sync::Arc;

/// How the sampler voices transpose, and whether the reverb is in the
/// scene.
#[derive(Clone, Copy)]
enum Variant {
    Varispeed,
    Vocoder,
    NoReverb,
    VocoderDry,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Self::Varispeed => "varispeed",
            Self::Vocoder => "vocoder",
            Self::NoReverb => "no-reverb",
            Self::VocoderDry => "vocoder-dry",
        }
    }

    /// Whether the sampler voices pitch-shift through the phase vocoder
    /// (otherwise they transpose by varispeed).
    fn vocoder(self) -> bool {
        matches!(self, Self::Vocoder | Self::VocoderDry)
    }

    /// Whether the scene has the convolution reverb and its sends.
    fn reverb(self) -> bool {
        matches!(self, Self::Varispeed | Self::Vocoder)
    }
}

use tutti_core::{
    Amplitude, Beat, Cents, ChannelLayout, Db, Hz, InterleavedMut, MotionEvent, PlaybackRate,
    Resonance, SampleRate, Samples, Seconds,
};
use tutti_graph::{Cost, Cx, Event, Io, Node, Offset, Prepare, Shape, Status, Unforkable};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
use tutti_nodes::{BusStripNode, ChannelSumNode, ConvolverNode, LimiterNode};
use tutti_polysynth::{EnvelopeConfig, FilterType, OscillatorType, PolySynth, SynthConfig};
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
use tutti_session::{EventSource, GraphSource, Session};

const RATE: f64 = 48_000.0;
const SYNTHS: usize = 8;
const NOTES: usize = 16;
const SAMPLER_TRACKS: usize = 4;
const SAMPLER_VOICES: usize = 8;
const WARM: Seconds = Seconds(1.0);
const MEASURED: Seconds = Seconds(10.0);

/// Holds `notes` notes on its event output: one note-on per block from the
/// first, then nothing.
struct Hold {
    base: u8,
    notes: usize,
    sent: usize,
}

impl Node for Hold {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_cost(Cost::Light)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if self.sent < self.notes {
            let note = MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                // Spread over four octaves: no two voices share a pitch.
                self.base + (self.sent as u8 * 3) % 48,
                0xC000,
            );
            let at = Offset::new(0, Samples(io.frames())).expect("a non-empty block");
            if io.event_out(0).push(Event::midi(at, note.data)).is_ok() {
                self.sent += 1;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Deterministic white noise in `-1..1` (xorshift), so the bench needs no
/// RNG crate and every run renders the same scene.
fn noise(state: &mut u32) -> f32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// A 2 s exponentially decaying noise tail: a plain reverb IR.
fn ir(seed: u32) -> Vec<f32> {
    let mut s = seed;
    let len = Seconds(2.0).to_samples(SampleRate(RATE)).get();
    (0..len)
        .map(|i| noise(&mut s) * (-6.9 * i as f32 / len as f32).exp() * 0.05)
        .collect()
}

fn connect_stereo(
    s: &mut Session,
    to: tutti_core::AudioNode,
    first_port: usize,
    from: tutti_core::AudioNode,
) {
    for c in 0..2 {
        s.graph()
            .set_source(to, first_port + c, GraphSource::Node(from, c));
    }
}

/// The scene the module docs describe, on `workers` render threads, its
/// transport rolling.
fn scene(block: usize, workers: usize, variant: Variant) -> Session {
    let mut s = Session::builder()
        .render_workers(workers)
        .build_offline(SampleRate(RATE), ChannelLayout::STEREO, Samples(block))
        .expect("builds");

    let mut strips = Vec::new();
    let mut sends = Vec::new();
    for i in 0..SYNTHS {
        let synth = PolySynth::new(SynthConfig {
            sample_rate: SampleRate(RATE),
            max_voices: NOTES,
            oscillator: OscillatorType::Saw,
            filter: FilterType::Moog {
                cutoff: Hz(1_200.0),
                resonance: Resonance(0.3),
            },
            envelope: EnvelopeConfig::new(
                Seconds(0.005),
                Seconds(0.2),
                Amplitude(0.7),
                Seconds(0.3),
            ),
            ..Default::default()
        })
        .expect("synth builds");
        let (synth, _) = s.graph().add(synth);
        let (hold, _) = s.graph().insert(Unforkable(Hold {
            base: 36 + i as u8,
            notes: NOTES,
            sent: 0,
        }));
        s.graph()
            .set_event_sources(synth, 0, &[EventSource::new(hold, 0)]);
        strips.push(synth);
    }

    let mut tone = tutti_io::Wave::new(1, RATE);
    let mut seed = 0x1234_5678;
    let frames = Seconds(20.0).to_samples(SampleRate(RATE)).get();
    for i in 0..frames {
        let t = i as f32 / RATE as f32;
        let x = (std::f32::consts::TAU * 220.0 * t).sin() * 0.3 + noise(&mut seed) * 0.05;
        tone.push_frame(&[x]);
    }
    let tone = Arc::new(tone);
    for t in 0..SAMPLER_TRACKS {
        let mut pool = VoicePool::new();
        for v in 0..SAMPLER_VOICES {
            pool.insert_voice(
                SlotId(v as u128 + 1),
                Voice {
                    source: VoiceSource::Memory(MemorySource::placed(
                        Arc::clone(&tone),
                        Beat(0.0),
                        None,
                    )),
                    play: {
                        let k = (t * SAMPLER_VOICES + v) as f32;
                        let interval = Cents::new(-350.0 + 25.0 * k + 3.0);
                        let play = Playback {
                            gain: Amplitude(0.1),
                            ..Default::default()
                        };
                        if variant.vocoder() {
                            Playback {
                                pitch: interval,
                                ..play
                            }
                        } else {
                            Playback {
                                speed: PlaybackRate::new_clamped(interval.to_pitch_ratio()),
                                ..play
                            }
                        }
                    },
                    channel_index: None,
                },
            );
        }
        let (pool, _) = s.graph().insert(pool);
        strips.push(pool);
    }

    // Each source into its channel strip, and (with the reverb) a send off
    // the source.
    let reverb = variant.reverb();
    let tracks: Vec<_> = strips
        .into_iter()
        .map(|src| {
            let (strip, _) = s.graph().add(BusStripNode::new());
            connect_stereo(&mut s, strip, 0, src);
            if reverb {
                let send = BusStripNode::new();
                send.volume().store(
                    Db(-12.0).to_amplitude().get(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                let (send, _) = s.graph().add(send);
                connect_stereo(&mut s, send, 0, src);
                sends.push(send);
            }
            strip
        })
        .collect();

    let mut sums = tracks;
    if reverb {
        let (send_bus, _) = s
            .graph()
            .add(ChannelSumNode::new(sends.len(), ChannelLayout::STEREO));
        for (k, send) in sends.iter().enumerate() {
            connect_stereo(&mut s, send_bus, 2 * k, *send);
        }
        let (l, r) = (ir(1), ir(2));
        let (conv, _) = s.graph().add(ConvolverNode::stereo(&l, &r, PARTITION));
        connect_stereo(&mut s, conv, 0, send_bus);
        let (ret, _) = s.graph().add(BusStripNode::new());
        connect_stereo(&mut s, ret, 0, conv);
        sums.push(ret);
    }

    let (master_bus, _) = s
        .graph()
        .add(ChannelSumNode::new(sums.len(), ChannelLayout::STEREO));
    for (k, strip) in sums.iter().enumerate() {
        connect_stereo(&mut s, master_bus, 2 * k, *strip);
    }
    let (master, _) = s.graph().add(BusStripNode::new());
    connect_stereo(&mut s, master, 0, master_bus);
    let (limiter, _) = s.graph().add(LimiterNode::with_channels(
        ChannelLayout::STEREO,
        Db(-1.0),
        Db(0.0),
    ));
    connect_stereo(&mut s, limiter, 0, master);
    for c in 0..2 {
        s.graph()
            .set_output_source(c, GraphSource::Node(limiter, c));
    }
    s.update();
    s.transport()
        .motion
        .try_send(MotionEvent::Play)
        .expect("room");
    s
}

/// The 512-frame cycle: the convolver's partition, and the vocoder's
/// output hop.
const PARTITION: usize = 512;

struct Figures {
    mean: f32,
    p99: f32,
    max: f32,
    over: usize,
    blocks: usize,
    /// Mean load at each block position within the 512-frame cycle
    /// (position 0 starts at a multiple of 512).
    phase: Vec<f32>,
    peak_out: f32,
}

fn measure(block: usize, workers: usize, variant: Variant) -> Figures {
    let s = scene(block, workers, variant);
    let meter = Arc::clone(s.load_meter().expect("an offline engine"));
    let mut out = vec![0.0f32; block * 2];
    let blocks = |span: Seconds| span.to_samples(SampleRate(RATE)).get().div_ceil(block);
    let warm = blocks(WARM);
    for _ in 0..warm {
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));
    }
    let n = blocks(MEASURED);
    let mut loads = Vec::with_capacity(n);
    let mut peak_out = 0.0f32;
    meter.reset();
    for _ in 0..n {
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));
        loads.push(meter.peek().last);
        peak_out = out.iter().fold(peak_out, |m, x| m.max(x.abs()));
    }
    let over = loads.iter().filter(|&&l| l > 1.0).count();
    assert_eq!(
        over as u64,
        meter.peek().over_budget,
        "the meter's own count"
    );
    let positions = (PARTITION / block).max(1);
    let mut phase = vec![(0.0f32, 0usize); positions];
    for (k, &l) in loads.iter().enumerate() {
        let p = &mut phase[(warm + k) % positions];
        p.0 += l;
        p.1 += 1;
    }
    let mean = loads.iter().sum::<f32>() / n as f32;
    let max = loads.iter().copied().fold(0.0, f32::max);
    let mut sorted = loads;
    sorted.sort_by(f32::total_cmp);
    let p99 = sorted[(n * 99).div_ceil(100) - 1];
    Figures {
        mean,
        p99,
        max,
        over,
        blocks: n,
        phase: phase.iter().map(|&(sum, n)| sum / n as f32).collect(),
        peak_out,
    }
}

/// The 1-minute load average, where the OS reports one: a figure taken
/// beside other work is not a quiet machine's, and the reader should see
/// which it was.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|l| l.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "n/a".to_owned())
}

fn main() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!(
        "render_load: {SYNTHS} synths x {NOTES} notes, {SAMPLER_TRACKS} sampler tracks x \
         {SAMPLER_VOICES} voices, convolution send, 48 kHz; {cores} cores available"
    );
    println!(
        "{:>5} {:>11} {:>7} {:>7} {:>7} {:>8} {:>11} {:>8} {:>8}  phase (mean load by position in 512 frames)",
        "block", "variant", "workers", "mean", "p99", "max", "over/blocks", "max/mean", "loadavg"
    );
    for variant in [
        Variant::Varispeed,
        Variant::NoReverb,
        Variant::Vocoder,
        Variant::VocoderDry,
    ] {
        for block in [64usize, 128] {
            for (label, workers) in [("1", 1usize), ("cores", 0)] {
                let f = measure(block, workers, variant);
                // A silent render would measure nothing worth reading.
                assert!(f.peak_out > 0.0, "the scene sounds");
                let phase: Vec<String> = f.phase.iter().map(|l| format!("{l:.3}")).collect();
                println!(
                    "{:>5} {:>11} {:>7} {:>7.3} {:>7.3} {:>8.3} {:>11} {:>8.2} {:>8}  {}",
                    block,
                    variant.label(),
                    label,
                    f.mean,
                    f.p99,
                    f.max,
                    format!("{}/{}", f.over, f.blocks),
                    f.max / f.mean,
                    loadavg(),
                    phase.join(" "),
                );
            }
        }
    }
}
