//! Lock-free ring buffers for audio streaming.

use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use std::path::PathBuf;
use std::sync::Arc;
use tutti_core::{AtomicU64, Ordering};

use super::command::RegionId;

/// ringbuf `HeapProd` wrapper that is `Send + Sync` when `T: Send`.
///
/// ringbuf conservatively doesn't impl Send/Sync for its SPSC handles.
/// This wrapper concentrates the unsafe impls in one place.
struct SendProd<T>(HeapProd<T>);

// SAFETY: HeapProd is designed for single-producer cross-thread use.
// T: Send ensures the element type is safe to send across threads.
unsafe impl<T: Send> Send for SendProd<T> {}
unsafe impl<T: Send> Sync for SendProd<T> {}

impl<T> SendProd<T> {
    fn new(prod: HeapProd<T>) -> Self {
        Self(prod)
    }
    fn try_push(&mut self, val: T) -> Result<(), T> {
        self.0.try_push(val)
    }
    fn vacant_len(&self) -> usize {
        self.0.vacant_len()
    }
    fn capacity(&self) -> core::num::NonZeroUsize {
        self.0.capacity()
    }
}

/// ringbuf `HeapCons` wrapper that is `Send + Sync` when `T: Send`.
struct SendCons<T>(HeapCons<T>);

// SAFETY: HeapCons is designed for single-consumer cross-thread use.
unsafe impl<T: Send> Send for SendCons<T> {}
unsafe impl<T: Send> Sync for SendCons<T> {}

impl<T> SendCons<T> {
    fn new(cons: HeapCons<T>) -> Self {
        Self(cons)
    }
    fn try_pop(&mut self) -> Option<T> {
        self.0.try_pop()
    }
    fn occupied_len(&self) -> usize {
        self.0.occupied_len()
    }
}

pub(crate) struct RegionMeta {
    region_id: RegionId,
    file_path: PathBuf,
    file_position: AtomicU64,
}

impl RegionMeta {
    pub fn file_position(&self) -> u64 {
        self.file_position.load(Ordering::Relaxed)
    }

    pub fn set_file_position(&self, pos: u64) {
        self.file_position.store(pos, Ordering::Relaxed);
    }
}

pub(crate) struct RegionWriter {
    prod: SendProd<(f32, f32)>,
    meta: Arc<RegionMeta>,
}

impl RegionWriter {
    pub fn file_position(&self) -> u64 {
        self.meta.file_position()
    }

    pub fn set_file_position(&self, pos: u64) {
        self.meta.set_file_position(pos);
    }

    pub fn write_space(&self) -> usize {
        self.prod.vacant_len()
    }

    pub fn capacity(&self) -> usize {
        self.prod.capacity().get()
    }

    pub fn write(&mut self, samples: &[(f32, f32)]) -> usize {
        let mut written = 0;
        for &sample in samples {
            if self.prod.try_push(sample).is_ok() {
                written += 1;
            } else {
                break;
            }
        }
        written
    }

    /// Write samples in reverse order (for reverse playback).
    /// Samples are taken from the end of the slice first.
    pub fn write_reversed(&mut self, samples: &[(f32, f32)]) -> usize {
        let mut written = 0;
        for &sample in samples.iter().rev() {
            if self.prod.try_push(sample).is_ok() {
                written += 1;
            } else {
                break;
            }
        }
        written
    }

    pub fn file_path(&self) -> &PathBuf {
        &self.meta.file_path
    }

    pub(crate) fn region_id(&self) -> RegionId {
        self.meta.region_id
    }
}

pub struct RegionReader {
    cons: SendCons<(f32, f32)>,
    read_position: Arc<AtomicU64>,
    region_id: RegionId,
}

impl RegionReader {
    pub(crate) fn region_id(&self) -> RegionId {
        self.region_id
    }

    /// Read the next sample from the buffer.
    /// Returns None if buffer is empty (underrun).
    #[inline]
    pub fn read(&mut self) -> Option<(f32, f32)> {
        self.cons.try_pop().inspect(|_| {
            self.read_position.fetch_add(1, Ordering::Relaxed);
        })
    }

    /// Clear all buffered samples without processing them.
    /// Used for loop resets — much faster than draining one-by-one.
    pub fn clear(&mut self) {
        let count = self.cons.occupied_len();
        for _ in 0..count {
            let _ = self.cons.try_pop();
        }
        self.read_position
            .fetch_add(count as u64, Ordering::Relaxed);
    }

    pub fn available(&self) -> usize {
        self.cons.occupied_len()
    }

    /// Get a shared handle to the read position for lock-free access.
    pub(crate) fn read_position_shared(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.read_position)
    }
}

pub(crate) struct RegionBuffer;

