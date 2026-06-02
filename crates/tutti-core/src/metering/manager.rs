//! Central metering manager.

use super::{AtomicAmplitude, AtomicLufs, AtomicStereoAnalysis, CpuMeter, StereoAnalysisSnapshot};
use crate::compat::{Arc, HashMap, Mutex};
use crate::Ordering;
use crossbeam_channel::Receiver;
use ebur128::{EbuR128, Mode};
use ringbuf::{
    traits::{Producer, Split},
    HeapCons, HeapProd, HeapRb,
};

/// Stereo sample pair (left, right).
type StereoSample = (f32, f32);

/// Receiver for master channel metering data.
type MasterMeterRx = Arc<Mutex<Option<Receiver<StereoSample>>>>;

/// Receivers for per-channel metering data, keyed by channel index.
type ChannelMeterRxs = Arc<Mutex<HashMap<usize, Receiver<StereoSample>>>>;

/// Central metering manager for amplitude, stereo analysis, CPU, and LUFS.
pub struct MeteringManager {
    amplitude: Arc<AtomicAmplitude>,
    stereo: Arc<AtomicStereoAnalysis>,
    cpu: Arc<CpuMeter>,
    ebur128: Arc<Mutex<EbuR128>>,
    /// Lock-free snapshot of the latest LUFS / true-peak readings. Updated
    /// from the RT callback via [`publish_lufs_snapshot`]; UI reads without
    /// taking the Mutex.
    lufs_snapshot: Arc<AtomicLufs>,

    amp_enabled: Arc<crate::AtomicBool>,
    corr_enabled: Arc<crate::AtomicBool>,
    lufs_enabled: Arc<crate::AtomicBool>,

    master_rx: MasterMeterRx,
    channel_rxs: ChannelMeterRxs,

    /// Ring buffer producer for analysis tap (opt-in via enable_tap)
    tap_on: Arc<crate::AtomicBool>,
    tap_producer: Mutex<Option<HeapProd<(f32, f32)>>>,

    sample_rate: f64,
}

impl MeteringManager {
    pub fn new(sample_rate: impl Into<crate::SampleRate>) -> Self {
        let sample_rate = sample_rate.into().get();
        // `Mode::HISTOGRAM` swaps `ebur128`'s per-block energy history from an
        // unbounded `VecDeque` (grows past 5000 pre-allocated slots after
        // ~8 minutes of audio, reallocating on the RT `add_frames` path) to a
        // fixed `Box<[u64; 1000]>`. UI metering doesn't need sub-bin LRA
        // precision — the histogram quantises energies to ~0.1 dB bins,
        // which is well within EBU R128 display tolerance — and the
        // bounded footprint is what makes the `update_lufs` fast-path
        // (`metering/rt.rs`) allocation-free.
        let ebur128 = EbuR128::new(
            2,
            sample_rate as u32,
            Mode::I | Mode::S | Mode::LRA | Mode::TRUE_PEAK | Mode::HISTOGRAM,
        )
        .expect("Failed to create EBU R128 meter");

        Self {
            amplitude: Arc::new(AtomicAmplitude::new()),
            stereo: Arc::new(AtomicStereoAnalysis::new()),
            cpu: Arc::new(CpuMeter::new(sample_rate)),
            ebur128: Arc::new(Mutex::new(ebur128)),
            lufs_snapshot: Arc::new(AtomicLufs::new()),

            amp_enabled: Arc::new(crate::AtomicBool::new(false)),
            corr_enabled: Arc::new(crate::AtomicBool::new(false)),
            lufs_enabled: Arc::new(crate::AtomicBool::new(false)),

            master_rx: Arc::new(Mutex::new(None)),
            channel_rxs: Arc::new(Mutex::new(HashMap::new())),

            tap_on: Arc::new(crate::AtomicBool::new(false)),
            tap_producer: Mutex::new(None),

            sample_rate,
        }
    }

    pub fn sample_rate(&self) -> crate::SampleRate {
        crate::SampleRate(self.sample_rate)
    }

    pub fn enable_amp(&self) {
        self.amp_enabled.store(true, Ordering::Release);
    }

    pub fn disable_amp(&self) {
        self.amp_enabled.store(false, Ordering::Release);
    }

    pub fn amp_enabled(&self) -> bool {
        self.amp_enabled.load(Ordering::Acquire)
    }

    /// Returns (peak_l, peak_r, rms_l, rms_r).
    pub fn amplitude(&self) -> (f32, f32, f32, f32) {
        self.amplitude.get()
    }

    pub fn amplitude_raw(&self) -> &Arc<AtomicAmplitude> {
        &self.amplitude
    }

    pub fn enable_corr(&self) {
        self.corr_enabled.store(true, Ordering::Release);
    }

    pub fn disable_corr(&self) {
        self.corr_enabled.store(false, Ordering::Release);
    }

    pub fn corr_enabled(&self) -> bool {
        self.corr_enabled.load(Ordering::Acquire)
    }

    pub fn stereo(&self) -> StereoAnalysisSnapshot {
        self.stereo.get()
    }

