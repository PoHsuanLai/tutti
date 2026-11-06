//! The raw C vtables of the host's safe objects, for the suites that drive
//! the callbacks the way a plugin does.
//!
//! The host crate hands none of these out (it forbids `unsafe`, and its
//! lists and streams lend their vtables only for the span of a plugin call
//! inside the FFI crate), so a test builds them here from the FFI crate's
//! test hooks. Each pointer is valid while its object is alive and unmoved,
//! and the few bytes behind it are leaked on purpose.

use clap_sys::events::{clap_input_events, clap_output_events};
use clap_sys::host::clap_host;
use clap_sys::stream::{clap_istream, clap_ostream};
use tutti_clap_ffi::{EventSink, EventSource, input_raw_for_test, output_raw_for_test};
use tutti_clap_host::{ClapHost, InputEventList, InputStream, OutputEventList, OutputStream};

/// `InputEventList::as_raw`.
pub trait InputListRaw {
    /// The `clap_input_events` over this list.
    fn as_raw(&self) -> *const clap_input_events;
}

impl InputListRaw for InputEventList {
    fn as_raw(&self) -> *const clap_input_events {
        let source: *const dyn EventSource = self;
        // SAFETY: the test keeps the list alive and in place while it uses the
        // pointer.
        unsafe { input_raw_for_test(source) }
    }
}

/// `OutputEventList::as_raw_mut`.
pub trait OutputListRaw {
    /// The `clap_output_events` into this list.
    fn as_raw_mut(&mut self) -> *const clap_output_events;
}

impl OutputListRaw for OutputEventList {
    fn as_raw_mut(&mut self) -> *const clap_output_events {
        let sink: *mut dyn EventSink = self;
        // SAFETY: as `InputListRaw::as_raw`, and the test touches the list only
        // between calls through the pointer.
        unsafe { output_raw_for_test(sink) }
    }
}

/// `ClapHost::as_raw`.
pub trait HostRaw {
    /// The `clap_host` a plugin is handed.
    fn as_raw(&self) -> *const clap_host;
}

impl HostRaw for ClapHost {
    fn as_raw(&self) -> *const clap_host {
        self.ffi().raw_for_test()
    }
}

/// `OutputStream::as_raw`.
pub trait OutputStreamRaw {
    /// The `clap_ostream` over this stream.
    fn as_raw(&mut self) -> *const clap_ostream;
}

impl OutputStreamRaw for OutputStream {
    fn as_raw(&mut self) -> *const clap_ostream {
        let slot: &'static mut Option<clap_ostream> = Box::leak(Box::new(None));
        self.raw(slot)
    }
}

/// `InputStream::as_raw`.
pub trait InputStreamRaw {
    /// The `clap_istream` over this stream.
    fn as_raw(&mut self) -> *const clap_istream;
}

impl InputStreamRaw for InputStream<'_> {
    fn as_raw(&mut self) -> *const clap_istream {
        let slot: &'static mut Option<clap_istream> = Box::leak(Box::new(None));
        self.raw(slot)
    }
}
