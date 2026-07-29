//! Microphone capture as an [`AudioIn`] — the input-device twin of the output
//! stream in [`output`](super::output).
//!
//! This is the device crate, so opening the `cpal` stream belongs here and
//! nowhere lower. Everything the mic feeds is device-free and sits one layer
//! down in `tutti-io`: the monitor node this hands back, the `WavOut` a take
//! records into, and the `Recorder` that pumps between them. What this crate
//! contributes is the one live [`AudioIn`] that touches hardware.
//!
//! # Threading
//!
//! `cpal` runs the input callback on its own real-time thread (the producer); it
//! only ever `try_push`es interleaved frames into lock-free SPSC rings, never
//! allocating or blocking. A background pump thread owns the [`MicIn`] (the
//! consumer) and drains the recording ring via [`poll_into`](AudioIn::poll_into).
//! If the consumer falls behind, the ring fills and the callback drops the newest
//! frames rather than block the audio thread — an overrun, surfaced as a gap,
//! never a glitch on the output stream.
//!
//! # Live monitoring
//!
//! [`open_with_monitor`](MicIn::open_with_monitor) tees the same capture
//! callback into a *second*, shallow ring drained by a [`MicMonitorNode`] (a
//! a `tutti_io` `AudioUnit`, so it's device-free and lives in the graph).
//! Add that node to the audio graph — through effects if you like — to hear the
//! mic live while recording the same input. The two rings are independent: the
//! recording ring is deep (dropout-resistant, latency irrelevant to a file); the
//! monitor ring is shallow (low-latency, so you don't hear yourself slapped-back).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};

use tutti_core::io::{AudioIn, OnEmpty};
use tutti_core::pcm::BitDepth;
use tutti_core::ChannelLayout;
use tutti_io::{share_mic_ring, MicMonitorNode, MicRing, WavOut};

use crate::error::{Error, Result};

/// Capture-ring capacity in stereo frames — ~1s at 48kHz. Large enough that a
/// briefly descheduled pump thread doesn't overrun, small enough to bound
/// latency/RAM. This is the *recording* ring; the pump thread drains it, so a
/// deep buffer trades latency (irrelevant to a file) for dropout resistance.
const RING_FRAMES: usize = 48_000;

/// Monitor-ring capacity in stereo frames — ~10ms at 48kHz. The audio callback
/// drains this one *per block*, so it needs only enough slack to bridge one
/// buffer's jitter. It is deliberately SHALLOW: a deep monitor ring would be
/// heard as latency (you'd hear yourself slapped-back), and a full ring just
/// drops the oldest-unread frames, which for live monitoring is the right
/// failure — always hear "now", never a growing delay.
const MONITOR_RING_FRAMES: usize = 480;

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
/// pump it into any [`AudioOut`](tutti_core::io::AudioOut) (e.g. a `WavOut`) to
/// record.
pub struct MicIn {
    cons: HeapCons<[f32; 2]>,
    sample_rate: f64,
    // Held to keep the input stream running; dropped with the source.
    _stream: StreamHandle,
}

impl MicIn {
    /// Open the default input device (or the `index`-th input device) and start
    /// capturing into the ring. Returns once the stream is live.
    pub fn open(device_index: Option<usize>) -> Result<Self> {
        let (source, _) = Self::open_inner(device_index, false)?;
        Ok(source)
    }

    /// Open the mic *and* a live-monitor tap in one stream: the capture callback
    /// pushes each frame into both the recording ring (drained by
    /// [`poll_into`](AudioIn::poll_into) / `pump` → a `WavOut`) and a shallow
    /// monitor ring drained by the returned [`MicMonitorNode`]. Add that node to
    /// the audio graph to hear the mic live — through effects — *while*
    /// recording the same input.
    ///
    /// One device, one callback, two independent rings: recording tolerates
    /// jitter with a deep buffer; monitoring stays low-latency with a shallow
    /// one. Neither can stall the other or the capture thread.
    pub fn open_with_monitor(device_index: Option<usize>) -> Result<(Self, MicMonitorNode)> {
        let (source, monitor) = Self::open_inner(device_index, true)?;
        // `open_inner(_, true)` always returns the monitor.
        Ok((source, monitor.expect("monitor requested")))
    }

