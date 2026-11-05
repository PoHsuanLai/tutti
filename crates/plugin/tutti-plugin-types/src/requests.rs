//! How a plugin's calls into the host, and the host's edits to a running
//! plugin, cross threads.
//!
//! A plugin calls its host from any thread. Each such call is one bit of a
//! [`RequestWord`]: [`Request::bit`] names it, the
//! callback raises it, and the main thread's idle pass turns the taken bits
//! back into [`Request`]s ([`taken`]) and feeds them to the core. Timers and
//! file descriptors carry an id, so they are main-thread registrations and
//! have no bit.
//!
//! An [`Edit`] goes the other way: a parameter write on an active plugin,
//! queued to the Processor and applied at the head of its next block.

use tutti_types::{EditReader, EditWriter, RequestBit, RequestWord, RingFull, edit_ring};

use crate::{Normalized, ParamId, Request, RescanScope};

const RESTART: RequestBit = RequestBit::new(0);
const IO_CHANGED: RequestBit = RequestBit::new(1);
const LATENCY_CHANGED: RequestBit = RequestBit::new(2);
const TAIL_CHANGED: RequestBit = RequestBit::new(3);
const RELOAD: RequestBit = RequestBit::new(4);
const CALLBACK: RequestBit = RequestBit::new(5);
const RESCAN_VALUES: RequestBit = RequestBit::new(6);
const RESCAN_TEXT: RequestBit = RequestBit::new(7);
const RESCAN_INFO: RequestBit = RequestBit::new(8);
const RESCAN_ALL: RequestBit = RequestBit::new(9);

/// Every request that has a bit, in the order [`taken`] yields them.
const WORD_REQUESTS: [(RequestBit, Request); 10] = [
    (RESTART, Request::Restart),
    (IO_CHANGED, Request::IoChanged),
    (LATENCY_CHANGED, Request::LatencyChanged),
    (TAIL_CHANGED, Request::TailChanged),
    (RELOAD, Request::Reload),
    (CALLBACK, Request::Callback),
    (RESCAN_VALUES, Request::RescanParams(RescanScope::Values)),
    (RESCAN_TEXT, Request::RescanParams(RescanScope::Text)),
    (RESCAN_INFO, Request::RescanParams(RescanScope::Info)),
    (RESCAN_ALL, Request::RescanParams(RescanScope::All)),
];

impl Request {
    /// The bit a callback raises for this request, or `None` for a timer or a
    /// file descriptor, which carry an id and are registered on the main
    /// thread.
    #[must_use]
    pub fn bit(self) -> Option<RequestBit> {
        WORD_REQUESTS
            .iter()
            .find_map(|&(bit, request)| (request == self).then_some(bit))
    }

    /// Raises this request on `word`. Wait-free; any thread. `false` when the
    /// request has no bit and nothing was raised.
    pub fn raise(self, word: &RequestWord) -> bool {
        match self.bit() {
            Some(bit) => {
                word.raise(bit);
                true
            }
            None => false,
        }
    }
}

/// The requests raised on `word` since the last call, each once. Main thread
/// only: it takes the word.
pub fn taken(word: &RequestWord) -> impl Iterator<Item = Request> + use<> {
    let bits = word.take();
    WORD_REQUESTS
        .into_iter()
        .filter_map(move |(bit, request)| bits.contains(bit).then_some(request))
}

/// A change the main thread queues to a running plugin.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Edit {
    /// Set a parameter.
    Param {
        /// Which.
        id: ParamId,
        /// To what.
        value: Normalized,
    },
}

const PARAM_TAG: u64 = 1;

impl Edit {
    fn encode(self) -> [u64; 2] {
        match self {
            Self::Param { id, value } => {
                [PARAM_TAG << 32 | u64::from(id.get()), value.get().to_bits()]
            }
        }
    }

    fn decode(raw: [u64; 2]) -> Option<Self> {
        (raw[0] >> 32 == PARAM_TAG).then(|| Self::Param {
            id: ParamId::new(raw[0] as u32),
            value: Normalized::new(f64::from_bits(raw[1])),
        })
    }
}

/// A ring of [`Edit`]s holding up to `capacity`, as its sending and draining
/// ends.
///
/// # Panics
///
/// If `capacity` is zero.
#[must_use]
pub fn edits(capacity: usize) -> (EditSink, EditDrain) {
    let (writer, reader) = edit_ring(capacity);
    (EditSink(writer), EditDrain(reader))
}

/// The main thread's end of an edit ring.
#[derive(Debug)]
pub struct EditSink(EditWriter<[u64; 2]>);

impl EditSink {
    /// Queues `edit`.
    ///
    /// # Errors
    ///
    /// [`RingFull`], counted in [`refused`](Self::refused). The caller keeps
    /// the latest value per parameter and retries on its next tick.
    pub fn send(&mut self, edit: Edit) -> Result<(), RingFull> {
        self.0.push(edit.encode())
    }

    /// Edits refused for want of room.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.0.refused()
    }
}

/// The audio thread's end of an edit ring.
#[derive(Debug)]
pub struct EditDrain(EditReader<[u64; 2]>);

impl EditDrain {
    /// Hands each queued edit to `apply`, oldest first. Wait-free; never
    /// allocates.
    pub fn drain(&mut self, mut apply: impl FnMut(Edit)) {
        while let Some(raw) = self.0.pop() {
            if let Some(edit) = Edit::decode(raw) {
                apply(edit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request with a bit round-trips through the word, once each, and a
    /// timer or descriptor has none.
    ///
    /// Mutation (run): two requests sharing a bit in `WORD_REQUESTS` (the
    /// `Info` rescan on bit 6) → the `Values` and `Info` rescans collapse and
    /// the count fails.
    #[test]
    fn every_request_with_a_bit_is_taken_once() {
        let word = RequestWord::new();
        for &(_, request) in &WORD_REQUESTS {
            assert!(request.raise(&word));
            assert!(request.raise(&word), "raising twice is one request");
        }
        assert!(!Request::Timer(crate::TimerId(1)).raise(&word));
        assert!(!Request::Fd(crate::FdId(3)).raise(&word));
        let got: Vec<_> = taken(&word).collect();
        let want: Vec<_> = WORD_REQUESTS.iter().map(|&(_, r)| r).collect();
        assert_eq!(got, want);
        assert_eq!(taken(&word).count(), 0);
    }

    /// An edit survives the ring's encoding, in order; a full ring refuses and
    /// counts.
    ///
    /// Mutation (run): `decode` reading `raw[1]` as the id's word → the value
    /// check fails.
    #[test]
    fn edits_round_trip_in_order() {
        let (mut sink, mut drain) = edits(2);
        let a = Edit::Param {
            id: ParamId::new(7),
            value: Normalized::new(0.25),
        };
        let b = Edit::Param {
            id: ParamId::new(u32::MAX),
            value: Normalized::new(1.0),
        };
        sink.send(a).unwrap();
        sink.send(b).unwrap();
        assert!(sink.send(a).is_err());
        assert_eq!(sink.refused(), 1);
        let mut got = Vec::new();
        drain.drain(|e| got.push(e));
        assert_eq!(got, [a, b]);
    }
}
