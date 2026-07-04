//! VST3 `IMidiLearn` support: forwarding live MIDI-CC input to the plugin so it
//! can bind the next incoming controller to the parameter the user is editing.
//!
//! The SDK pins `IMidiLearn::onLiveMIDIControllerInput` as a **UI-thread** call
//! (`[UI-thread & (Initialized | Connected)]` in `ivstmidilearn.h`), and its
//! own example gates the whole thing behind a "doMIDILearn" flag set by a UI
//! button — returning `kResultFalse` when not armed. But the host only *sees*
//! incoming CC on the **realtime audio thread** (in [`super::midi_mapping`]).
//! Those two facts are the entire design here:
//!
//! - **Capture (RT thread):** when armed, each incoming CC `(channel, cc)` is
//!   pushed to a bounded, lock-free channel. Gated by an [`AtomicBool`] so the
//!   normal (un-armed) case does literally nothing on the hot path. Bounded +
//!   `try_send` keeps the push allocation-free; on overflow the CC is dropped
//!   (learning only needs *a* recent CC, not all of them).
//! - **Forward (main thread):** [`Vst3Loaded::poll_plugin_notifications`] drains
//!   the channel and makes the actual `onLiveMIDIControllerInput` COM call,
//!   honouring the spec's threading contract.
//!
//! Arming is the host/UI's job (e.g. right-click a knob → "MIDI learn" →
//! [`MidiLearnConsumer::arm`]). Until armed, the path is inert and RT-free —
//! the same "plumbed but idle until a producer wires it" shape note expression
//! uses.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use vst3::ComPtr;
use vst3::Steinberg::Vst::{IMidiLearn, IMidiLearnTrait};

/// Bound on the in-flight CC capture channel. Learning only needs a recent CC,
/// so a small ring is plenty; overflow drops the oldest-unread silently.
const CC_CHANNEL_CAPACITY: usize = 64;

/// A captured live CC: `(channel, controller_number)`. Value is irrelevant for
/// learning — the plugin only needs to know *which* controller moved.
type CapturedCc = (i16, i16);

/// RT-thread half: captures live CCs into the channel when armed. Lives in the
/// audio scratch ([`super::instance`]'s `AudioIO`) so the process loop can reach
/// it allocation-free.
///
/// Created from the long-lived [`MidiLearnConsumer`] via
/// [`MidiLearnConsumer::producer`] at activation time (the consumer outlives it:
/// it's built at load, the producer only exists while the plugin is active).
pub(super) struct MidiLearnProducer {
    armed: Arc<AtomicBool>,
    tx: Sender<CapturedCc>,
}

impl MidiLearnProducer {
    /// True when MIDI learn is armed. The hot path checks this first and skips
    /// all capture work (including MIDI decode) otherwise.
    #[inline]
    pub(super) fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Capture a single live CC `(channel, controller)` if armed. Allocation-free
    /// (`try_send` into a pre-sized bounded channel); drops the CC on a full or
    /// disconnected channel rather than blocking the audio thread.
    ///
    /// `controller` is a VST3 `ControllerNumbers` index (0-127 plus the
    /// synthetic aftertouch/pitch-bend slots) — passed through verbatim; the
    /// plugin decides what it accepts.
    #[inline]
    pub(super) fn capture(&self, channel: u8, controller: usize) {
        if !self.is_armed() {
            return;
        }
        match self.tx.try_send((channel as i16, controller as i16)) {
            Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Main-thread half: arms/disarms learning and forwards captured CCs to the
/// plugin's `IMidiLearn`. Lives in [`super::loaded::Vst3Loaded`] — built once at
/// load and surviving activate/deactivate cycles, each of which spins up a fresh
/// [`producer`](Self::producer) for the audio scratch.
pub(super) struct MidiLearnConsumer {
    controller: Option<ComPtr<IMidiLearn>>,
    armed: Arc<AtomicBool>,
    tx: Sender<CapturedCc>,
    rx: Receiver<CapturedCc>,
}

impl MidiLearnConsumer {
    /// Build the consumer from the plugin's `IMidiLearn` (queried off
    /// `IEditController`), or `None` if the plugin doesn't implement it — in
    /// which case [`forward_pending`](Self::forward_pending) is a no-op and
    /// arming has no observable effect.
    pub(super) fn new(controller: Option<ComPtr<IMidiLearn>>) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(CC_CHANNEL_CAPACITY);
        Self {
            controller,
            armed: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
        }
    }

    /// Spawn an RT-side [`MidiLearnProducer`] sharing this consumer's armed flag
    /// and CC channel. Called at activation; the producer lives in the audio
    /// scratch and is dropped at deactivation, leaving the consumer intact.
    pub(super) fn producer(&self) -> MidiLearnProducer {
        MidiLearnProducer {
            armed: self.armed.clone(),
            tx: self.tx.clone(),
        }
    }
    /// Arm or disarm MIDI learn. While armed, the RT path captures incoming
    /// CCs; [`forward_pending`](Self::forward_pending) then relays them to the
    /// plugin on the next poll. No-op effect when the plugin has no
    /// `IMidiLearn` (capture still records, but forwarding does nothing).
    pub(super) fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::Relaxed);
    }

