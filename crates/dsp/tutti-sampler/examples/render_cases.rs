//! Render a matrix of sampler cases to WAV for independent analysis.
//!
//! This exists because four separate defects in this crate — a 60 dB gain error,
//! a 17.5 dB phase-seeding error, an inert pitch shift, and a seek smear — were
//! each invisible to every test in the suite. The tests that missed them were
//! written against the same understanding of the DSP as the code, so they could
//! not contradict it. An external tool re-deriving the expected answer from the
//! signal alone has no such shared blind spot.
//!
//! Drives the **real** `VoicePool`: placement gate, `MemorySource`, the resident
//! stretch filter, and a block-advanced transport — not `stretch::Unit` in
//! isolation. That is the point; the unit already has value-based tests, and what
//! is unverified is the assembly.
//!
//! Run: `cargo run --release -p tutti-sampler --example render_cases -- <outdir>`

use std::f32::consts::TAU;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::dsp::{BufferArray, U2};
use tutti_core::{AudioUnit, Beat, Bpm, Cents, StretchFactor, Timeline, Wave};
use tutti_sampler::voice::{
    MemorySource, Playback, SlotId, Voice, VoiceCommand, VoicePool, VoiceSource,
};

/// A rolling transport whose beat this example advances by hand.
///
/// The crate's `MockTransport` is `#[cfg(test)]`, and an example is not a test.
/// Reimplemented here rather than un-gating it: `Timeline` is three methods, and
/// widening a test-only surface so a diagnostic can reach it would make the
/// production API answer to this file.
struct Clock {
    beat: AtomicU64,
    tempo: f64,
}

impl Clock {
    fn new(tempo: f64) -> Arc<Self> {
        Arc::new(Self {
            beat: AtomicU64::new(0f64.to_bits()),
            tempo,
        })
    }

    fn set_beat(&self, beat: f64) {
        self.beat.store(beat.to_bits(), Ordering::Relaxed);
    }

    /// Move by `samples`, the way a block-driven transport does after `process`.
    fn advance(&self, samples: usize, sample_rate: f64) {
        let beats = samples as f64 * self.tempo / 60.0 / sample_rate;
        let now = f64::from_bits(self.beat.load(Ordering::Relaxed));
        self.set_beat(now + beats);
    }
}

impl Timeline for Clock {
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(self.tempo)
    }
    fn is_rolling(&self) -> bool {
        true
    }
}

const SR: f64 = 48_000.0;
/// Long enough that the analysis window can settle well before the measurement
/// window opens, at every stretch factor rendered here.
const FRAMES: usize = 48_000;
const BLOCK: usize = 64;

/// A 440 Hz sine, the reference tone every case is measured against.
fn tone(freq: f32, frames: usize) -> Arc<Wave> {
    let mut w = Wave::new(1, SR);
    for i in 0..frames {
        w.push((TAU * freq * i as f32 / SR as f32).sin());
    }
    Arc::new(w)
}

