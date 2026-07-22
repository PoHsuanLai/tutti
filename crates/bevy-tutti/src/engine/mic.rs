//! Microphone capture as an [`AudioIn`] — the input-device twin of the output
//! stream in [`audio_io`](super::audio_io).
//!
//! The device layer lives here in the adapter, not in `tutti-sampler`: opening a
//! `cpal` stream needs a device, and the engine already owns device I/O for
//! playback. `tutti-sampler` stays device-free (it can build `--no-default-
//! features`); this crate provides the one live [`AudioIn`] that touches
//! hardware, so recording — `pump(mic, wav_sink)` — has a real source to drain.
//!
//! # Threading
//!
//! `cpal` runs the input callback on its own real-time thread (the producer); it
//! only ever `try_push`es interleaved frames into a lock-free SPSC ring, never
//! allocating or blocking. A background pump thread owns the [`MicSource`] (the
//! consumer) and drains the ring via [`poll_into`](AudioIn::poll_into). If the
//! consumer falls behind, the ring fills and the callback drops the newest
//! frames rather than block the audio thread — an overrun, surfaced as a gap,
//! never a glitch on the output stream.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};

use tutti_sampler::AudioIn;

use crate::engine::error::{Error, Result};

/// Ring capacity in stereo frames — ~1s at 48kHz. Large enough that a briefly
/// descheduled pump thread doesn't overrun, small enough to bound latency/RAM.
const RING_FRAMES: usize = 48_000;

/// Keeps the `cpal` input [`Stream`](cpal::Stream) alive. The callback runs for
/// as long as this value exists; dropping it stops capture. The field is never
/// read — ownership *is* the API, mirroring `audio_io::StreamHandle`.
struct StreamHandle(#[allow(dead_code)] cpal::Stream);

// SAFETY: `cpal::Stream` is not `Send` on every platform, but we only ever hold
// it (never touch it across threads) and stop it by dropping on the owning
// thread — the same contract `audio_io::StreamHandle` relies on.
unsafe impl Send for StreamHandle {}

/// A live microphone as an [`AudioIn`]: the consumer end of the capture ring
/// plus the stream handle that feeds it. Poll it with [`poll_into`](AudioIn::poll_into);
/// pump it into any [`AudioOut`](tutti_sampler::AudioOut) (e.g. a `WavSink`) to
/// record.
pub struct MicSource {
    cons: HeapCons<(f32, f32)>,
    sample_rate: f64,
    // Held to keep the input stream running; dropped with the source.
    _stream: StreamHandle,
}

impl MicSource {
    /// Open the default input device (or the `index`-th input device) and start
    /// capturing into the ring. Returns once the stream is live.
    pub fn open(device_index: Option<usize>) -> Result<Self> {
        let device = input_device(device_index)?;
        let config = device.default_input_config()?;
        let sample_rate = f64::from(config.sample_rate().0);
        let channels = usize::from(config.channels());

        let rb = HeapRb::<(f32, f32)>::new(RING_FRAMES);
        let (prod, cons) = rb.split();

        let stream = match config.sample_format() {
            cpal::SampleFormat::I8 => build_input::<i8>(&device, &config.into(), channels, prod)?,
            cpal::SampleFormat::I16 => build_input::<i16>(&device, &config.into(), channels, prod)?,
            cpal::SampleFormat::I32 => build_input::<i32>(&device, &config.into(), channels, prod)?,
            cpal::SampleFormat::U8 => build_input::<u8>(&device, &config.into(), channels, prod)?,
            cpal::SampleFormat::U16 => build_input::<u16>(&device, &config.into(), channels, prod)?,
            cpal::SampleFormat::U32 => build_input::<u32>(&device, &config.into(), channels, prod)?,
            cpal::SampleFormat::F32 => build_input::<f32>(&device, &config.into(), channels, prod)?,
            cpal::SampleFormat::F64 => build_input::<f64>(&device, &config.into(), channels, prod)?,
            format => {
                return Err(Error::InvalidConfig(format!(
                    "Unsupported input sample format: {format:?}"
                )));
            }
        };

        stream.play()?;

        Ok(Self {
            cons,
            sample_rate,
            _stream: StreamHandle(stream),
        })
    }

    /// The capture device's native sample rate. A recorder passes this to the
    /// sink so the WAV header matches the frames it's fed.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Input devices as `(index, name)` — the index is what [`open`](Self::open)
    /// takes. Mirrors `AudioEngine::output_devices`.
    pub fn input_devices() -> Result<impl Iterator<Item = (usize, String)>> {
        Ok(cpal::default_host()
            .input_devices()?
            .enumerate()
            .map(|(i, d)| (i, d.name().unwrap_or_default())))
    }
}

impl AudioIn for MicSource {
    fn poll_into(&mut self, out: &mut [(f32, f32)]) -> usize {
        // Pop up to out.len() frames the callback has pushed. A short/zero count
        // is normal for a live source — the pump backs off and tries again.
        let mut n = 0;
        while n < out.len() {
            match self.cons.try_pop() {
                Some(frame) => {
                    out[n] = frame;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }
}

fn input_device(index: Option<usize>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match index {
        Some(i) => {
            let devices: Vec<_> = host.input_devices()?.collect();
            let count = devices.len();
            devices.into_iter().nth(i).ok_or_else(|| {
                Error::InvalidDevice(format!(
                    "Input device index {i} out of range ({count} available)"
                ))
            })
        }
        None => host
            .default_input_device()
            .ok_or_else(|| Error::InvalidDevice("No input device available".into())),
    }
}

/// Build the `cpal` input stream: the callback downmixes each interleaved
/// device frame to stereo and `try_push`es it into the ring. Alloc-free and
/// non-blocking — a full ring drops the frame (overrun) rather than stall.
fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mut prod: HeapProd<(f32, f32)>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            for frame in data.chunks(channels) {
                let left: f32 = frame[0].to_sample();
                // Mono → duplicate; stereo+ → take the first two channels.
                let right = if channels > 1 {
                    frame[1].to_sample()
                } else {
                    left
                };
                // Drop on overrun: a full ring means the pump fell behind. Never
                // block the RT input thread.
                let _ = prod.try_push((left, right));
            }
        },
        |_err| {},
        None,
    )?;
    Ok(stream)
}
