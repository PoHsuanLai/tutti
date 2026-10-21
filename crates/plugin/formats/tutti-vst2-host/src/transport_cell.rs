//! The transport snapshot the plugin reads back via `audioMasterGetTime`: a
//! sequence cell ([`SeqWriter`] / [`SeqReader`]) holding a `TimeInfo`'s
//! fields. `TimeInfo` is 88 bytes of `Copy` data and readers take it by
//! value, so the cell is overwritten in place: wait-free reads,
//! allocation-free writes.
//!
//! Not an `ArcSwap`: the audio thread is the *writer* here (once per block),
//! and `store` would allocate the new snapshot and free the retired one inside
//! the callback, which the audio thread must never do.
//! `ArcLend` is control-thread→audio-thread and also takes ownership of a
//! fresh value, so it does not fit either. The sequence lock is
//! `tutti-types`' [`SeqWriter`] / [`SeqReader`], with its loom model there;
//! this file only mirrors `TimeInfo` as a plain-bytes [`Snapshot`], and holds
//! no `unsafe`.
//!
//! Re-entrancy is the governing constraint: the plugin calls `audioMasterGetTime`
//! from inside `processReplacing`, on the same thread that just wrote, and a
//! seqlock reader cannot make progress against a write in flight on its own
//! thread. [`Vst2Instance::process_f32`] runs `update_transport` to completion
//! before entering the plugin, so the sequence is already even by then. Writes
//! never overlap by construction: the one [`TransportWriter`] lives in the
//! instance and writes through `&mut self`.
//!
//! A reader on another thread is the genuinely concurrent case, which is why
//! the read gives up after [`READ_RETRIES`] rather than spinning. What the
//! plugin sees is `None` ("host has no transport", a null `VstTimeInfo`) both
//! before the first write and when the read gives up: a plugin must already
//! handle the null, and the alternatives are worse. Spinning until a copy
//! lands is unbounded on the plugin's thread; handing back an older snapshot
//! would need a copy per reader, kept where `&self` cannot write it. A read
//! gives up only when a write overlapped every copy, which the audio thread's
//! own reads (the usual caller) never see.
//!
//! [`Vst2Instance::process_f32`]: crate::Vst2Instance::process_f32

use bytemuck::{Pod, Zeroable};
use tutti_types::{SeqReader, SeqWriter};
use vst::api::{SmpteFrameRate, TimeInfo};

/// Copies a read tries after the first before it gives up: eight in all. A
/// write is eleven relaxed stores, so a reader that loses this many races
/// was descheduled, and more copies would not help.
const READ_RETRIES: usize = 7;

/// `TimeInfo`'s fields as plain bytes, so the cell can hold it: `TimeInfo`
/// itself is not [`Pod`] (its frame rate is an enum, which not every bit
/// pattern is), so the frame rate is kept as its discriminant. The same
/// fields in the same order, so it is 88 bytes with no padding, eleven words.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct Snapshot {
    sample_pos: f64,
    sample_rate: f64,
    nanoseconds: f64,
    ppq_pos: f64,
    tempo: f64,
    bar_start_pos: f64,
    cycle_start_pos: f64,
    cycle_end_pos: f64,
    time_sig_numerator: i32,
    time_sig_denominator: i32,
    smpte_offset: i32,
    smpte_frame_rate: i32,
    samples_to_next_clock: i32,
    flags: i32,
}

impl From<TimeInfo> for Snapshot {
    fn from(t: TimeInfo) -> Self {
        Self {
            sample_pos: t.sample_pos,
            sample_rate: t.sample_rate,
            nanoseconds: t.nanoseconds,
            ppq_pos: t.ppq_pos,
            tempo: t.tempo,
            bar_start_pos: t.bar_start_pos,
            cycle_start_pos: t.cycle_start_pos,
            cycle_end_pos: t.cycle_end_pos,
            time_sig_numerator: t.time_sig_numerator,
            time_sig_denominator: t.time_sig_denominator,
            smpte_offset: t.smpte_offset,
            smpte_frame_rate: t.smpte_frame_rate as i32,
            samples_to_next_clock: t.samples_to_next_clock,
            flags: t.flags,
        }
    }
}

