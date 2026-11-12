//! Inputs: a capture device read into the graph, and reopened by a restart
//! ([`Session::add_input`]).
//!
//! An input is two things that belong together: a capture stream on the
//! session's audio host ([`tutti_cpal::MicIn`]) and the inlet node that copies
//! its monitor ring into the graph ([`Graph::add_inlet`](crate::Graph::add_inlet)). Both are made by one
//! call, and the node is the input's name in the graph: wire it
//! (`connect(input.node(), ..)`) like any node, to hear the device live
//! through effects.
//!
//! A capture stream runs at the rate it was opened at, and an inlet does not
//! resample, so a restart onto a device of another rate leaves every input
//! playing at the wrong speed. The session therefore keeps each input's
//! [`InputConfig`] and reopens them all after a restart, at the graph's new
//! rate, swapping each new ring into the **same node**: nothing is rewired and
//! the key a host holds stays good. An input that cannot be reopened (its
//! device is gone) is reported ([`DeviceEvent::InputReopenFailed`]) and left
//! silent until the next restart tries again.

use std::any::Any;
use std::sync::Arc;

use tutti_cpal::{AudioHost, DeviceSelector, MONITOR_RING_FRAMES, MicConfig, MicIn, StreamLatency};
use tutti_graph::{CrossfadeCurve, InletNode, InletRing};
use tutti_types::{SampleRate, Samples, Seconds};

use crate::graph::CrossfadeRefused;
use crate::{DeviceEvent, Error, Key, Result, Session, SessionEvent};

/// How long a reopened input's new ring takes over from the old one in its
/// node: the old ring is silent by then (its stream was dropped first), so
/// this only smooths the step.
const REOPEN_FADE: Seconds = Seconds(0.01);

/// What [`Session::add_input`] opens: the capture device, which of its inputs,
/// and how deep the ring that carries them into the graph is.
///
/// `Default` is the host's default input device, its first two inputs (a mono
/// device's one input on both sides) and a ring of about 10 ms. Build one with
/// the methods or by struct update.
///
/// ```
/// use tutti_cpal::DeviceSelector;
/// use tutti_session::InputConfig;
///
/// // Inputs 3 and 4 of a named interface.
/// let config = InputConfig::default()
///     .device(DeviceSelector::named("Interface"))
///     .channels(&[2, 3]);
/// assert_eq!(config.channels.as_deref(), Some(&[2, 3][..]));
/// ```
#[derive(Clone, Debug)]
pub struct InputConfig {
    /// The input device, on the session's host
    /// ([`SessionBuilder::host`](crate::SessionBuilder::host)).
    pub device: DeviceSelector,
    /// The device inputs to capture, counted from 0: channel `i` of the input
    /// is the device's input `channels[i]` (`&[2, 3]` is inputs 3 and 4 of 16,
    /// chosen with [`Fold::select`](tutti_types::Fold::select)). `None`: the
    /// first two.
    pub channels: Option<Vec<usize>>,
    /// The depth of the ring into the graph, in frames: shallow on purpose, so a
    /// monitored input is heard now and not late. A longer one rides out a
    /// longer stall of the graph's callback at that much latency.
    pub capacity: Samples,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            device: DeviceSelector::Default,
            channels: None,
            capacity: Samples(MONITOR_RING_FRAMES),
        }
    }
}

impl InputConfig {
    /// This config on `device`.
    #[must_use]
    pub fn device(mut self, device: impl Into<DeviceSelector>) -> Self {
        self.device = device.into();
        self
    }

    /// This config capturing the device inputs `channels` (counted from 0).
    #[must_use]
    pub fn channels(mut self, channels: &[usize]) -> Self {
        self.channels = Some(channels.to_vec());
        self
    }

    /// This config with a ring of `capacity` frames.
    #[must_use]
    pub fn capacity(mut self, capacity: Samples) -> Self {
        self.capacity = capacity;
        self
    }
}

/// An input the session opened ([`Session::add_input`]): the name of its
/// inlet node.
///
/// It is a plain value. The input lives as long as the session, which owns the
/// capture stream and reopens it after a restart; remove the node
/// ([`Session::remove`]) to end it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input {
    node: Key<InletNode>,
}

impl Input {
    /// The inlet node the input feeds the graph through: wire it like any
    /// node, to monitor the device through effects. It is the same node across
    /// restarts.
    #[must_use]
    pub fn node(&self) -> Key<InletNode> {
        self.node
    }
}

/// What an [`InputOpener`] opened: the ring into the graph, and the capture
/// stream's owner, dropped to close it.
pub struct OpenedInput {
    /// The ring the capture callback fills, with the rate it fills it at.
    pub ring: InletRing,
    /// Whatever keeps the capture stream running; dropping it stops capture.
    pub keep: Box<dyn Any + Send>,
}

impl std::fmt::Debug for OpenedInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedInput")
            .field("ring", &self.ring)
            .finish_non_exhaustive()
    }
}

