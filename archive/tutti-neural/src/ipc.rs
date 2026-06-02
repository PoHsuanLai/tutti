//! Lock-free IPC between the audio thread and the engine thread.
//!
//! # What lives here
//!
//! Five groups of primitives, each with a narrow purpose:
//!
//! | Group | Types | Role |
//! | ----- | ----- | ---- |
//! | Sample slot | [`Slot`], [`SlotReader`], [`SlotWriter`] | Deliver one buffer of processed audio from the engine back to the audio thread. Backed by [`arc_swap`]. |
//! | Reply channel | [`Ask`], [`Reply`] | One-shot reply pair. `send` consumes `Reply`, `recv` consumes `Ask` — so the bounded-1 invariant is structural. |
//! | Event shape | [`Event`], [`Command`], [`Request`], [`Response`] | What flows on the engine's input channel: control plane (commands) + data plane (requests). |
//! | DDSP output | [`ControlParams`], [`tensor_to_params`] | How `Synth` receives per-sample `f0`/amplitude arrays. |
//! | Audio-thread helpers | [`ArcPool`], [`submit`] | Round-robin `Arc<[f32]>` allocator and the `try_send` wrapper for `Event::Req`. |
//!
//! # Audio-thread guarantees
//!
//! Everything here is safe to call from an audio callback:
//!
//! - `SlotReader::read` / `has_output` — non-blocking atomic reads.
//! - `arc_pool` closures — pool reuse, no heap allocation in the hot path.
//! - `submit` — `try_send` on a bounded channel; returns `false` if full.
//!
//! The one piece that *can* block is [`Ask::recv`], which the audio thread
//! should never call. [`Engine`](crate::Engine) method calls ride on `Ask`
//! and belong in setup code.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use crossbeam_channel::{bounded, Receiver, RecvError, RecvTimeoutError, Sender, TrySendError};

use std::path::PathBuf;

use crate::backend::{BackendError, ProbeReport};
use crate::model_id::ModelId;

/// Loaded-model descriptor returned by [`Engine::load_model`](crate::Engine::load_model).
///
/// Pairs the newly-assigned [`ModelId`] with the probe report classifying
/// the model's realtime-safety and shape.
#[derive(Debug, Clone)]
pub struct LoadedModel {
    pub id: ModelId,
    pub report: ProbeReport,
}

/// Shared state for one audio node's output buffer.
///
/// One [`Slot`] per [`Effect`](crate::Effect) or `Synth`. The engine writes
/// through a [`SlotWriter`]; the audio thread reads through a [`SlotReader`].
/// `ArcSwap` makes the whole struct `Sync` with no `unsafe`.
pub struct Slot {
    output: ArcSwap<Option<Vec<f32>>>,
    ready: AtomicBool,
    pub channels: usize,
    pub buffer_size: usize,
}

impl Slot {
    fn new(channels: usize, buffer_size: usize) -> Arc<Self> {
        Arc::new(Self {
            output: ArcSwap::new(Arc::new(None)),
            ready: AtomicBool::new(false),
            channels,
            buffer_size,
        })
    }
}

/// Audio-thread handle. Holds a local read buffer so output can be streamed
/// one sample at a time.
pub struct SlotReader {
    shared: Arc<Slot>,
    read_buf: Option<Arc<Vec<f32>>>,
    read_pos: usize,
}

/// Engine-thread handle. Writes processed output.
pub struct SlotWriter {
    shared: Arc<Slot>,
}

/// Construct a matched pair of handles.
pub fn slot(channels: usize, buffer_size: usize) -> (SlotReader, SlotWriter) {
    let shared = Slot::new(channels, buffer_size);
    let reader = SlotReader {
        shared: Arc::clone(&shared),
        read_buf: None,
        read_pos: 0,
    };
    let writer = SlotWriter { shared };
    (reader, writer)
}

impl SlotReader {
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    pub fn buffer_size(&self) -> usize {
        self.shared.buffer_size
    }

    /// Spawn a fresh writer that shares this reader's output channel. Used
    /// by [`SlotSink`](crate::effect_node::SlotSink) to mint a new writer
    /// for every outgoing request without consuming the reader.
    pub fn new_writer(&self) -> SlotWriter {
        SlotWriter {
            shared: Arc::clone(&self.shared),
        }
    }