impl From<Snapshot> for TimeInfo {
    fn from(s: Snapshot) -> Self {
        Self {
            sample_pos: s.sample_pos,
            sample_rate: s.sample_rate,
            nanoseconds: s.nanoseconds,
            ppq_pos: s.ppq_pos,
            tempo: s.tempo,
            bar_start_pos: s.bar_start_pos,
            cycle_start_pos: s.cycle_start_pos,
            cycle_end_pos: s.cycle_end_pos,
            time_sig_numerator: s.time_sig_numerator,
            time_sig_denominator: s.time_sig_denominator,
            smpte_offset: s.smpte_offset,
            smpte_frame_rate: smpte_from_i32(s.smpte_frame_rate),
            samples_to_next_clock: s.samples_to_next_clock,
            flags: s.flags,
        }
    }
}

/// The one writer of the transport snapshot: the instance's
/// `update_transport`, on the audio thread, once per block. Not `Clone`, and
/// [`write`](Self::write) takes `&mut self`, so `process_f32`'s own `&mut`
/// is what keeps writes from overlapping.
pub(crate) struct TransportWriter(SeqWriter<Snapshot>);

impl TransportWriter {
    /// A cell holding no transport information yet.
    pub(crate) fn new() -> Self {
        Self(SeqWriter::new())
    }

    /// A read side, for the plugin's `audioMasterGetTime`.
    pub(crate) fn reader(&self) -> TransportReader {
        TransportReader(self.0.reader())
    }

    /// Publishes a new snapshot, overwriting in place: nothing is allocated or
    /// freed.
    pub(crate) fn write(&mut self, next: TimeInfo) {
        self.0.write(next.into());
    }

    /// The snapshot the last [`write`](Self::write) published, or `None`
    /// before the first: the writer's own copy, so it never retries and never
    /// gives up, which a [`TransportReader::read`] may.
    pub(crate) fn last(&self) -> Option<TimeInfo> {
        self.0.last().map(TimeInfo::from)
    }
}

/// A read side of the transport snapshot: the plugin's `audioMasterGetTime`,
/// from any thread.
#[derive(Clone)]
pub(crate) struct TransportReader(SeqReader<Snapshot>);

impl TransportReader {
    /// The current snapshot, or `None` if none has been published yet or
    /// every one of the read's copies overlapped a write (see the module
    /// docs for why both are `None` to the plugin).
    ///
    /// Wait-free: bounded retries, never blocks. Safe to call re-entrantly from
    /// the audio thread inside the plugin's `process`.
    pub(crate) fn read(&self) -> Option<TimeInfo> {
        self.0.read(READ_RETRIES).ok().flatten().map(TimeInfo::from)
    }
}