/// Render one case through a real pool and return interleaved stereo output.
fn render(stretch: f32, cents: f32, seek_at: Option<usize>) -> Vec<f32> {
    let (mut pool, handle) = VoicePool::new();
    let transport = Clock::new(120.0);
    // 4x the output length: at the slowest factor the source is consumed far
    // faster than it is emitted, and a short wave would run dry mid-measurement
    // and look like an attenuation bug.
    let wave = tone(440.0, FRAMES * 4);

    let source = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
    let mut play = Playback::default();
    play.stretch = StretchFactor::new(stretch);
    play.pitch = Cents::new(cents);

    handle.send(VoiceCommand::AddVoice {
        id: SlotId(1),
        voice: Box::new(Voice {
            source: VoiceSource::Memory(source),
            play,
            channel_index: None,
        }),
        stretch: None,
    });

    // Block-driven via `process`, NOT per-frame `tick`.
    //
    // A placed voice derives its read position from the playhead, and the
    // playhead advances once per block. `process` walks the block with
    // `offset_in_block` so each sample reads the right place; `tick` has no such
    // offset, so calling it 64 times against one transport reading emits the
    // SAME sample 64 times — a staircase that resamples the source downward and
    // makes every case fail, including an unprocessed one.
    //
    // That is what the first draft of this example did, and the `dry` control is
    // what caught it: 440 Hz came out as 308 Hz with no processing engaged.
    // Keeping the note because the mistake is invisible in any single case —
    // only the control says "the harness is wrong, not the engine".
    let mut ib = BufferArray::<U2>::new();
    let mut ob = BufferArray::<U2>::new();
    let mut out = Vec::with_capacity(FRAMES * 2);
    let mut rendered = 0usize;
    while rendered < FRAMES {
        if let Some(at) = seek_at {
            // Jump the playhead forward at the block containing `at`, the case
            // that used to smear pre-jump audio over the new region for ~48 ms.
            if rendered <= at && at < rendered + BLOCK {
                // Beat 5 of an 8-beat wave (FRAMES*4 samples at 120 BPM). A
                // seek past the material renders correct silence and proves
                // nothing about flushing — the first draft jumped to beat 20 and
                // measured an empty region.
                transport.set_beat(5.0);
            }
        }
        pool.process(BLOCK, &ib.buffer_ref(), &mut ob.buffer_mut());
        for i in 0..BLOCK {
            out.push(ob.buffer_ref().at_f32(0, i));
            out.push(ob.buffer_ref().at_f32(1, i));
        }
        transport.advance(BLOCK, SR);
        rendered += BLOCK;
    }
    out.truncate(FRAMES * 2);
    out
}

/// Minimal 16-bit PCM WAV writer — avoids a dependency for four header structs.
fn write_wav(path: &Path, interleaved: &[f32], channels: u16) {
    let bits = 16u16;
    let n = interleaved.len() as u32 * u32::from(bits) / 8;
    let byte_rate = SR as u32 * u32::from(channels) * u32::from(bits) / 8;
    let mut v = Vec::with_capacity(44 + n as usize);
    v.extend(b"RIFF");
    v.extend((36 + n).to_le_bytes());
    v.extend(b"WAVEfmt ");
    v.extend(16u32.to_le_bytes());
    v.extend(1u16.to_le_bytes());
    v.extend(channels.to_le_bytes());
    v.extend((SR as u32).to_le_bytes());
    v.extend(byte_rate.to_le_bytes());
    v.extend((channels * bits / 8).to_le_bytes());
    v.extend(bits.to_le_bytes());
    v.extend(b"data");
    v.extend(n.to_le_bytes());
    for &s in interleaved {
        v.extend(((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, v).expect("write wav");
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: render_cases <outdir>");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).expect("create outdir");

    // name, stretch, cents, seek_at. Every case states what it should sound
    // like; the Python side asserts it without reading this file.
    let cases: &[(&str, f32, f32, Option<usize>)] = &[
        ("dry", 1.0, 0.0, None),
        ("pitch_up_octave", 1.0, 1200.0, None),
        ("pitch_down_octave", 1.0, -1200.0, None),
        ("pitch_up_fifth", 1.0, 700.0, None),
        ("pitch_down_fourth", 1.0, -500.0, None),
        ("pitch_up_two_octaves", 1.0, 2400.0, None),
        ("pitch_down_two_octaves", 1.0, -2400.0, None),
        ("stretch_half", 0.5, 0.0, None),
        ("stretch_double", 2.0, 0.0, None),
        ("stretch_1p5", 1.5, 0.0, None),
        ("stretch_double_pitch_up", 2.0, 1200.0, None),
        ("stretch_half_pitch_down", 0.5, -1200.0, None),
        ("stretch_1p5_pitch_up_fifth", 1.5, 700.0, None),
        ("seek_while_stretched", 2.0, 0.0, Some(24_000)),
        ("seek_while_dry", 1.0, 0.0, Some(24_000)),
    ];

    for (name, stretch, cents, seek) in cases {
        let pcm = render(*stretch, *cents, *seek);
        write_wav(&dir.join(format!("{name}.wav")), &pcm, 2);
        println!("{name},{stretch},{cents},{}", seek.is_some());
    }
    eprintln!("wrote {} cases to {}", cases.len(), dir.display());
}
