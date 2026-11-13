//! Microphone capture as a [`Source`] — the input-device twin of the output
//! stream in [`output`](super::output).
//!
//! This is the device crate, so opening the `cpal` stream belongs here and
//! nowhere lower. Everything the mic feeds is device-free and lives lower: the
//! inlet node that copies the monitor ring into the graph (`tutti-graph`'s
//! `InletNode`, opened by `tutti-session`'s `Graph::add_inlet`), the
//! `tutti_codec::FileOut` a take records into, and the `Recorder` that pumps
//! between them (`tutti-session`). What this crate contributes is the one live
//! [`Source`] that touches hardware.
//!
//! # Threading
//!
//! `cpal` runs the input callback on its own real-time thread (the producer); it
//! only ever pushes whole interleaved frames into lock-free SPSC rings (one lane
//! a channel, [`tutti_graph::TapWriter`]), never allocating or blocking. A background pump thread owns the [`MicIn`] (the
//! consumer) and drains the recording ring via [`pull`](Source::pull).
//! If the consumer falls behind, the ring fills and the callback drops the newest
//! frames rather than block the audio thread — an overrun, surfaced as a gap,
//! never a glitch on the output stream.
//!
//! # Live monitoring
//!
//! [`open_with_monitor`](MicIn::open_with_monitor) tees the same capture
//! callback into a *second*, shallow ring and hands back its reader. Give it to
//! `tutti-session`'s `Graph::add_inlet`, which reads the rate off the ring and
//! refuses one that is not the graph's; the inlet node it makes copies the
//! ring into the graph. (`tutti-session`'s `Session::add_input` does both
//! halves, and reopens them after a restart.) Wire that node through effects if you like to hear the
//! mic live while recording the same input. The two rings are independent: the
//! recording ring is deep (dropout-resistant, latency irrelevant to a file); the
//! monitor ring is shallow (low-latency, so you don't hear yourself slapped-back).
//!
//! # Which inputs
//!
//! A device's inputs are numbered, not placed: cpal reports a count and no
//! channel map, so they are [`Speaker::Unassigned`](tutti_types::Speaker::Unassigned)
//! and never go through a surround matrix (a 6-input interface is six
//! microphones, not 5.1). The mic delivers the inputs it was opened with:
//! [`MicConfig::inputs`] defaults to the first two (a mono device's one input on
//! both sides) and takes any list of them, so "inputs 3 and 4 of 16" is
//! `Some(vec![2, 3])`. The selection is
//! [`Fold::select`](tutti_types::Fold::select), applied in the capture callback.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, StreamTrait};

use tutti_codec::{FileOut, Format, Spec};
use tutti_graph::TapId;
use tutti_graph::{InletRing, TapReader, TapWriter};
use tutti_rt::Drained;
use tutti_types::SampleRate;
use tutti_types::Samples;
use tutti_types::{ChannelLayout, ChannelTopology, Fold};
use tutti_types::{Pull, Source};

use crate::driver_seam::device_rate;
use crate::error::{Error, Result};
use crate::faults::StreamFaults;
use crate::host::{AudioHost, DeviceHost, DeviceSelector};
use crate::latency::StreamLatency;
use crate::owner::Owned;
use crate::roster::Direction;

/// Capture-ring capacity in frames — ~1s at 48kHz. Large enough that a
/// briefly descheduled pump thread doesn't overrun, small enough to bound
/// latency/RAM. This is the *recording* ring; the pump thread drains it, so a
/// deep buffer trades latency (irrelevant to a file) for dropout resistance.
const RING_FRAMES: usize = 48_000;

/// Monitor-ring capacity in frames — ~10ms at 48kHz. The audio callback
/// drains this one *per block*, so it needs only enough slack to bridge one
/// buffer's jitter. It is deliberately SHALLOW: a deep monitor ring would be
/// heard as latency (you'd hear yourself slapped-back), and a full ring just
/// drops the oldest-unread frames, which for live monitoring is the right
/// failure — always hear "now", never a growing delay.
pub const MONITOR_RING_FRAMES: usize = 480;

/// Device frames the capture callback selects and pushes at a time. The
/// callback's scratch is this many frames of the selection, allocated once, so
/// a callback of any size runs in chunks of it.
const CHUNK_FRAMES: usize = 64;

/// Keeps the `cpal` input stream alive on its own thread. The callback runs for
/// as long as this value exists; dropping it stops capture. The field is never
/// read — ownership *is* the API, as with the output side's `CpalStream`.
#[derive(Debug)]
struct StreamHandle(
    #[allow(dead_code, reason = "ownership is the API — held for Drop, never read")] Owned,
);

