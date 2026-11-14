//! An open winmm input.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use windows_sys::Win32::Media::Audio::{
    CALLBACK_FUNCTION, HMIDIIN, MIDIHDR, midiInAddBuffer, midiInClose, midiInOpen,
    midiInPrepareHeader, midiInReset, midiInStart, midiInUnprepareHeader,
};
use windows_sys::Win32::Media::{MM_MIM_CLOSE, MM_MIM_DATA, MM_MIM_LONGDATA};

use crate::error::{Error, check};

/// How many SysEx buffers an input keeps with winmm, and how long each is:
/// a message longer than one buffer arrives as consecutive [`Event::Long`]s.
const BUFFERS: usize = 4;
const BUFFER_BYTES: usize = 4096;

/// What winmm delivered, with its stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A short message (`MIM_DATA`): the status byte and its data bytes, low
    /// byte first, as winmm packs them. Running status is already expanded.
    Short {
        /// The packed message.
        message: u32,
        /// Milliseconds since the input started.
        millis: u32,
    },
    /// Bytes of a System Exclusive message (`MIM_LONGDATA`): a message that
    /// fills a buffer continues in the next event, with no `0xF7` between.
    Long {
        /// The bytes winmm recorded, valid for the call only.
        data: &'a [u8],
        /// Milliseconds since the input started.
        millis: u32,
    },
    /// The driver closed the port (`MIM_CLOSE`): the device was removed, or
    /// the driver was stopped. Nothing is delivered after it. Not sent for
    /// the close that dropping the input makes.
    Closed,
}

type Handler = Box<dyn FnMut(Event<'_>) + Send>;

/// One SysEx buffer: winmm holds the header's address while it is prepared, so
/// it lives in a box that never moves.
struct Buffer {
    header: MIDIHDR,
    /// What the header's `lpData` points into; owned here, never resized.
    _data: Box<[u8]>,
}

impl Buffer {
    fn new() -> Self {
        let mut data = vec![0u8; BUFFER_BYTES].into_boxed_slice();
        let header = MIDIHDR {
            lpData: data.as_mut_ptr(),
            dwBufferLength: BUFFER_BYTES as u32,
            ..MIDIHDR::default()
        };
        Self {
            header,
            _data: data,
        }
    }
}

/// What the driver's thread reaches through the instance pointer.
struct Shared {
    handler: Mutex<Handler>,
    /// Set before the input resets: a buffer winmm returns after that is not
    /// handed back to it.
    closing: AtomicBool,
}

/// An open input, started. Dropping it returns winmm's buffers and closes the
/// port; no event is delivered after the drop returns.
pub struct MidiIn {
    handle: HMIDIIN,
    shared: Box<Shared>,
    /// Allocated once and never grown, so each header keeps its address.
    buffers: Box<[Buffer]>,
}

impl std::fmt::Debug for MidiIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiIn").finish_non_exhaustive()
    }
}

// SAFETY: a winmm handle names a driver object and is not tied to the thread
// that opened it; `MidiIn` is used from one thread at a time (it is not
// `Sync`), and everything else it holds is `Send`.
unsafe impl Send for MidiIn {}

