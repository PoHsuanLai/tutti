//! An open winmm output.

use std::time::{Duration, Instant};

use windows_sys::Win32::Media::Audio::{
    CALLBACK_NULL, HMIDIOUT, MHDR_DONE, MIDIHDR, midiOutClose, midiOutLongMsg, midiOutOpen,
    midiOutPrepareHeader, midiOutReset, midiOutShortMsg, midiOutUnprepareHeader,
};

use crate::error::{Error, check};

/// How long a SysEx send may take beyond what its bytes take on a MIDI 1.0
/// wire (320 microseconds each, rounded up to a millisecond per three).
const SLACK: Duration = Duration::from_secs(1);

/// An open output. Dropping it silences the port and closes it.
pub struct MidiOut {
    handle: HMIDIOUT,
}

impl std::fmt::Debug for MidiOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiOut").finish_non_exhaustive()
    }
}

// SAFETY: a winmm handle names a driver object and is not tied to the thread
// that opened it; `MidiOut` is used from one thread at a time (it is not
// `Sync`).
unsafe impl Send for MidiOut {}

impl MidiOut {
    /// Opens output port `index`.
    ///
    /// # Errors
    ///
    /// `midiOutOpen`'s: `MMSYSERR_ALLOCATED` when another application holds the
    /// port, `MMSYSERR_BADDEVICEID` when it is gone.
    pub fn open(index: u32) -> Result<Self, Error> {
        let mut handle: HMIDIOUT = std::ptr::null_mut();
        // SAFETY: `handle` is a live out-pointer; no callback is registered.
        let code = unsafe { midiOutOpen(&raw mut handle, index, 0, 0, CALLBACK_NULL) };
        check("midiOutOpen", code)?;
        Ok(Self { handle })
    }

    /// Sends one short message: the status byte, then its data bytes, low
    /// byte first.
    ///
    /// # Errors
    ///
    /// `midiOutShortMsg`'s.
    pub fn short(&mut self, message: u32) -> Result<(), Error> {
        // SAFETY: `handle` is the open output.
        let code = unsafe { midiOutShortMsg(self.handle, message) };
        check("midiOutShortMsg", code)
    }

    /// Sends `message`, a whole System Exclusive message from `0xF0` to
    /// `0xF7`, and returns when the driver has taken all of it.
    ///
    /// # Errors
    ///
    /// The failing call; and `midiOutLongMsg` after its timeout, in which case
    /// the buffer is kept for good (the driver may still read it).
    pub fn long(&mut self, message: &[u8]) -> Result<(), Error> {
        let mut data = message.to_vec().into_boxed_slice();
        let mut header = Box::new(MIDIHDR::default());
        header.lpData = data.as_mut_ptr();
        header.dwBufferLength = u32::try_from(data.len()).map_err(|_| Error {
            operation: "midiOutLongMsg",
            code: windows_sys::Win32::Media::MMSYSERR_INVALPARAM,
        })?;
        let size = size_of::<MIDIHDR>() as u32;
        let ptr = &raw mut *header;
        // SAFETY: `ptr` is a live header whose `lpData` is a live buffer of
        // `dwBufferLength` bytes, both owned here until unprepared.
        let code = unsafe { midiOutPrepareHeader(self.handle, ptr, size) };
        check("midiOutPrepareHeader", code)?;
        // SAFETY: the header is prepared and stays put until it is done.
        let sent = check("midiOutLongMsg", unsafe {
            midiOutLongMsg(self.handle, ptr, size)
        });
        let mut done = sent.is_err();
        if sent.is_ok() {
            let bound = Duration::from_millis(message.len() as u64 / 3 + 1) + SLACK;
            let started = Instant::now();
            while started.elapsed() < bound {
                // SAFETY: the header is live; the driver sets `MHDR_DONE` from
                // its own thread, so the field is read volatile (and
                // unaligned, the struct being packed).
                let flags = unsafe { (&raw const (*ptr).dwFlags).read_volatile() };
                if flags & MHDR_DONE != 0 {
                    done = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if !done {
            // The driver may still read the header and the bytes: keep both.
            std::mem::forget(header);
            std::mem::forget(data);
            return Err(Error {
                operation: "midiOutLongMsg",
                code: Error::NOT_RETURNED,
            });
        }
        // SAFETY: the header is prepared and the driver has finished with it
        // (or never took it).
        let code = unsafe { midiOutUnprepareHeader(self.handle, ptr, size) };
        sent?;
        check("midiOutUnprepareHeader", code)
    }
}

impl Drop for MidiOut {
    fn drop(&mut self) {
        // SAFETY: `handle` is the open output, reset (all notes off) and
        // closed once.
        unsafe {
            midiOutReset(self.handle);
            midiOutClose(self.handle);
        }
    }
}