impl RegionBuffer {
    pub(crate) fn with_capacity(
        region_id: RegionId,
        file_path: PathBuf,
        capacity: usize,
    ) -> (RegionWriter, RegionReader) {
        let capacity = capacity.max(4096);

        let rb = HeapRb::<(f32, f32)>::new(capacity);
        let (prod, cons) = rb.split();

        let meta = Arc::new(RegionMeta {
            region_id,
            file_path,
            file_position: AtomicU64::new(0),
        });

        let producer = RegionWriter {
            prod: SendProd::new(prod),
            meta: meta.clone(),
        };

        let consumer = RegionReader {
            cons: SendCons::new(cons),
            read_position: Arc::new(AtomicU64::new(0)),
            region_id,
        };

        (producer, consumer)
    }
}

pub(crate) struct CaptureMeta {
    file_path: PathBuf,
    frames_written: AtomicU64,
    frames_captured: AtomicU64,
    frames_dropped: AtomicU64,
}

impl CaptureMeta {
    fn add_frames_written(&self, count: u64) {
        self.frames_written.fetch_add(count, Ordering::Relaxed);
    }

    fn add_frames_captured(&self, count: u64) {
        self.frames_captured.fetch_add(count, Ordering::Relaxed);
    }

    fn add_frames_dropped(&self, count: u64) {
        self.frames_dropped.fetch_add(count, Ordering::Relaxed);
    }

    fn frames_dropped(&self) -> u64 {
        self.frames_dropped.load(Ordering::Relaxed)
    }
}

pub struct CaptureWriter {
    prod: SendProd<(f32, f32)>,
    meta: Arc<CaptureMeta>,
}

impl CaptureWriter {
    pub fn write_space(&self) -> usize {
        self.prod.vacant_len()
    }

    #[inline]
    pub fn write(&mut self, sample: (f32, f32)) -> bool {
        if self.prod.try_push(sample).is_ok() {
            self.meta.add_frames_captured(1);
            true
        } else {
            self.meta.add_frames_dropped(1);
            false
        }
    }

    /// Frames dropped because the capture ring was full when audio tried to
    /// push. Nonzero means overruns occurred and the recording lost samples.
    pub fn frames_dropped(&self) -> u64 {
        self.meta.frames_dropped()
    }

    pub fn file_path(&self) -> &PathBuf {
        &self.meta.file_path
    }
}

pub(crate) struct CaptureReader {
    cons: SendCons<(f32, f32)>,
    meta: Arc<CaptureMeta>,
}

impl std::fmt::Debug for CaptureReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureReader").finish_non_exhaustive()
    }
}

impl CaptureReader {
    pub(crate) fn available(&self) -> usize {
        self.cons.occupied_len()
    }

    pub(crate) fn read_into(&mut self, buffer: &mut [(f32, f32)]) -> usize {
        let mut read = 0;
        for slot in buffer.iter_mut() {
            if let Some(sample) = self.cons.try_pop() {
                *slot = sample;
                read += 1;
            } else {
                break;
            }
        }
        read
    }

    pub(crate) fn add_frames_written(&self, count: u64) {
        self.meta.add_frames_written(count);
    }

    /// Frames dropped by the producer due to a full ring (overruns).
    pub(crate) fn frames_dropped(&self) -> u64 {
        self.meta.frames_dropped()
    }
}

pub(crate) struct CaptureBuffer;

