//! The `MIDIEventList` an instrument takes through `MusicDeviceMIDIEventList`,
//! built in a buffer reserved once.
//!
//! CoreMIDI's list is `{ protocol: u32, numPackets: u32, packet[] }` and each
//! packet `{ timeStamp: u64, wordCount: u32, words[wordCount] }`, all packed to
//! four bytes (`MIDIServices.h`: `#pragma pack(push, 4)`), so the whole list is
//! a run of `u32`s and a packet takes `3 + wordCount` of them. This module
//! writes that run itself instead of calling `MIDIEventListAdd`: the audio
//! thread builds a list per block, and a plain safe builder over a
//! pre-allocated slice allocates nothing and needs no `unsafe`. A test on the
//! host's own CoreMIDI compares it with `MIDIEventListInit` and
//! `MIDIEventListAdd`, word for word.
//!
//! A list has **one** protocol (`MIDIProtocolID`): MIDI 1.0 channel voice
//! messages (UMP type `0x2`) or MIDI 2.0 ones (type `0x4`), never both, so a
//! message of the other protocol, or one out of time order (a list's
//! timestamps do not decrease), is refused by [`EventList::push`] and the
//! caller sends the list and starts another. Every other message type is
//! neutral and rides in either.

/// The protocol a list's channel voice messages are in (`MIDIProtocolID`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// `kMIDIProtocol_1_0`: MIDI 1.0 in universal packets, type `0x2`.
    Midi1,
    /// `kMIDIProtocol_2_0`: MIDI 2.0 channel voice, type `0x4`.
    Midi2,
}

impl Wire {
    /// The raw `MIDIProtocolID`.
    pub const fn raw(self) -> u32 {
        match self {
            Self::Midi1 => 1,
            Self::Midi2 => 2,
        }
    }

    /// The protocol a UMP message type puts a list in: `Some` for a channel
    /// voice message, `None` for one that rides in either.
    pub const fn of_message_type(message_type: u8) -> Option<Self> {
        match message_type {
            0x2 => Some(Self::Midi1),
            0x4 => Some(Self::Midi2),
            _ => None,
        }
    }
}

/// Words before the first packet: the protocol and the packet count.
const HEADER_WORDS: usize = 2;
/// Words a packet takes besides its UMP words: the 64-bit timestamp and the count.
const PACKET_OVERHEAD: usize = 3;
/// The longest UMP packet, in words.
const MAX_PACKET_WORDS: usize = 4;

/// `MusicDeviceMIDIEventList`'s signature (`MusicDevice.h`).
type SendEventList = unsafe extern "C" fn(
    coreaudio_sys::MusicDeviceComponent,
    u32,
    *const coreaudio_sys::MIDIEventList,
) -> coreaudio_sys::OSStatus;

unsafe extern "C" {
    /// `dlsym` from libSystem, which every process links.
    fn dlsym(
        handle: *mut std::os::raw::c_void,
        symbol: *const std::os::raw::c_char,
    ) -> *mut std::os::raw::c_void;
}

/// `RTLD_DEFAULT` on macOS: search every image the process has loaded.
const RTLD_DEFAULT: *mut std::os::raw::c_void = -2isize as *mut std::os::raw::c_void;

/// `MusicDeviceMIDIEventList`, resolved once at run time, `None` before macOS 12.
///
/// It is not linked directly: tutti declares no deployment target, so Rust's
/// default (macOS 10.12 on x86_64, 11.0 on aarch64) applies, and a direct
/// reference to a macOS 12 symbol would stop the dynamic loader from loading
/// anything that links this crate on an older system, whatever unit it hosts.
pub(crate) fn send_event_list_fn() -> Option<SendEventList> {
    static FUNCTION: std::sync::OnceLock<Option<SendEventList>> = std::sync::OnceLock::new();
    *FUNCTION.get_or_init(|| {
        // SAFETY: `dlsym` takes a handle and a NUL-terminated name and
        // returns the symbol's address or null; both arguments are valid.
        let address = unsafe { dlsym(RTLD_DEFAULT, c"MusicDeviceMIDIEventList".as_ptr()) };
        if address.is_null() {
            None
        } else {
            // SAFETY: the symbol of that name is `MusicDeviceMIDIEventList`,
            // whose C signature `SendEventList` states (`MusicDevice.h`), and a
            // function pointer and a data pointer are the same size on macOS.
            Some(unsafe {
                std::mem::transmute::<*mut std::os::raw::c_void, SendEventList>(address)
            })
        }
    })
}