    /// `true` iff the engine has written output that the audio thread has
    /// not yet fully consumed.
    #[inline]
    pub fn has_output(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire)
    }

    /// Read one output sample for `channel`. Returns `0.0` when no output.
    #[inline]
    pub fn read(&mut self, channel: usize) -> f32 {
        if self.read_buf.is_none() {
            if !self.shared.ready.load(Ordering::Acquire) {
                return 0.0;
            }
            let arc = self.shared.output.swap(Arc::new(None));
            self.shared.ready.store(false, Ordering::Release);
            match arc.as_ref().as_ref() {
                Some(buf) => {
                    self.read_buf = Some(Arc::new(buf.clone()));
                    self.read_pos = 0;
                }
                None => return 0.0,
            }
        }
        let buf = self.read_buf.as_ref().unwrap();
        let idx = self.read_pos * self.shared.channels + channel;
        let sample = buf.get(idx).copied().unwrap_or(0.0);
        if channel + 1 == self.shared.channels {
            self.read_pos += 1;
            if self.read_pos >= self.shared.buffer_size {
                self.read_buf = None;
                self.read_pos = 0;
            }
        }
        sample
    }
}

impl SlotWriter {
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    pub fn buffer_size(&self) -> usize {
        self.shared.buffer_size
    }

    /// Publish a processed buffer. Clamped to `channels * buffer_size`.
    pub fn write(&mut self, data: &[f32]) {
        let total = self.shared.channels * self.shared.buffer_size;
        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&data[..data.len().min(total)]);
        self.shared.output.store(Arc::new(Some(buf)));
        self.shared.ready.store(true, Ordering::Release);
    }
}

/// DDSP-style per-sample control parameters from an inference result.
///
/// When the engine's inference output targets a `Synth` rather than an
/// `Effect`, it's decoded into `ControlParams` via [`tensor_to_params`]
/// before reaching the audio thread.
#[derive(Debug, Clone, Default)]
pub struct ControlParams {
    pub f0: Vec<f32>,
    pub amplitudes: Vec<f32>,
}

/// Pack a flat output tensor into [`ControlParams`]. If the tensor is at
/// least `2 * buffer_size` long, the first half is `f0`, the rest
/// `amplitudes`. Shorter tensors split 50/50 and pad defaults.
pub fn tensor_to_params(data: &[f32], buffer_size: usize) -> ControlParams {
    if data.len() >= buffer_size * 2 {
        ControlParams {
            f0: data[..buffer_size].to_vec(),
            amplitudes: data[buffer_size..buffer_size * 2].to_vec(),
        }
    } else if data.is_empty() {
        ControlParams::default()
    } else {
        let half = data.len() / 2;
        let mut f0 = data[..half].to_vec();
        let mut amplitudes = data[half..].to_vec();
        f0.resize(buffer_size, 440.0);
        amplitudes.resize(buffer_size, 0.0);
        ControlParams { f0, amplitudes }
    }
}

/// One-shot reply sender.
///
/// Consumed on [`send`](Reply::send), so the bounded-1 nature is enforced
/// at the type level — you can't double-send or forget to send. Paired with
/// an [`Ask`] of the same type via [`ask()`](ask).
pub struct Reply<T>(Sender<T>);

impl<T> Reply<T> {
    pub fn send(self, value: T) {
        let _ = self.0.try_send(value);
    }
}

/// One-shot receiver paired with a [`Reply`].
pub struct Ask<T>(Receiver<T>);

impl<T> Ask<T> {
    pub fn recv(self) -> Result<T, RecvError> {
        self.0.recv()
    }

    pub fn recv_timeout(self, d: Duration) -> Result<T, RecvTimeoutError> {
        self.0.recv_timeout(d)
    }
}

pub fn ask<T>() -> (Ask<T>, Reply<T>) {
    let (tx, rx) = bounded(1);
    (Ask(rx), Reply(tx))
}

/// What flows on the engine's input channel.
///
/// Control plane ([`Command`]) and data plane ([`Request`]) share one FIFO
/// so `Command::Unload` cannot race past an in-flight request for the same
/// model.
pub enum Event {
    Cmd(Command),
    Req(Request),
}