    fn open_inner(
        device_index: Option<usize>,
        with_monitor: bool,
    ) -> Result<(Self, Option<MicMonitorNode>)> {
        let device = input_device(device_index)?;
        let config = device.default_input_config()?;
        let sample_rate = f64::from(config.sample_rate().0);
        let channels = ChannelLayout::from(usize::from(config.channels()));

        let rb = HeapRb::<[f32; 2]>::new(RING_FRAMES);
        let (prod, cons) = rb.split();

        // Optional monitor tap: a second, shallow ring the callback also feeds.
        let (mon_prod, monitor) = if with_monitor {
            let mon_rb = HeapRb::<[f32; 2]>::new(MONITOR_RING_FRAMES);
            let (mon_prod, mon_cons) = mon_rb.split();
            let ring: MicRing = share_mic_ring(mon_cons);
            (Some(mon_prod), Some(MicMonitorNode::new(ring)))
        } else {
            (None, None)
        };

        // Raw interleaved buffer math (`data.chunks(..)`) needs the device's
        // channel count as a `usize`; derive it from the layout at the boundary.
        let channel_count = channels.count() as usize;

        let sample_format = config.sample_format();
        let cfg = config.into();
        let stream = match sample_format {
            cpal::SampleFormat::I8 => {
                build_input::<i8>(&device, &cfg, channel_count, prod, mon_prod)?
            }
            cpal::SampleFormat::I16 => {
                build_input::<i16>(&device, &cfg, channel_count, prod, mon_prod)?
            }
            cpal::SampleFormat::I32 => {
                build_input::<i32>(&device, &cfg, channel_count, prod, mon_prod)?
            }
            cpal::SampleFormat::U8 => {
                build_input::<u8>(&device, &cfg, channel_count, prod, mon_prod)?
            }
            cpal::SampleFormat::U16 => {
                build_input::<u16>(&device, &cfg, channel_count, prod, mon_prod)?
            }
            cpal::SampleFormat::U32 => {
                build_input::<u32>(&device, &cfg, channel_count, prod, mon_prod)?
            }
            cpal::SampleFormat::F32 => {
                build_input::<f32>(&device, &cfg, channel_count, prod, mon_prod)?
            }
            cpal::SampleFormat::F64 => {
                build_input::<f64>(&device, &cfg, channel_count, prod, mon_prod)?
            }
            format => {
                return Err(Error::InvalidConfig(format!(
                    "Unsupported input sample format: {format:?}"
                )));
            }
        };

        stream.play()?;

        Ok((
            Self {
                cons,
                sample_rate,
                _stream: StreamHandle(stream),
            },
            monitor,
        ))
    }

    /// The capture device's native sample rate. A recorder passes this to the
    /// sink so the WAV header matches the frames it's fed.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Build a WAV sink that matches this mic: its native rate, its stereo
    /// width, at `depth`.
    ///
    /// Recording needs a source and a sink whose rate and channel count agree,
    /// and **nothing downstream can check that**: `AudioIn` deliberately carries
    /// no rate (see its docs — a caller that needs one holds the concrete type),
    /// so a pump handed an 8 kHz sink and a 48 kHz mic writes a valid WAV that
    /// plays back six times too slow, silently.
    ///
    /// This is the one place both halves are in scope, so pairing them here is
    /// what makes the mismatch unrepresentable for the common case. A caller
    /// with a different sink still builds its own — the obligation is only
    /// removed where it can be.
    ///
    /// Returns `None` when the file cannot be created or the header written,
    /// matching [`WavOut::create`].
    pub fn matching_sink(&self, path: &std::path::PathBuf, depth: BitDepth) -> Option<WavOut> {
        // 2 because `MicIn` downmixes every device frame to a stereo pair before
        // it reaches the ring; see the callback.
        WavOut::create(path, self.sample_rate, 2, depth)
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

impl AudioIn for MicIn {
    /// A live capture device: an empty ring means the callback has not pushed
    /// since the last poll, not that the microphone is finished. A consumer
    /// that stopped here would end a recording within milliseconds of starting
    /// it.
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
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
/// device frame to stereo and `try_push`es it into the recording ring — and,
/// when monitoring, into a second (shallow) monitor ring. Alloc-free and
/// non-blocking — a full ring drops the frame (overrun) rather than stall, so
/// neither tap can ever block the RT input thread or the other tap.
fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mut prod: HeapProd<[f32; 2]>,
    mut mon_prod: Option<HeapProd<[f32; 2]>>,
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
                let stereo = [left, right];
                // Drop on overrun: a full ring means the consumer fell behind.
                // Never block the RT input thread.
                let _ = prod.try_push(stereo);
                // Tee into the monitor ring when present. A full monitor ring
                // drops this frame (the graph consumer briefly fell behind); the
                // node reads silence for those and catches up on the next block.
                // The ring is sized shallow (~10ms) so the monitor never builds
                // a growing backlog of latency even under sustained pressure.
                if let Some(ref mut mon) = mon_prod {
                    let _ = mon.try_push(stereo);
                }
            }
        },
        |_err| {},
        None,
    )?;
    Ok(stream)
}