/// Whether this macOS has `MusicDeviceMIDIEventList` (macOS 12 and later).
pub fn event_list_available() -> bool {
    send_event_list_fn().is_some()
}

/// What [`EventList::push`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Pushed {
    /// The packet is in the list.
    Added,
    /// The list cannot take it as it is (no room, another protocol, an earlier
    /// time): send the list, [`clear`](EventList::clear) it and push again.
    Refused,
}

/// A `MIDIEventList` under construction, in a buffer that never grows.
#[derive(Debug)]
pub struct EventList {
    words: Box<[u32]>,
    used: usize,
    packets: u32,
    wire: Option<Wire>,
    last: u32,
}

impl EventList {
    /// A list with room for `capacity` words (rounded up to hold one packet of
    /// the longest kind, which an empty list therefore always takes).
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(HEADER_WORDS + PACKET_OVERHEAD + MAX_PACKET_WORDS);
        let mut list = Self {
            words: vec![0; capacity].into_boxed_slice(),
            used: HEADER_WORDS,
            packets: 0,
            wire: None,
            last: 0,
        };
        list.clear();
        list
    }

    /// Empties the list. Allocates nothing.
    pub fn clear(&mut self) {
        self.used = HEADER_WORDS;
        self.packets = 0;
        self.wire = None;
        self.last = 0;
        self.words[0] = Wire::Midi2.raw();
        self.words[1] = 0;
    }

    /// Whether no packet is in the list.
    pub fn is_empty(&self) -> bool {
        self.packets == 0
    }

    /// Appends one UMP packet (`1..=4` words) stamped `offset` sample frames
    /// into the render, in `wire` (`None` for a message type that rides in
    /// either protocol). Refused, leaving the list as it was, when the packet
    /// does not fit, is of the other protocol than the list's, or is earlier
    /// than the last one.
    pub fn push(&mut self, wire: Option<Wire>, offset: u32, ump: &[u32]) -> Pushed {
        let n = ump.len();
        debug_assert!((1..=MAX_PACKET_WORDS).contains(&n));
        if n == 0
            || n > MAX_PACKET_WORDS
            || self.used + PACKET_OVERHEAD + n > self.words.len()
            || offset < self.last
            || matches!((self.wire, wire), (Some(a), Some(b)) if a != b)
        {
            return Pushed::Refused;
        }
        let at = self.used;
        // `timeStamp`, a `u64` packed to four bytes: low word first on the
        // little-endian machines AudioToolbox runs on.
        self.words[at] = offset;
        self.words[at + 1] = 0;
        self.words[at + 2] = n as u32;
        self.words[at + PACKET_OVERHEAD..at + PACKET_OVERHEAD + n].copy_from_slice(ump);
        self.used += PACKET_OVERHEAD + n;
        self.packets += 1;
        self.words[1] = self.packets;
        self.last = offset;
        if let Some(wire) = wire {
            self.wire = Some(wire);
            self.words[0] = wire.raw();
        }
        Pushed::Added
    }

    /// The list as CoreMIDI lays it out, for the call and for a test.
    pub fn as_words(&self) -> &[u32] {
        &self.words[..self.used]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The list is CoreMIDI's layout**: protocol, count, then each packet as
    /// a 64-bit timestamp, a word count and its words.
    #[test]
    fn a_list_is_protocol_count_then_packets() {
        let mut list = EventList::with_capacity(64);
        assert_eq!(
            list.push(Some(Wire::Midi2), 5, &[0x4090_3C00, 0xFFFF_0000]),
            Pushed::Added
        );
        assert_eq!(list.push(None, 9, &[0x3016_0102]), Pushed::Added);
        assert_eq!(
            list.as_words(),
            &[
                2,
                2,
                5,
                0,
                2,
                0x4090_3C00,
                0xFFFF_0000,
                9,
                0,
                1,
                0x3016_0102
            ]
        );
    }

    /// **A list holds one protocol**: a MIDI 1.0 message after a MIDI 2.0 one
    /// is refused, and a neutral one is taken by either.
    #[test]
    fn a_list_refuses_the_other_protocol() {
        let mut list = EventList::with_capacity(64);
        assert_eq!(list.push(None, 0, &[0x1000_0000]), Pushed::Added);
        assert_eq!(
            list.push(Some(Wire::Midi1), 0, &[0x2090_3C40]),
            Pushed::Added
        );
        assert_eq!(
            list.push(Some(Wire::Midi2), 0, &[0x4090_3C00, 0]),
            Pushed::Refused
        );
        assert_eq!(list.as_words()[0], 1);
        assert_eq!(list.as_words()[1], 2);
    }

    /// **Time never runs backwards inside a list**, and a full list refuses.
    #[test]
    fn a_list_refuses_an_earlier_time_and_a_full_buffer() {
        let mut list = EventList::with_capacity(0);
        assert_eq!(list.push(None, 8, &[0x1000_0000]), Pushed::Added);
        assert_eq!(list.push(None, 7, &[0x1000_0000]), Pushed::Refused);
        // The minimum capacity (two header words, one 4-word packet) holds one
        // short packet and no second long one.
        assert_eq!(list.push(None, 8, &[1, 2, 3, 4]), Pushed::Refused);
        list.clear();
        assert!(list.is_empty());
        assert_eq!(list.push(None, 1, &[1, 2, 3, 4]), Pushed::Added);
    }

    /// **The words are what CoreMIDI builds**: the same packets through
    /// `MIDIEventListInit` and `MIDIEventListAdd` come out word for word, so
    /// the layout this module writes is the header's. (CoreMIDI folds packets of
    /// one timestamp into one; this builder keeps one packet per message, which
    /// the call takes as well, so the times here differ.)
    #[test]
    fn the_list_is_what_coremidi_builds() {
        use coremidi_sys::{MIDIEventList, MIDIEventListAdd, MIDIEventListInit};

        let packets: [(u32, &[u32]); 3] = [
            (3, &[0x4090_3C00, 0xFFFF_0000]),
            (4, &[0x4090_4000, 0x8000_0000]),
            (130, &[0x4080_3C00, 0x1234_0000]),
        ];
        let mut theirs = vec![0u32; 256];
        let list = theirs.as_mut_ptr().cast::<MIDIEventList>();
        // SAFETY: `theirs` is 1024 bytes, four aligned, as `listSize` says, and
        // each `Add` is given the packet `Init` or the last `Add` returned.
        unsafe {
            let mut at = MIDIEventListInit(list, 2);
            for (time, words) in packets {
                at = MIDIEventListAdd(
                    list,
                    1024,
                    at,
                    u64::from(time),
                    words.len() as _,
                    words.as_ptr(),
                );
                assert!(!at.is_null());
            }
        }
        let mut ours = EventList::with_capacity(64);
        for (time, words) in packets {
            assert_eq!(ours.push(Some(Wire::Midi2), time, words), Pushed::Added);
        }
        let n = ours.as_words().len();
        assert_eq!(ours.as_words(), &theirs[..n]);
    }

    /// **The resolver finds the symbol by its name** on this macOS (26.6, well
    /// past 12): a misspelt name would find nothing and fail here.
    #[test]
    fn the_event_list_function_is_found() {
        assert!(event_list_available());
    }
}