/// Control-plane operations. All except [`Command::Shutdown`] carry a
/// [`Reply`]; callers block on the paired [`Ask`].
pub enum Command {
    /// Load a model from disk, probe it for RT-safety, return a
    /// [`LoadedModel`] descriptor. Engine thread owns both the load and
    /// probe — no user code runs on the engine thread.
    Load {
        path: PathBuf,
        reply: Reply<Result<LoadedModel, BackendError>>,
    },
    /// Unload a previously-loaded model, freeing its backend resources.
    Unload {
        id: ModelId,
        reply: Reply<Result<(), BackendError>>,
    },
    Shutdown,
}

/// Data-plane request: run inference for `id` on `input`, deliver the
/// result via [`Response`].
pub struct Request {
    /// Which registered model to run.
    pub id: ModelId,
    /// Flat input buffer. Shape is [`Self::shape`]; `input.len() ==
    /// shape.batch * shape.features`.
    pub input: Arc<[f32]>,
    /// Logical shape of [`Self::input`]. Backends pass this through to the
    /// tensor reshape at inference time.
    pub shape: Shape,
    /// Where the engine dispatches the result.
    pub resp: Response,
}

/// `[batch, features]` of a flat [`Request::input`] buffer.
///
/// Stored as two `usize`s rather than `[usize; 2]` so that call sites read
/// `shape.batch` / `shape.features` rather than `shape[0]` / `shape[1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub batch: usize,
    pub features: usize,
}

impl Shape {
    /// Convenience constructor for `Shape { batch, features }`.
    pub const fn new(batch: usize, features: usize) -> Self {
        Self { batch, features }
    }

    /// `[batch, features]` form the backend's tensor API expects.
    pub fn as_array(self) -> [usize; 2] {
        [self.batch, self.features]
    }
}

impl From<[usize; 2]> for Shape {
    fn from([batch, features]: [usize; 2]) -> Self {
        Self { batch, features }
    }
}

/// Where an inference result should be delivered.
pub enum Response {
    /// Write the flat output into a [`SlotWriter`], to be read by the
    /// audio-thread [`SlotReader`] paired with it.
    Audio(SlotWriter),
    /// Decode the flat output into [`ControlParams`] (size `buffer_size`
    /// halves) and send it on `tx`. Used by `Synth`.
    Params {
        tx: Sender<ControlParams>,
        buffer_size: usize,
    },
}

/// Try-send a request, discarding it if the channel is full.
/// Returns `true` on success.
#[inline]
pub fn submit(tx: &Sender<Event>, req: Request) -> bool {
    match tx.try_send(Event::Req(req)) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::trace!("neural event channel full, dropping request");
            false
        }
        Err(TrySendError::Disconnected(_)) => {
            tracing::warn!("neural engine disconnected");
            false
        }
    }
}

/// Round-robin pool of `slot_count` buffers of `buffer_size` `f32`s.
///
/// [`fill`](ArcPool::fill) writes `data` into the next available slot and
/// hands back an `Arc<[f32]>` pointing at it. When every slot is still held
/// elsewhere — i.e. the inference thread hasn't caught up — `fill` returns
/// `None` so the audio thread can drop the request rather than allocate.
///
/// `Send + Sync`: holds owned `Arc`s and a `usize`, no interior mutability,
/// no closure capture. Audio nodes embed one inline per inference path.
pub struct ArcPool {
    slots: Vec<Option<Arc<[f32]>>>,
    next: usize,
}

impl ArcPool {
    pub fn new(slot_count: usize, buffer_size: usize) -> Self {
        Self {
            slots: (0..slot_count)
                .map(|_| Some(Arc::<[f32]>::from(vec![0.0f32; buffer_size])))
                .collect(),
            next: 0,
        }
    }

