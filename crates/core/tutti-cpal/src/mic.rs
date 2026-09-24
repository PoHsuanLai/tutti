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

use cpal::traits::{DeviceTrait, StreamTrait};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};

use tutti_core::io::{AudioIn, OnEmpty};
use tutti_core::pcm::BitDepth;
use tutti_core::ChannelLayout;
use tutti_core::SampleRate;
use tutti_core::Samples;
use tutti_core::MAX_ROOT_CHANNELS;
use tutti_io::{share_mic_ring, MicMonitorNode, MicRing, WavOut};

use crate::error::{Error, Result};
use crate::host::{AudioHost, DeviceHost, DeviceSelector, Direction};

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
struct StreamHandle(
    #[allow(dead_code, reason = "ownership is the API — held for Drop, never read")] cpal::Stream,
);

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
    sample_rate: SampleRate,
    // Held to keep the input stream running; dropped with the source.
    _stream: StreamHandle,
}

impl MicIn {
    /// Open an input device at the graph's sample rate and start capturing.
    /// Returns once the stream is live.
    ///
    /// **The rate is a parameter, not something read off the device**, and
    /// that is the point. [`MicMonitorNode`](tutti_io::MicMonitorNode) renders
    /// the mic into the graph with no resampling — its `set_sample_rate` is a
    /// documented no-op resting on the assumption that the device layer opened
    /// the mic at the graph's rate. Nothing enforced that: this function used
    /// to take whatever `default_input_config` reported while
    /// `AudioEngine::start` independently took whatever the *output* device
    /// reported, and nothing compared them. A 44.1 kHz mic feeding a 48 kHz
    /// graph drifted, silently, for as long as the take lasted.
    ///
    /// A device whose supported range covers `graph_rate` is opened **at**
    /// `graph_rate`. One that cannot is [`Error::SampleRateMismatch`] rather
    /// than a stream that sounds nearly right.
    ///
    /// Be aware of what this does *not* prove: asking for a rate does not make
    /// a device produce it. ALSA plug devices advertise wide ranges and
    /// resample internally. The error is honest about what was checked.
    pub fn open(sel: impl Into<DeviceSelector>, graph_rate: SampleRate) -> Result<Self> {
        let (source, _) = Self::open_inner(sel.into(), graph_rate, false)?;
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
    pub fn open_with_monitor(
        sel: impl Into<DeviceSelector>,
        graph_rate: SampleRate,
    ) -> Result<(Self, MicMonitorNode)> {
        let (source, monitor) = Self::open_inner(sel.into(), graph_rate, true)?;
        // `open_inner(_, true)` always returns the monitor.
        Ok((source, monitor.expect("monitor requested")))
    }

    fn open_inner(
        sel: DeviceSelector,
        graph_rate: SampleRate,
        with_monitor: bool,
    ) -> Result<(Self, Option<MicMonitorNode>)> {
        let host = DeviceHost::open(AudioHost::Default)?;
        let device = host.device(Direction::Input, &sel)?;
        let name = device.name().unwrap_or_default();
        let config = choose_input_config(
            device.supported_input_configs().map_err(|e| {
                Error::InvalidDevice(format!("cannot query {name:?} input configs: {e}"))
            })?,
            device.default_input_config()?,
            graph_rate,
            &name,
        )?;
        let sample_rate = SampleRate::from(config.sample_rate().0);
        let channels = ChannelLayout::from(usize::from(config.channels()));

        let rb = HeapRb::<[f32; 2]>::new(RING_FRAMES);
        let (prod, cons) = rb.split();

        // Optional monitor tap: a second, shallow ring the callback also feeds.
        let (mon_prod, monitor) = if with_monitor {
            let mon_rb = HeapRb::<[f32; 2]>::new(MONITOR_RING_FRAMES);
            let (mon_prod, mon_cons) = mon_rb.split();
            let ring: MicRing = share_mic_ring(mon_cons);
            // `new_at`, not `new`: the node then carries the rate it was
            // opened at, and its `set_sample_rate` debug-asserts the graph
            // agrees. That assertion is the unchecked half of the same
            // guarantee `Error::SampleRateMismatch` is the checked half of —
            // the two-check shape `pump`'s layout `debug_assert` and
            // `Recorder::start`'s returned error already use.
            (
                Some(mon_prod),
                Some(MicMonitorNode::new_at(ring, sample_rate)),
            )
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
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Build a WAV sink that matches this mic: its native rate, its own
    /// reported width, at `depth`.
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
    /// # Errors
    ///
    /// Whatever [`WavOut::create`] reports — the file could not be created or
    /// the header not written.
    pub fn matching_sink(
        &self,
        path: impl AsRef<std::path::Path>,
        depth: BitDepth,
    ) -> std::io::Result<WavOut> {
        // Read off `AudioIn::layout` rather than hard-coded, so this pairing
        // cannot drift from what `poll_into` actually hands back. (Today that is
        // always stereo — the callback folds every device frame to a pair before
        // it reaches the ring — but a sink built from a *different* constant than
        // the source reports is exactly what `Recorder::start` now refuses, and
        // this helper exists to be the pairing that always passes.)
        WavOut::create(path, self.sample_rate, AudioIn::layout(self), depth)
    }

    /// Input devices as `(index, name)` — the index is what [`open`](Self::open)
    /// takes. Mirrors `AudioEngine::output_devices`.
    pub fn input_devices() -> Result<impl Iterator<Item = (usize, String)>> {
        Ok(DeviceHost::open(AudioHost::Default)?
            .input_devices()?
            .into_iter()
            .map(|d| (d.index, d.name)))
    }
}

impl AudioIn for MicIn {
    /// A live capture device: an empty ring means the callback has not pushed
    /// since the last poll, not that the microphone is finished. A consumer
    /// that stopped here would end a recording within milliseconds of starting
    /// it.
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    /// Always stereo, whatever the device's own width is.
    ///
    /// The capture callback **folds** every device frame down to a stereo pair
    /// before it reaches the ring (in `build_input`), so what a consumer
    /// polls is stereo by construction. The ring's element is `[f32; 2]`, and
    /// widening that means widening a lock-free ring element — a separate
    /// change with its own cost. Reporting the *device's* layout here would be a
    /// lie about what `poll_into` hands back, and `Recorder::start` would then
    /// approve a 6-channel sink for a stereo stream.
    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        // Pop up to `out.len() / 2` FRAMES the callback has pushed — the return
        // is frames, the slice is samples. A short/zero count is normal for a
        // live source — the pump backs off and tries again.
        let frames = Samples::from_interleaved_len(out.len(), ChannelLayout::STEREO).get();
        let mut n = 0;
        while n < frames {
            match self.cons.try_pop() {
                Some([l, r]) => {
                    out[n * 2] = l;
                    out[n * 2 + 1] = r;
                    n += 1;
                }
                None => break,
            }
        }
        Samples(n)
    }
}

/// Build the `cpal` input stream: the callback **folds** each interleaved
/// device frame down to stereo and `try_push`es it into the recording ring —
/// and, when monitoring, into a second (shallow) monitor ring. Alloc-free and
/// non-blocking — a full ring drops the frame (overrun) rather than stall, so
/// neither tap can ever block the RT input thread or the other tap.
///
/// # Folding, not truncating
///
/// Reading `frame[0]` and `frame[1]` and discarding the rest would, on a 5.1
/// capture device, silently throw away the **centre channel — the dialogue —
/// and both surrounds**, which is precisely the defect `downmix`'s module doc
/// calls out. Every device frame instead routes through
/// [`fold_frame`](tutti_core::fold_frame), the engine's single ITU-R BS.775 /
/// Dolby implementation, so a wide capture arrives correctly downmixed and a
/// mono one still duplicates into both sides (the fold's 1→2 arm).
///
/// # Why a scratch buffer, and why it is on the stack
///
/// `data: &[T]` is generic over [`cpal::SizedSample`], so the samples must be
/// converted to `f32` before the fold can see them — a per-frame convert is
/// unavoidable. **This is the RT input callback, so it must not allocate**: the
/// scratch is a fixed `[f32; MAX_ROOT_CHANNELS]` used as a *prefix* (the
/// engine's established pattern), never a `vec!`. A device wider than the
/// ceiling contributes only its leading channels rather than growing the
/// buffer.
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
    // Derived ONCE, outside the callback — never inside the per-frame loop.
    let stride = channels.max(1);
    let fold_width = stride.min(MAX_ROOT_CHANNELS);

    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            // Stack scratch, reused every frame. Sized at the fixed ceiling and
            // sliced to the device width — no allocation on the RT thread.
            let mut src = [0.0f32; MAX_ROOT_CHANNELS];
            for frame in data.chunks_exact(stride) {
                for (slot, s) in src[..fold_width].iter_mut().zip(frame) {
                    *slot = s.to_sample();
                }
                // The engine's one downmix: mono fans, stereo passes, surround
                // folds per ITU/Dolby with the LFE dropped.
                let mut stereo = [0.0f32; 2];
                tutti_core::fold_frame(&src[..fold_width], &mut stereo);
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

/// Choose an input configuration that runs at `graph_rate`, or say why none
/// can.
///
/// Free and pure so it is testable with **no device**: cpal's
/// `SupportedStreamConfigRange::new` is public, so a fixture can state a
/// device's capabilities directly. Before this existed, every branch of the
/// rate decision lived inside `MicIn::open_inner` behind a real sound card.
///
/// The rules, in order:
/// 1. a supported range whose `[min, max]` contains `graph_rate` — preferring
///    `F32`, the format the callback converts from most cheaply;
/// 2. otherwise the device's own default, if it happens to match;
/// 3. otherwise [`Error::SampleRateMismatch`], naming both rates.
pub(crate) fn choose_input_config(
    supported: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    fallback: cpal::SupportedStreamConfig,
    graph_rate: SampleRate,
    device_name: &str,
) -> Result<cpal::SupportedStreamConfig> {
    let wanted = cpal::SampleRate(graph_rate.get().round() as u32);

    let mut best: Option<cpal::SupportedStreamConfig> = None;
    for range in supported {
        if range.min_sample_rate() > wanted || range.max_sample_rate() < wanted {
            continue;
        }
        let candidate = range.with_sample_rate(wanted);
        // Prefer F32: `build_input` converts every other format per sample,
        // and this is the one that is already the graph's type.
        if candidate.sample_format() == cpal::SampleFormat::F32 {
            return Ok(candidate);
        }
        if best.is_none() {
            best = Some(candidate);
        }
    }
    if let Some(c) = best {
        return Ok(c);
    }

    if fallback.sample_rate() == wanted {
        return Ok(fallback);
    }

    Err(Error::SampleRateMismatch {
        device_name: device_name.to_string(),
        device: SampleRate::from(fallback.sample_rate().0),
        graph: graph_rate,
    })
}

#[cfg(test)]
mod tests {
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
}