/// Where a session's inputs come from: the sound card's capture streams
/// ([`MicIn`]), or, behind `testing`, a test's own.
pub trait InputOpener: std::fmt::Debug + Send {
    /// Opens an input as `config` describes it, at the graph's `rate`.
    ///
    /// # Errors
    ///
    /// Whatever the capture device refuses.
    fn open(&mut self, config: &MicConfig, rate: SampleRate) -> tutti_cpal::Result<OpenedInput>;
}

/// The sound card's capture streams.
#[derive(Debug)]
struct Capture;

impl InputOpener for Capture {
    fn open(&mut self, config: &MicConfig, rate: SampleRate) -> tutti_cpal::Result<OpenedInput> {
        let (mic, ring) = MicIn::open_with_monitor(config, rate)?;
        Ok(OpenedInput {
            ring,
            keep: Box::new(mic),
        })
    }
}

/// One input the session holds: what to reopen, and the stream while it runs.
struct Held {
    node: Key<InletNode>,
    config: InputConfig,
    keep: Option<Box<dyn Any + Send>>,
}

/// The session's inputs, and what opens them.
pub(crate) struct Inputs {
    host: AudioHost,
    /// Where every input's capture callback reports its delay, which the
    /// [`Device`](crate::Device) reads.
    latency: Arc<StreamLatency>,
    opener: Box<dyn InputOpener>,
    held: Vec<Held>,
}

impl std::fmt::Debug for Inputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inputs")
            .field("host", &self.host)
            .field("open", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl Inputs {
    pub(crate) fn new(host: AudioHost, opener: Option<Box<dyn InputOpener>>) -> Self {
        Self {
            host,
            latency: Arc::new(StreamLatency::new()),
            opener: opener.unwrap_or_else(|| Box::new(Capture)),
            held: Vec::new(),
        }
    }

    /// The cell the capture callbacks report into.
    pub(crate) fn latency(&self) -> Arc<StreamLatency> {
        Arc::clone(&self.latency)
    }

    fn mic(&self, config: &InputConfig) -> MicConfig {
        MicConfig {
            host: self.host,
            device: config.device.clone(),
            inputs: config.channels.clone(),
            latency: Some(Arc::clone(&self.latency)),
            monitor_frames: config.capacity,
        }
    }
}

impl Session {
    /// Opens a capture input on the session's host and puts it in the graph:
    /// the capture stream ([`tutti_cpal::MicIn`]) and the inlet node that copies
    /// it in ([`Graph::add_inlet`](crate::Graph::add_inlet)), in one call. Wire
    /// [`Input::node`] to monitor it.
    ///
    /// The session keeps the input's config and **reopens it after every
    /// restart** ([`LiveSession::restart`](crate::LiveSession::restart)), at
    /// the graph's new rate, into the same node: nothing to rewire. The capture
    /// delay it reports reads back as [`Device::latency`](crate::Device::latency)'s
    /// `input`.
    ///
    /// # Errors
    ///
    /// [`Error::Device`] when the host or the device cannot be opened, cannot
    /// run at the graph's rate, or has not the inputs asked for;
    /// [`Error::Inlet`] when the stream's rate is not the graph's.
    pub fn add_input(&mut self, config: InputConfig) -> Result<Input> {
        let rate = self.graph.prepared().sample_rate();
        let opened = self.inputs.opener.open(&self.inputs.mic(&config), rate)?;
        let node = self.graph.add_inlet(opened.ring)?;
        self.inputs.held.push(Held {
            node,
            config,
            keep: Some(opened.keep),
        });
        Ok(Input { node })
    }

    /// Reopens every input at the graph's rate, after a restart: the old stream
    /// first (a device may not be opened twice), then a new ring swapped into
    /// the same node. What cannot be reopened is reported and left silent.
    pub(crate) fn reopen_inputs(&mut self) {
        let rate = self.graph.prepared().sample_rate();
        // The new streams have reported nothing yet.
        self.inputs.latency.reset();
        let mut held = std::mem::take(&mut self.inputs.held);
        held.retain_mut(|input| match self.reopen(input, rate) {
            Ok(()) => true,
            // The host removed the node: there is no input to keep.
            Err(Error::Crossfade(CrossfadeRefused::NoSuchNode(_))) => false,
            Err(error) => {
                self.device_events
                    .push(SessionEvent::Device(DeviceEvent::InputReopenFailed {
                        input: Input { node: input.node },
                        error,
                    }));
                true
            }
        });
        self.inputs.held = held;
    }

    fn reopen(&mut self, input: &mut Held, rate: SampleRate) -> Result<()> {
        input.keep = None;
        let opened = self
            .inputs
            .opener
            .open(&self.inputs.mic(&input.config), rate)?;
        let node = self.graph.inlet_node(opened.ring)?;
        self.graph
            .crossfade(input.node, node, REOPEN_FADE, CrossfadeCurve::default())?;
        input.keep = Some(opened.keep);
        Ok(())
    }
}