    /// Slot count, fixed at construction.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Copy `data` into the next free slot. Returns `Some(Arc)` on success,
    /// `None` if every slot is still outstanding (audio path should drop the
    /// request rather than block or allocate).
    #[inline]
    pub fn fill(&mut self, data: &[f32]) -> Option<Arc<[f32]>> {
        let count = self.slots.len();
        for _ in 0..count {
            let idx = self.next;
            self.next = (self.next + 1) % count;
            if let Some(arc) = self.slots[idx].take() {
                if Arc::strong_count(&arc) == 1 {
                    let mut arc = arc;
                    let buf: &mut [f32] = Arc::make_mut(&mut arc);
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    let result = Arc::clone(&arc);
                    self.slots[idx] = Some(arc);
                    return Some(result);
                } else {
                    self.slots[idx] = Some(arc);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slot_write_read() {
        let (mut r, mut w) = slot(2, 4);
        assert_eq!(r.channels(), 2);
        assert_eq!(r.buffer_size(), 4);
        assert!(!r.has_output());

        let data: Vec<f32> = (0..8).map(|i| i as f32 * 0.1).collect();
        w.write(&data);
        assert!(r.has_output());

        for i in 0..4 {
            let l = r.read(0);
            let rgt = r.read(1);
            let expected_l = i as f32 * 2.0 * 0.1;
            let expected_r = (i as f32 * 2.0 + 1.0) * 0.1;
            assert!((l - expected_l).abs() < 1e-6, "ch0 frame {i}");
            assert!((rgt - expected_r).abs() < 1e-6, "ch1 frame {i}");
        }
        assert!(!r.has_output());
    }

    #[test]
    fn test_slot_read_returns_silence_when_empty() {
        let (mut r, _w) = slot(2, 4);
        assert_eq!(r.read(0), 0.0);
        assert_eq!(r.read(1), 0.0);
    }

    #[test]
    fn test_ask_reply_oneshot() {
        let (ask, reply) = ask::<u32>();
        reply.send(42);
        assert_eq!(ask.recv().unwrap(), 42);
    }

    #[test]
    fn test_ask_reply_timeout() {
        let (ask, reply) = ask::<u32>();
        drop(reply);
        let err = ask.recv_timeout(Duration::from_millis(10)).unwrap_err();
        assert!(matches!(err, RecvTimeoutError::Disconnected));
    }

    #[test]
    fn test_ask_no_reply_times_out() {
        let (ask, _reply) = ask::<u32>();
        let err = ask.recv_timeout(Duration::from_millis(5)).unwrap_err();
        assert!(matches!(err, RecvTimeoutError::Timeout));
    }

    #[test]
    fn test_arc_pool_reuse() {
        let mut pool = ArcPool::new(4, 8);
        let data = vec![1.0f32; 8];
        for _ in 0..16 {
            let arc = pool.fill(&data).expect("slot available");
            drop(arc);
        }
    }

    #[test]
    fn test_arc_pool_exhausts_when_outstanding() {
        let mut pool = ArcPool::new(2, 4);
        let data = vec![1.0f32; 4];
        let _held1 = pool.fill(&data).unwrap();
        let _held2 = pool.fill(&data).unwrap();
        assert!(pool.fill(&data).is_none(), "all slots should be taken");
    }

    #[test]
    fn test_tensor_to_params_full() {
        let data = vec![440.0, 440.0, 0.5, 0.5];
        let p = tensor_to_params(&data, 2);
        assert_eq!(p.f0, vec![440.0, 440.0]);
        assert_eq!(p.amplitudes, vec![0.5, 0.5]);
    }

    #[test]
    fn test_tensor_to_params_empty() {
        let p = tensor_to_params(&[], 512);
        assert!(p.f0.is_empty());
        assert!(p.amplitudes.is_empty());
    }

    #[test]
    fn test_submit_full_channel_returns_false() {
        let (tx, _rx) = bounded::<Event>(1);
        let (_r, w) = slot(2, 4);
        let req1 = Request {
            id: ModelId::new(),
            input: Arc::from([1.0f32].as_slice()),
            shape: Shape::new(1, 1),
            resp: Response::Audio(w),
        };
        assert!(submit(&tx, req1));

        let (_r2, w2) = slot(2, 4);
        let req2 = Request {
            id: ModelId::new(),
            input: Arc::from([1.0f32].as_slice()),
            shape: Shape::new(1, 1),
            resp: Response::Audio(w2),
        };
        assert!(
            !submit(&tx, req2),
            "second submit should fail — channel full"
        );
    }
}