/// A live microphone as a [`Source`]. Requires the `capture` feature.
///
/// The CPAL input callback selects the inputs the mic was opened with from each
/// device frame and pushes them into a lock-free ring of about one second; this
/// value is the consumer end. Pull from it with [`pull`](Source::pull),
/// or hand it to a `tutti_session::Recorder` with a sink from
/// [`matching_sink`](Self::matching_sink) to record. If the consumer falls
/// behind, the callback drops the newest frames rather than block. Capture
/// stops when this value is dropped.
///
/// Its [`layout`](Source::layout) is the number of inputs selected, and its
/// [`topology`](Self::topology) says they feed no speaker.
///
/// [`open_with_monitor`](Self::open_with_monitor) also returns the reader of a
/// second, shallow (about 10 ms) ring fed from the same callback, for an inlet
/// that makes the mic heard live through the graph while it is recorded.
pub struct MicIn {
    tap: TapReader,
    sample_rate: SampleRate,
    /// How many inputs the device has.
    device_inputs: ChannelLayout,
    /// What the input stream's error callback reports into.
    faults: Arc<StreamFaults>,
    /// The delay between a frame's capture and its callback, as the callback
    /// last measured it.
    latency: Arc<StreamLatency>,
    // Held to keep the input stream running; dropped with the source.
    _stream: StreamHandle,
}

impl std::fmt::Debug for MicIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicIn")
            .field("sample_rate", &self.sample_rate)
            .field("inputs", &self.tap.layout())
            .field("faults", &self.faults.count())
            .field("_stream", &self._stream)
            .finish_non_exhaustive()
    }
}

/// The inputs [`MicIn::open`] takes of a device of `width` inputs: the first
/// two, or a lone input on both sides.
fn default_inputs(width: ChannelLayout) -> [usize; 2] {
    [0, usize::from(width.count() > 1)]
}

/// What [`MicIn`] opens: the host and device, which inputs, and where the
/// capture delay is reported. `Default` is the default host's default input
/// device, its first two inputs, and a delay cell of the mic's own; build one
/// by struct update.
///
/// ```
/// use tutti_cpal::{AudioHost, DeviceSelector, MicConfig};
///
/// let config = MicConfig {
///     host: AudioHost::Jack,
///     device: DeviceSelector::named("system"),
///     inputs: Some(vec![2, 3]), // inputs 3 and 4 of the device
///     ..MicConfig::default()
/// };
/// assert_eq!(config.inputs.as_deref(), Some(&[2, 3][..]));
/// ```
#[derive(Debug, Clone)]
pub struct MicConfig {
    /// The platform host the device is opened through.
    pub host: AudioHost,
    /// The input device on it.
    pub device: DeviceSelector,
    /// The device inputs to capture, counted from 0: destination channel `i` is
    /// the device's input `inputs[i]`. `None`: the first two (a mono device's
    /// one input on both sides).
    pub inputs: Option<Vec<usize>>,
    /// Where the capture callback reports the delay between a frame's capture
    /// and its callback (`None`: a cell of the mic's own,
    /// [`MicIn::latency`]). A host that reopens its mic hands the same cell to
    /// each, so one reader follows them all.
    pub latency: Option<Arc<StreamLatency>>,
    /// The depth of the monitor ring [`MicIn::open_with_monitor`] makes, in
    /// frames: shallow on purpose, so a monitored input is heard now and not
    /// late. [`MONITOR_RING_FRAMES`] (about 10 ms at 48 kHz) by default.
    pub monitor_frames: Samples,
}

impl Default for MicConfig {
    fn default() -> Self {
        Self {
            host: AudioHost::default(),
            device: DeviceSelector::default(),
            inputs: None,
            latency: None,
            monitor_frames: Samples(MONITOR_RING_FRAMES),
        }
    }
}

impl MicIn {
    /// Opens the input device `config` names at the graph's sample rate and
    /// starts capturing its inputs. Returns once the stream is live.
    ///
    /// The rate is a parameter rather than read off the device because nothing
    /// downstream resamples: an inlet renders the mic into the graph
    /// as-is, so the device must run at `graph_rate`. A device whose supported
    /// range covers `graph_rate` is opened at it (preferring `f32` samples);
    /// otherwise its default config is used if it already matches.
    ///
    /// Asking for a rate does not prove a device produces it natively: ALSA
    /// plug devices, for one, advertise wide ranges and resample internally.
    ///
    /// # Errors
    /// [`Error::HostUnavailable`] if the host cannot be opened;
    /// [`Error::SampleRateMismatch`] if the device cannot run at
    /// `graph_rate`; [`Error::InvalidDevice`] if the device does not resolve or
    /// its configs cannot be queried; [`Error::DeviceNotAvailable`] if it has
    /// no default input config; [`Error::InvalidConfig`] for an unsupported
    /// sample format; [`Error::BuildStream`] / [`Error::PlayStream`] from
    /// CPAL; [`Error::Inputs`] for an empty list of inputs or one the device
    /// does not have.
    pub fn open(config: &MicConfig, graph_rate: SampleRate) -> Result<Self> {
        Self::open_inner(config, graph_rate, false).map(|(mic, _)| mic)
    }