impl CaptureBuffer {
    #[allow(clippy::new_ret_no_self)]
    pub(crate) fn new(
        file_path: PathBuf,
        sample_rate: f64,
        buffer_size_ms: f32,
    ) -> (CaptureWriter, CaptureReader) {
        let capacity = ((buffer_size_ms / 1000.0 * sample_rate as f32) as usize).max(4096);

        let rb = HeapRb::<(f32, f32)>::new(capacity);
        let (prod, cons) = rb.split();

        let meta = Arc::new(CaptureMeta {
            file_path,
            frames_written: AtomicU64::new(0),
            frames_captured: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
        });

        let producer = CaptureWriter {
            prod: SendProd::new(prod),
            meta: Arc::clone(&meta),
        };

        let consumer = CaptureReader {
            cons: SendCons::new(cons),
            meta,
        };

        (producer, consumer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_region_buffer_creation() {
        let region_id = RegionId(1);
        let capacity = (100.0 / 1000.0 * 44100.0) as usize; // 100ms buffer at 44.1kHz
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(region_id, PathBuf::from("test.wav"), capacity);

        // Write some samples
        let samples: Vec<_> = (0..100)
            .map(|i| (i as f32 / 100.0, i as f32 / 100.0))
            .collect();
        let written = prod.write(&samples);
        assert_eq!(written, 100);

        // Read them back
        let sample = cons.read().unwrap();
        assert_eq!(sample, (0.0, 0.0));
    }

    #[test]
    fn test_buffer_full() {
        let region_id = RegionId(1);
        let (mut prod, _) = RegionBuffer::with_capacity(
            region_id,
            PathBuf::from("test.wav"),
            10, // Tiny buffer (will be clamped to 4096)
        );

        // Fill the buffer
        let samples: Vec<_> = (0..4096).map(|i| (i as f32, i as f32)).collect();
        let written = prod.write(&samples);
        assert!(written <= 4096);
    }

    #[test]
    fn test_clear_allows_refill() {
        let region_id = RegionId(1);
        let capacity = 100;
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(region_id, PathBuf::from("test.wav"), capacity);

        // Fill buffer with "section A" data (values 0.0 - 0.99)
        let section_a: Vec<_> = (0..50)
            .map(|i| (i as f32 / 100.0, i as f32 / 100.0))
            .collect();
        let written = prod.write(&section_a);
        assert_eq!(written, 50);

        // Verify we can read section A
        let first = cons.read().unwrap();
        assert!((first.0 - 0.0).abs() < 0.001, "First sample should be ~0.0");

        // Clear the buffer (simulating a seek)
        cons.clear();

        // After clear, write_space should be available for producer
        let write_space_after_clear = prod.write_space();
        assert!(
            write_space_after_clear > 0,
            "Producer should have write space after consumer clear, got {}",
            write_space_after_clear
        );

        // Write "section B" data (values 1.0 - 1.49)
        let section_b: Vec<_> = (0..50)
            .map(|i| (1.0 + i as f32 / 100.0, 1.0 + i as f32 / 100.0))
            .collect();
        let written_b = prod.write(&section_b);
        assert!(written_b > 0, "Should be able to write after clear");

        // Read from consumer - should get section B data
        let sample_b = cons.read().unwrap();
        assert!(
            sample_b.0 >= 1.0,
            "After clear and refill, should read section B (>=1.0), got {}",
            sample_b.0
        );
    }

    /// Test full buffer clear and refill scenario (simulates seek)
    #[test]
    fn test_full_buffer_seek_simulation() {
        let region_id = RegionId(1);
        // Use a larger buffer to match more realistic scenarios
        let capacity = 4096;
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(region_id, PathBuf::from("test.wav"), capacity);

        // Fill buffer completely with "220Hz-like" data (low values)
        let low_freq: Vec<_> = (0..4096)
            .map(|i| {
                let phase = (i as f32 * 0.03).sin(); // ~220Hz pattern
                (phase, phase)
            })
            .collect();
        let written_low = prod.write(&low_freq);
        eprintln!("Wrote {} low-freq samples", written_low);

        // Read a few samples to simulate audio playback
        for _ in 0..100 {
            let _ = cons.read();
        }

        let write_space_before = prod.write_space();
        eprintln!("Write space before clear: {}", write_space_before);

        // Clear the buffer (simulating seek)
        cons.clear();

        let write_space_after = prod.write_space();
        eprintln!("Write space after clear: {}", write_space_after);

        // The key assertion: producer should see MORE write space after clear
        assert!(
            write_space_after > write_space_before,
            "Producer write space should increase after clear: before={}, after={}",
            write_space_before,
            write_space_after
        );

        // Refill with "880Hz-like" data (high values)
        let high_freq: Vec<_> = (0..write_space_after)
            .map(|i| {
                let phase = (i as f32 * 0.125).sin(); // ~880Hz pattern
                (phase, phase)
            })
            .collect();
        let written_high = prod.write(&high_freq);
        eprintln!("Wrote {} high-freq samples after clear", written_high);

        // Read should get high-freq data, not low-freq
        let sample = cons.read().unwrap();

        // First sample of high-freq (i=0): sin(0) = 0.0
        // Second sample: sin(0.125) ≈ 0.125
        // Compare to low-freq first sample: sin(0) = 0.0, second: sin(0.03) ≈ 0.03

        let sample2 = cons.read().unwrap();
        let sample3 = cons.read().unwrap();
        let sample_diff = sample3.0 - sample2.0;

        eprintln!("Sample values: {:?}, {:?}, {:?}", sample, sample2, sample3);
        eprintln!("Diff between samples: {}", sample_diff);

        // High freq should have larger differences between samples
        // Low freq: sin(0.03) - sin(0) ≈ 0.03
        // High freq: sin(0.25) - sin(0.125) ≈ 0.12
        assert!(
            sample_diff.abs() > 0.05,
            "After refill, should get high-freq data with larger sample diff, got {}",
            sample_diff.abs()
        );
    }

    #[test]
    fn test_capture_overrun_records_drops() {
        // Small ring (clamped to 4096) with no consumer draining it.
        let (mut writer, reader) =
            CaptureBuffer::new(PathBuf::from("test.wav"), 44100.0, 0.0);

        let capacity = writer.write_space();
        assert!(capacity > 0);
        assert_eq!(writer.frames_dropped(), 0);

        // Push exactly enough to fill the ring; all should succeed.
        for _ in 0..capacity {
            assert!(writer.write((0.0, 0.0)));
        }
        assert_eq!(writer.frames_dropped(), 0);

        // Overrun: nothing is draining, so these must be dropped.
        let overrun = 100;
        for _ in 0..overrun {
            assert!(!writer.write((0.0, 0.0)));
        }

        assert_eq!(writer.frames_dropped(), overrun as u64);
        // The reader observes the same shared counter.
        assert_eq!(reader.frames_dropped(), overrun as u64);
    }
}