/// The frame rate a [`Snapshot`] stored as its discriminant. Every value read
/// back was written from a `SmpteFrameRate`, so the fallback is unreachable;
/// it is the type's default rather than a panic on a callback with no unwind
/// guard.
fn smpte_from_i32(v: i32) -> SmpteFrameRate {
    use SmpteFrameRate as R;
    [
        R::Smpte24fps,
        R::Smpte25fps,
        R::Smpte2997fps,
        R::Smpte30fps,
        R::Smpte2997dfps,
        R::Smpte30dfps,
        R::SmpteFilm16mm,
        R::SmpteFilm35mm,
        R::Smpte239fps,
        R::Smpte249fps,
        R::Smpte599fps,
        R::Smpte60fps,
    ]
    .into_iter()
    .find(|r| *r as i32 == v)
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_with(sample_pos: f64) -> vst::api::TimeInfo {
        vst::api::TimeInfo {
            sample_pos,
            ..Default::default()
        }
    }

    /// Every field survives the cell, bit for bit: each holds a value no
    /// other field does, negatives and the last frame rate included.
    ///
    /// Mutations (run): `From<Snapshot>` reading `smpte_offset` from
    /// `samples_to_next_clock` → fails; `From<TimeInfo>` storing `ppq_pos`
    /// in `tempo` → fails; `smpte_from_i32` missing `Smpte60fps` → the frame
    /// rate reads back as the default → fails.
    #[test]
    fn every_field_round_trips() {
        let info = vst::api::TimeInfo {
            sample_pos: 1.5,
            sample_rate: 2.5,
            nanoseconds: 3.5,
            ppq_pos: 4.5,
            tempo: 5.5,
            bar_start_pos: -6.5,
            cycle_start_pos: 7.5,
            cycle_end_pos: 8.5,
            time_sig_numerator: 7,
            time_sig_denominator: -8,
            smpte_offset: -9,
            smpte_frame_rate: vst::api::SmpteFrameRate::Smpte60fps,
            samples_to_next_clock: 11,
            flags: -12,
        };
        let mut writer = TransportWriter::new();
        writer.write(info);
        let got = writer.reader().read().expect("written");
        assert_eq!(
            writer.last().map(|l| l.flags),
            Some(-12),
            "the writer's own copy is the same snapshot"
        );
        assert_eq!(
            [
                got.sample_pos,
                got.sample_rate,
                got.nanoseconds,
                got.ppq_pos,
                got.tempo,
                got.bar_start_pos,
                got.cycle_start_pos,
                got.cycle_end_pos,
            ],
            [1.5, 2.5, 3.5, 4.5, 5.5, -6.5, 7.5, 8.5]
        );
        assert_eq!(
            [
                got.time_sig_numerator,
                got.time_sig_denominator,
                got.smpte_offset,
                got.smpte_frame_rate as i32,
                got.samples_to_next_clock,
                got.flags,
            ],
            [7, -8, -9, 13, 11, -12]
        );
    }

    #[test]
    fn reads_none_before_any_write() {
        let writer = TransportWriter::new();
        assert!(writer.reader().read().is_none());
        assert!(writer.last().is_none());
    }

    #[test]
    fn read_returns_last_written_snapshot() {
        let mut writer = TransportWriter::new();
        let cell = writer.reader();
        writer.write(info_with(128.0));
        assert_eq!(cell.read().unwrap().sample_pos, 128.0);
        writer.write(info_with(256.0));
        assert_eq!(cell.read().unwrap().sample_pos, 256.0);
        assert_eq!(writer.last().unwrap().sample_pos, 256.0);
    }

    /// The re-entrant shape: a read from inside `process`, after the write
    /// completed on the same thread. Must return the fresh value, not spin.
    #[test]
    fn reentrant_same_thread_read_sees_completed_write() {
        let mut writer = TransportWriter::new();
        let cell = writer.reader();
        writer.write(info_with(64.0));
        // Nested reads, mimicking a plugin querying more than once per block.
        let outer = cell.read().unwrap();
        let inner = cell.read().unwrap();
        assert_eq!(outer.sample_pos, 64.0);
        assert_eq!(inner.sample_pos, 64.0);
    }

    /// A concurrent reader must never observe a half-written snapshot. The
    /// writer keeps every field it sets in lockstep, so a torn read shows up as
    /// a disagreeing pair.
    #[test]
    #[cfg_attr(miri, ignore = "a threaded stress run; the loom model covers it")]
    fn concurrent_reader_never_observes_a_torn_snapshot() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mut writer = TransportWriter::new();
        let stop = Arc::new(AtomicBool::new(false));

        let reader = {
            let cell = writer.reader();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut torn = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(info) = cell.read() {
                        // The writer keeps these three in lockstep, so any
                        // disagreement is a torn read.
                        if info.ppq_pos != info.sample_pos || info.tempo != info.sample_pos {
                            torn += 1;
                        }
                    }
                }
                torn
            })
        };

        for i in 0..200_000u32 {
            let v = f64::from(i);
            writer.write(vst::api::TimeInfo {
                sample_pos: v,
                ppq_pos: v,
                tempo: v,
                ..Default::default()
            });
        }
        stop.store(true, Ordering::Relaxed);

        assert_eq!(reader.join().unwrap(), 0, "reader observed a torn snapshot");
    }
}