    /// Opens the mic like [`open`](Self::open), and also returns the reader of
    /// a live-monitor ring fed by the same capture callback (one lane a
    /// selected input).
    ///
    /// Each captured frame goes into both the recording ring (drained by
    /// [`pull`](Source::pull)) and a shallow monitor ring. Give the
    /// monitor reader to `Graph::add_inlet` (`tutti-session`), with this mic's
    /// [`sample_rate`](Self::sample_rate), to hear the mic live, through
    /// effects if wanted, while recording the same input. The rings are
    /// independent: when one is full it drops frames without stalling the
    /// other or the capture thread.
    ///
    /// # Errors
    /// As [`open`](Self::open).
    pub fn open_with_monitor(
        config: &MicConfig,
        graph_rate: SampleRate,
    ) -> Result<(Self, InletRing)> {
        Self::open_inner(config, graph_rate, true).map(Self::with_ring)
    }

    fn with_ring((source, ring): (Self, Option<InletRing>)) -> (Self, InletRing) {
        (source, ring.expect("a monitor was asked for"))
    }

    fn open_inner(
        mic: &MicConfig,
        graph_rate: SampleRate,
        monitor: bool,
    ) -> Result<(Self, Option<InletRing>)> {
        let host = DeviceHost::open(mic.host)?;
        let device = host.device(Direction::Input, &mic.device)?;
        let inputs = mic.inputs.as_deref();
        let latency = mic.latency.clone().unwrap_or_default();
        let name = device.name().unwrap_or_default();
        let config = choose_input_config(
            device.supported_input_configs().map_err(|e| {
                Error::InvalidDevice(format!("cannot query {name:?} input configs: {e}"))
            })?,
            device.default_input_config()?,
            graph_rate,
            &name,
        )?;
        let sample_rate = device_rate(config.sample_rate())?;
        let device_inputs = ChannelLayout::from(config.channels());

        // Checked before the stream exists: an input the device lacks is an
        // error here, not silence in the callback.
        let select = match inputs {
            Some(list) => Fold::select(device_inputs, list),
            None => Fold::select(device_inputs, &default_inputs(device_inputs)),
        }?;
        let width = select.dst_layout();

        let faults = Arc::new(StreamFaults::new());
        let (reader, rec) =
            TapReader::pair(TapId(0), usize::from(width.count()), Samples(RING_FRAMES));
        let (ring, mon) = if monitor {
            // The ring carries the rate the stream runs at, so whoever takes it
            // into a graph can check it there.
            let (ring, writer) = InletRing::pair(width, mic.monitor_frames, sample_rate);
            (Some(ring), Some(writer))
        } else {
            (None, None)
        };
        let capture = Capture::new(select, device_inputs, rec, mon);

        let sample_format = config.sample_format();
        let cfg: cpal::StreamConfig = config.into();
        if !matches!(
            sample_format,
            cpal::SampleFormat::I8
                | cpal::SampleFormat::I16
                | cpal::SampleFormat::I32
                | cpal::SampleFormat::U8
                | cpal::SampleFormat::U16
                | cpal::SampleFormat::U32
                | cpal::SampleFormat::F32
                | cpal::SampleFormat::F64
        ) {
            return Err(Error::InvalidConfig(format!(
                "Unsupported input sample format: {sample_format:?}"
            )));
        }
        let cb_faults = Arc::clone(&faults);
        let cb_latency = Arc::clone(&latency);
        let owned = Owned::spawn("tutti-input-stream", move || {
            let (d, c) = (&device, &cfg);
            let stream = match sample_format {
                cpal::SampleFormat::I8 => build_input::<i8>(d, c, capture, cb_faults, cb_latency)?,
                cpal::SampleFormat::I16 => {
                    build_input::<i16>(d, c, capture, cb_faults, cb_latency)?
                }
                cpal::SampleFormat::I32 => {
                    build_input::<i32>(d, c, capture, cb_faults, cb_latency)?
                }
                cpal::SampleFormat::U8 => build_input::<u8>(d, c, capture, cb_faults, cb_latency)?,
                cpal::SampleFormat::U16 => {
                    build_input::<u16>(d, c, capture, cb_faults, cb_latency)?
                }
                cpal::SampleFormat::U32 => {
                    build_input::<u32>(d, c, capture, cb_faults, cb_latency)?
                }
                cpal::SampleFormat::F32 => {
                    build_input::<f32>(d, c, capture, cb_faults, cb_latency)?
                }
                cpal::SampleFormat::F64 => {
                    build_input::<f64>(d, c, capture, cb_faults, cb_latency)?
                }
                // Checked above, before the thread is spawned.
                _ => unreachable!("sample format checked before spawn"),
            };
            stream.play()?;
            Ok(stream)
        })?;

        let mic = Self {
            tap: reader,
            sample_rate,
            device_inputs,
            faults,
            latency,
            _stream: StreamHandle(owned),
        };
        Ok((mic, ring))
    }