    pub fn stereo_raw(&self) -> &Arc<AtomicStereoAnalysis> {
        &self.stereo
    }

    pub fn cpu(&self) -> &Arc<CpuMeter> {
        &self.cpu
    }

    pub fn enable_lufs(&self) {
        self.lufs_enabled.store(true, Ordering::Release);
    }

    pub fn disable_lufs(&self) {
        self.lufs_enabled.store(false, Ordering::Release);
    }

    pub fn lufs_enabled(&self) -> bool {
        self.lufs_enabled.load(Ordering::Acquire)
    }

    /// Integrated loudness (LUFS). RT-safe: reads from the lock-free snapshot
    /// published by the audio thread's `update_lufs` path.
    pub fn lufs(&self) -> crate::Result<f64> {
        self.lufs_snapshot.integrated()
    }

    /// Short-term loudness (3-second window, LUFS). RT-safe snapshot read.
    pub fn lufs_short(&self) -> crate::Result<f64> {
        self.lufs_snapshot.short_term()
    }

    /// Loudness range (LRA) in LU. RT-safe snapshot read.
    pub fn lufs_range(&self) -> crate::Result<f64> {
        self.lufs_snapshot.range()
    }

    /// True peak level for a channel (0=left, 1=right) in dBTP. RT-safe
    /// snapshot read.
    pub fn true_peak(&self, channel: u32) -> crate::Result<f64> {
        self.lufs_snapshot.true_peak(channel)
    }

    /// Resets LUFS measurement history. Not RT-safe (takes the EBU R128
    /// `Mutex`); call from a UI / control-plane thread.
    pub fn reset_lufs(&self) {
        self.ebur128.lock().reset();
        self.lufs_snapshot.reset();
    }

    /// Lock-free LUFS snapshot (used by the RT updater and UI readers).
    pub fn lufs_snapshot(&self) -> &Arc<AtomicLufs> {
        &self.lufs_snapshot
    }

    /// Internal: the EBU R128 meter behind its `Mutex`. Used by the RT
    /// updater via `try_lock`; do not expose publicly.
    pub(super) fn ebur128(&self) -> &Arc<Mutex<EbuR128>> {
        &self.ebur128
    }

    /// Sets the receiver for master channel metering data.
    pub fn set_master_consumer(&self, rx: Receiver<StereoSample>) {
        *self.master_rx.lock() = Some(rx);
    }

    /// Takes the master consumer receiver (returns receiver and sample rate).
    pub fn take_master_consumer(&self) -> Option<(Receiver<StereoSample>, crate::SampleRate)> {
        self.master_rx
            .lock()
            .take()
            .map(|r| (r, crate::SampleRate(self.sample_rate)))
    }

    /// Takes all channel consumer receivers.
    pub fn take_channel_consumers(&self) -> HashMap<usize, Receiver<StereoSample>> {
        core::mem::take(&mut *self.channel_rxs.lock())
    }

    /// Creates a new channel buffer and returns the sender for the audio thread.
    pub fn create_channel_buffer(&self, channel: usize) -> crossbeam_channel::Sender<StereoSample> {
        let (tx, rx) = crossbeam_channel::bounded(8192);
        self.channel_rxs.lock().insert(channel, rx);
        tx
    }

    /// Removes a channel buffer.
    pub fn remove_channel_buffer(&self, channel: usize) {
        self.channel_rxs.lock().remove(&channel);
    }

    /// Enable the analysis tap and return the consumer end of the ring buffer.
    ///
    /// Creates a SPSC ring buffer (~3 seconds at 44.1kHz). The audio callback
    /// pushes stereo pairs via `push_tap()`. The caller owns the
    /// consumer and drains it from an analysis thread.
    pub fn enable_tap(&self) -> HeapCons<(f32, f32)> {
        let capacity = 131072; // ~3s at 44.1kHz
        let rb = HeapRb::<(f32, f32)>::new(capacity);
        let (prod, cons) = rb.split();
        *self.tap_producer.lock() = Some(prod);
        self.tap_on.store(true, Ordering::Release);
        cons
    }

    /// Disable the analysis tap and drop the producer.
    pub fn disable_tap(&self) {
        self.tap_on.store(false, Ordering::Release);
        *self.tap_producer.lock() = None;
    }

    /// Returns whether the analysis tap is enabled.
    pub fn tap_enabled(&self) -> bool {
        self.tap_on.load(Ordering::Acquire)
    }

    /// Push interleaved stereo samples to the analysis tap (RT-safe).
    ///
    /// Called from the audio callback. Drops samples if the buffer is full
    /// or the lock is contended (never blocks). No-op if the tap is disabled.
    #[inline]
    pub fn push_tap(&self, output: &[f32], frames: usize) {
        if !self.tap_on.load(Ordering::Acquire) {
            return;
        }
        // try_lock: skip this callback if the producer is being swapped
        if let Some(ref mut guard) = self.tap_producer.try_lock() {
            if let Some(ref mut prod) = **guard {
                output.chunks_exact(2).take(frames).for_each(|ch| {
                    let _ = prod.try_push((ch[0], ch[1]));
                });
            }
        }
    }
}