impl MidiIn {
    /// Opens input port `index` and starts it: `handler` runs on winmm's
    /// thread for each message from then on.
    ///
    /// The handler is called from a thread winmm owns, one call at a time, and
    /// must return quickly and call no winmm function.
    ///
    /// # Errors
    ///
    /// The failing call: `midiInOpen` (`MMSYSERR_ALLOCATED` when another
    /// application holds the port exclusively, `MMSYSERR_BADDEVICEID` when it
    /// is gone), a buffer's preparation, or `midiInStart`.
    pub fn open(
        index: u32,
        handler: impl FnMut(Event<'_>) + Send + 'static,
    ) -> Result<Self, Error> {
        let shared = Box::new(Shared {
            handler: Mutex::new(Box::new(handler)),
            closing: AtomicBool::new(false),
        });
        let mut handle: HMIDIIN = std::ptr::null_mut();
        // SAFETY: `handle` is a live out-pointer. The callback has the
        // `MIDIINPROC` signature, and the instance value is the address of the
        // boxed `Shared`, which this `MidiIn` keeps alive and unmoved until
        // `midiInClose` has returned (see `Drop`).
        let code = unsafe {
            midiInOpen(
                &raw mut handle,
                index,
                callback as *const () as usize,
                std::ptr::from_ref::<Shared>(&*shared) as usize,
                CALLBACK_FUNCTION,
            )
        };
        check("midiInOpen", code)?;
        let mut input = Self {
            handle,
            shared,
            buffers: (0..BUFFERS).map(|_| Buffer::new()).collect(),
        };
        // From here a failure drops `input`, which closes the port.
        for buffer in &mut input.buffers {
            let header = &raw mut buffer.header;
            // SAFETY: `header` points at a live header whose `lpData` is a live
            // buffer of `dwBufferLength` bytes, both owned by `input`, whose
            // `buffers` slice is never moved, resized or freed while the port
            // is open.
            let code = unsafe { midiInPrepareHeader(input.handle, header, header_size()) };
            check("midiInPrepareHeader", code)?;
            // SAFETY: the header was prepared just above and is owned by `input`.
            let code = unsafe { midiInAddBuffer(input.handle, header, header_size()) };
            check("midiInAddBuffer", code)?;
        }
        // SAFETY: `handle` is the open input.
        let code = unsafe { midiInStart(input.handle) };
        check("midiInStart", code)?;
        Ok(input)
    }
}

impl Drop for MidiIn {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        // SAFETY: `handle` is the open input. A reset stops recording and
        // returns every queued buffer to its owner; the driver's callbacks
        // for them see `closing` and hand nothing back.
        let reset = unsafe { midiInReset(self.handle) };
        let mut returned = reset == 0;
        for buffer in &mut self.buffers {
            let header = &raw mut buffer.header;
            // SAFETY: the header was prepared by `add_buffer` and is owned by
            // `self`; winmm refuses (and leaves it prepared) while it is
            // still queued.
            let code = unsafe { midiInUnprepareHeader(self.handle, header, header_size()) };
            returned &= code == 0;
        }
        // SAFETY: `handle` is the open input, closed once.
        let closed = unsafe { midiInClose(self.handle) } == 0;
        if !(returned && closed) {
            // winmm may still hold the buffers or call back: freeing them, or
            // the closure's state, would be a use after free. Keep both.
            std::mem::forget(std::mem::take(&mut self.buffers));
            let shared = std::mem::replace(
                &mut self.shared,
                Box::new(Shared {
                    handler: Mutex::new(Box::new(|_| {})),
                    closing: AtomicBool::new(true),
                }),
            );
            std::mem::forget(shared);
        }
    }
}

fn header_size() -> u32 {
    size_of::<MIDIHDR>() as u32
}

/// winmm's `MIDIINPROC`: runs on the driver's thread.
///
/// # Safety
///
/// `instance` is the address of the `Shared` that `MidiIn::open` passed, and
/// for a `MIM_LONGDATA` message `param1` is the address of one of that input's
/// prepared headers; winmm guarantees both for the lifetime of the open port.
unsafe extern "system" fn callback(
    handle: HMIDIIN,
    message: u32,
    instance: usize,
    param1: usize,
    param2: usize,
) {
    // SAFETY: see the function's contract: the `Shared` outlives every
    // callback, because `Drop` closes the port before it is freed.
    let shared = unsafe { &*(instance as *const Shared) };
    // A panic must not unwind into winmm; a panicking handler drops the event.
    let _ = catch_unwind(AssertUnwindSafe(|| match message {
        MM_MIM_DATA => {
            let mut handler = shared
                .handler
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            handler(Event::Short {
                message: param1 as u32,
                millis: param2 as u32,
            });
        }
        MM_MIM_LONGDATA => {
            let header = param1 as *mut MIDIHDR;
            // SAFETY: the header is the input's own (see the contract); it is
            // packed, so its fields are read unaligned.
            let (data, recorded) = unsafe {
                (
                    (&raw const (*header).lpData).read_unaligned(),
                    (&raw const (*header).dwBytesRecorded).read_unaligned(),
                )
            };
            if recorded > 0 && !shared.closing.load(Ordering::Acquire) {
                // SAFETY: `lpData` is the buffer `add_buffer` made and winmm
                // wrote `recorded` bytes into; nothing else touches it until
                // it is handed back below.
                let bytes =
                    unsafe { std::slice::from_raw_parts(data.cast_const(), recorded as usize) };
                let mut handler = shared
                    .handler
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                handler(Event::Long {
                    data: bytes,
                    millis: param2 as u32,
                });
            }
            if !shared.closing.load(Ordering::Acquire) {
                // SAFETY: the header is prepared and has been returned to us by
                // this very message; handing it back is how a recording
                // continues.
                unsafe { midiInAddBuffer(handle, header, header_size()) };
            }
        }
        MM_MIM_CLOSE if !shared.closing.load(Ordering::Acquire) => {
            let mut handler = shared
                .handler
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            handler(Event::Closed);
        }
        _ => {}
    }));
}