    /// The faults the input stream has reported, shared with its error callback.
    ///
    /// Take it before handing the mic to a `tutti_session::Recorder`, which owns the
    /// `MicIn`. A disconnect also ends the take: the recorder's
    /// `failed()` turns `true` once the buffered frames are written. Nothing
    /// else observes the mic; a host that wants to report it polls this handle,
    /// as it does the output's.
    #[must_use]
    pub fn faults(&self) -> Arc<StreamFaults> {
        Arc::clone(&self.faults)
    }

    /// The delay between a frame's capture and the callback that delivered it,
    /// as the last callback measured it: the cell a host reads, `None` while
    /// no callback has reported one. It is the one [`MicConfig::latency`]
    /// named if it named one.
    #[must_use]
    pub fn latency(&self) -> Arc<StreamLatency> {
        Arc::clone(&self.latency)
    }

    /// Returns the rate the capture stream runs at (the `graph_rate` it was
    /// opened with). A recorder passes this to its sink so the WAV header
    /// matches the frames it is fed.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// How many inputs the device has, of which this mic delivers
    /// [`layout`](Source::layout)'s.
    #[must_use]
    pub fn device_inputs(&self) -> ChannelLayout {
        self.device_inputs
    }

    /// Which speaker each delivered channel feeds: none, they are numbered
    /// device inputs ([`Speaker::Unassigned`](tutti_types::Speaker::Unassigned)).
    /// [`Fold::between`] this to the topology a mic should be placed in.
    #[must_use]
    pub fn topology(&self) -> ChannelTopology {
        ChannelTopology::unassigned(self.tap.layout())
    }

    /// Creates a file sink that matches this mic: its sample rate, its
    /// layout (one channel a selected input), in `format`.
    ///
    /// A source and sink must agree on rate and channel count, and nothing
    /// downstream can check the rate: [`Source`] carries none, so an 8 kHz
    /// sink fed by a 48 kHz mic writes a valid WAV that plays six times too
    /// slow. Building the sink here rules that out.
    ///
    /// # Errors
    ///
    /// Whatever [`FileOut::create`] reports: the file could not be created, the
    /// format cannot be written at this width, or the header not written.
    pub fn matching_sink(
        &self,
        path: impl AsRef<std::path::Path>,
        format: Format,
    ) -> tutti_codec::Result<FileOut> {
        // Read off `Source::layout` rather than hard-coded, so this pairing
        // cannot drift from what `pull` actually hands back; a sink built
        // from a *different* width than the source reports is exactly what
        // `Recorder::start` refuses, and this helper exists to be the pairing
        // that always passes.
        FileOut::create(
            &Spec::of_width(format, Source::layout(self), self.sample_rate),
            path,
        )
    }
}

impl Source for MicIn {
    /// One channel a selected input (two for [`MicIn::open`]), whatever the
    /// device's own width is.
    fn layout(&self) -> ChannelLayout {
        self.tap.layout()
    }

    /// Pops up to `out.len() / width` FRAMES the callback has pushed.
    ///
    /// A live capture device: an empty ring means the callback has not pushed
    /// since the last pull, so [`Starved`](Pull::Starved), never
    /// [`Ended`](Pull::Ended): a consumer that stopped there would end a
    /// recording within milliseconds of starting it.
    ///
    /// [`Failed`](Pull::Failed) means the backend reported the device
    /// disconnected (any other backend error may be transient and does not fail
    /// the source) **and** the ring is empty. The fault flag is read before the
    /// ring, so frames the callback pushed before the
    /// disconnect are never hidden behind it.
    fn pull(&mut self, out: &mut [f32]) -> Pull {
        let tap = &mut self.tap;
        match self.faults.drain(|| {
            let n = tap.pop_into(out);
            (!n.is_zero()).then_some(n)
        }) {
            Drained::Item(n) => Pull::Frames(n),
            Drained::Empty => Pull::Starved,
            Drained::Failed => Pull::Failed,
        }
    }