    /// Whether MIDI learn is currently armed.
    pub(super) fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Drain every captured CC and forward it to the plugin's `IMidiLearn` on
    /// this (main/UI) thread, honouring the SDK's threading contract. No-op if
    /// the plugin doesn't implement the interface. Returns the number of CCs
    /// forwarded.
    pub(super) fn forward_pending(&self) -> usize {
        let Some(controller) = self.controller.as_ref() else {
            // No IMidiLearn — still drain so a stale armed flag can't back the
            // channel up unboundedly.
            return self.rx.try_iter().count();
        };
        let mut count = 0;
        for (channel, cc) in self.rx.try_iter() {
            // busIndex 0: live CC is reported against the first event bus, the
            // same contract IMidiMapping uses (see midi_mapping.rs).
            unsafe {
                controller.onLiveMIDIControllerInput(0, channel, cc);
            }
            count += 1;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un-armed capture is a no-op: nothing reaches the consumer.
    #[test]
    fn unarmed_capture_is_dropped() {
        let consumer = MidiLearnConsumer::new(None);
        let producer = consumer.producer();
        producer.capture(3, 7);
        producer.capture(0, 1);
        // Nothing was armed → channel is empty → no forwards.
        assert_eq!(consumer.forward_pending(), 0);
    }

    /// Armed capture reaches the consumer; without an IMidiLearn the COM call is
    /// skipped but the channel is still drained.
    #[test]
    fn armed_capture_is_drained() {
        let consumer = MidiLearnConsumer::new(None);
        let producer = consumer.producer();
        consumer.arm(true);
        assert!(consumer.is_armed());
        producer.capture(2, 74);
        producer.capture(5, 11);
        // Drained (2 CCs) even though there's no plugin to forward to.
        assert_eq!(consumer.forward_pending(), 2);
        // Second poll has nothing left.
        assert_eq!(consumer.forward_pending(), 0);
    }

    /// Disarming stops capture mid-stream.
    #[test]
    fn disarm_stops_capture() {
        let consumer = MidiLearnConsumer::new(None);
        let producer = consumer.producer();
        consumer.arm(true);
        producer.capture(0, 1);
        consumer.arm(false);
        producer.capture(0, 2); // dropped — disarmed
        assert_eq!(consumer.forward_pending(), 1);
    }

    /// Overflow past the bounded capacity drops extra CCs without panicking or
    /// allocating — the RT-safety contract.
    #[test]
    fn overflow_drops_without_panic() {
        let consumer = MidiLearnConsumer::new(None);
        let producer = consumer.producer();
        consumer.arm(true);
        for i in 0..(CC_CHANNEL_CAPACITY * 4) {
            producer.capture(0, i % 128);
        }
        // At most the channel capacity survived; the rest were dropped.
        let forwarded = consumer.forward_pending();
        assert!(forwarded <= CC_CHANNEL_CAPACITY, "forwarded {forwarded} > cap");
        assert!(forwarded > 0);
    }

    /// RT regression: armed capture into a warmed bounded channel is
    /// allocation-free, including the overflow path (`try_send` returning `Full`
    /// without allocating). The bounded channel allocates its slab once at
    /// construction; `capture` only ever writes into it.
    #[test]
    fn armed_capture_is_allocation_free_after_warmup() {
        let consumer = MidiLearnConsumer::new(None);
        let producer = consumer.producer();
        consumer.arm(true);
        producer.capture(0, 1); // warm

        assert_no_alloc::assert_no_alloc(|| {
            // Far more pushes than capacity — exercises both the accepted and
            // the dropped-on-full branches with zero allocation.
            for i in 0..10_000 {
                producer.capture((i % 16) as u8, i % 128);
            }
        });
        // Drain whatever's left (off-RT, allocation allowed).
        let _ = consumer.forward_pending();
    }
}
