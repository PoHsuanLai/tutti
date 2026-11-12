//! The edits the plugin made to its own parameters, as the host heard them.
//!
//! A knob turned in the plugin's own editor changes the unit and tells every
//! parameter listener (`AudioUnitUtilities.h`: the listener exists "to provide
//! notifications when an Audio Unit's parameters or other state changes").
//! The host's listener ([`crate::watch`]) lands each change here, on a dispatch
//! queue of AudioToolbox's, and the control side drains them in
//! [`Control::take_reported_edits`](tutti_plugin_types::Control::take_reported_edits).
//! Not a realtime path: neither end is the audio thread, so a mutex is fine.

#![cfg(target_os = "macos")]

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Edits held between two drains. A drain runs every control tick, so this is
/// seconds of a knob under a hand; past it the oldest are dropped and counted.
const ROOM: usize = 4096;

/// A parameter edit as the listener delivered it: the id and the plain value.
type Edit = (u32, f32);

/// The queue between the listener's thread and the control side.
#[derive(Debug, Default)]
pub(crate) struct ReportedEdits {
    queue: Mutex<VecDeque<Edit>>,
    dropped: AtomicU64,
}

impl ReportedEdits {
    /// Holds one edit, dropping the oldest (and counting it) when the queue is
    /// full, so what survives is where the knob ended up.
    pub(crate) fn push(&self, id: u32, plain: f32) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() == ROOM {
            queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back((id, plain));
    }

    /// Moves every held edit, oldest first, to `out`.
    pub(crate) fn drain(&self, out: &mut Vec<Edit>) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        out.extend(queue.drain(..));
    }

    /// Edits dropped for want of room over the plugin's life.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full queue drops its oldest edit, counts it, and keeps the newest, so
    /// the surviving values end where the knob did.
    #[test]
    fn a_full_queue_keeps_the_newest_and_counts_the_rest() {
        let edits = ReportedEdits::default();
        for i in 0..ROOM + 3 {
            edits.push(7, i as f32);
        }
        let mut out = Vec::new();
        edits.drain(&mut out);
        assert_eq!(out.len(), ROOM);
        assert_eq!(out.first(), Some(&(7, 3.0)));
        assert_eq!(out.last(), Some(&(7, (ROOM + 2) as f32)));
        assert_eq!(edits.dropped(), 3);
    }
}