    /// The frames the capture callback dropped on a full ring since the last
    /// call: the recording closes up over them. (Counted in the callback by the
    /// ring's writer.)
    fn take_lost(&mut self) -> Samples {
        TapReader::take_lost(&self.tap)
    }
}

/// The capture callback's state: what it selects, and where it writes.
///
/// Built on the control side, so everything the callback touches is allocated
/// before the stream runs: it only reads, folds and pushes.
struct Capture {
    /// The selection: destination channel `i` is device input `inputs[i]`.
    select: Fold,
    /// The device's interleave stride.
    stride: usize,
    /// One device frame as `f32`.
    frame: Vec<f32>,
    /// A chunk of selected frames, [`CHUNK_FRAMES`] of them.
    chunk: Vec<f32>,
    /// The recording ring's writer.
    rec: TapWriter,
    /// The monitor ring's writer, when monitoring.
    mon: Option<TapWriter>,
}

impl Capture {
    fn new(select: Fold, device: ChannelLayout, rec: TapWriter, mon: Option<TapWriter>) -> Self {
        let stride = usize::from(device.count()).max(1);
        let width = usize::from(select.dst_layout().count());
        Self {
            select,
            stride,
            frame: vec![0.0; stride],
            chunk: vec![0.0; CHUNK_FRAMES * width],
            rec,
            mon,
        }
    }

    /// Selects the inputs of every whole device frame in `data` and pushes the
    /// result to the rings, a chunk of frames at a time. A full ring refuses
    /// whole frames and counts them lost (the writer does); neither ring can
    /// stall the other or the caller. Allocation-free.
    fn block<T>(&mut self, data: &[T])
    where
        T: cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        let width = usize::from(self.select.dst_layout().count());
        for run in data.chunks(self.stride * CHUNK_FRAMES) {
            let mut frames = 0;
            for (device, selected) in run
                .chunks_exact(self.stride)
                .zip(self.chunk.chunks_exact_mut(width))
            {
                for (slot, s) in self.frame.iter_mut().zip(device) {
                    *slot = s.to_sample();
                }
                self.select.frame(&self.frame, selected);
                frames += 1;
            }
            let pushed = &self.chunk[..frames * width];
            self.rec.push_interleaved(pushed);
            // The monitor ring is shallow (~10ms) so it never builds a backlog
            // of latency, and a full one drops this chunk (the graph consumer
            // briefly fell behind); the node reads silence for those.
            if let Some(mon) = &mut self.mon {
                mon.push_interleaved(pushed);
            }
        }
    }
}

/// Build the `cpal` input stream: the callback runs [`Capture::block`] over
/// each device buffer. Alloc-free and non-blocking — a full ring drops the
/// frames (overrun) rather than stall, so neither tap can ever block the RT
/// input thread or the other tap.
///
/// # Selecting, not truncating or folding
///
/// A device's inputs are numbered, not placed, so a wide capture is not
/// matrixed (a six-input interface is not 5.1) and its leading channels are
/// not all there is: the mic delivers exactly the inputs it was opened with,
/// whatever their number, by [`Fold::select`].
///
/// # Why the scratch is allocated before the stream runs
///
/// `data: &[T]` is generic over [`cpal::SizedSample`], so the samples must be
/// converted to `f32` before the selection can see them — a per-frame convert
/// is unavoidable. **This is the RT input callback, so it must not allocate**:
/// the device frame and the chunk are `Vec`s sized once in [`Capture::new`] to
/// the device's width and the selection's, and never grown.
fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut capture: Capture,
    faults: Arc<StreamFaults>,
    latency: Arc<StreamLatency>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let delivering = Arc::clone(&faults);
    let stream = device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            delivering.delivered();
            latency.record(crate::latency::input_delay(&info.timestamp()));
            capture.block(data);
        },
        // Not exercised without a device: what it calls, `StreamFaults::record`
        // and `MicIn::pull`'s verdict, is tested on its own.
        move |err| faults.record(&err),
        None,
    )?;
    Ok(stream)
}

/// Choose an input configuration that runs at `graph_rate`, or say why none
/// can.
///
/// Free and pure so it is testable with **no device**: cpal's
/// `SupportedStreamConfigRange::new` is public, so a fixture can state a
/// device's capabilities directly.
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
        device: device_rate(fallback.sample_rate())?,
        graph: graph_rate,
    })
}

#[cfg(test)]
mod tests;
