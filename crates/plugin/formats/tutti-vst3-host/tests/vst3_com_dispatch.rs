//! Does the `vst3` crate dispatch `queryInterface` correctly for a class that
//! implements **more than one** interface?
//!
//! This isolates a suspicion raised while debugging the Linux editor crash:
//! `HostPlugFrame` gained a second interface (`IRunLoop` alongside
//! `IPlugFrame`), and afterwards `FUnknownPtr<IRunLoop>` came back null even
//! though our `queryInterface` returned `kResultOk`. A gdb trace showed the
//! receiver and IID pointers apparently shifted by one argument, which would
//! be a thunk/ABI bug in the generated dispatch.
//!
//! `com_scrape_types` gives interface `i` in the tuple an `OFFSET` of
//! `i * size_of::<*mut ()>()`, and each generated `queryInterface` recovers the
//! object header with `(this as *mut u8).offset(-OFFSET)`. If that arithmetic
//! is wrong for any index but the first, a second interface is unreachable —
//! and every VST3 host object with two interfaces is silently broken.
//!
//! These tests need no plugin, no display, and no debugger: build a
//! `ComWrapper`, call `queryInterface` through the raw vtable exactly as a
//! plugin would, and check what comes back.

#![cfg(feature = "conformance")]

use std::ffi::c_void;

use vst3::Steinberg::{
    kResultOk, tresult, FUnknown, IPlugFrame, IPlugFrameTrait, IPlugFrame_iid, IPlugView, ViewRect,
    TUID,
};
use vst3::{Class, ComWrapper};

#[cfg(target_os = "linux")]
use vst3::Steinberg::Linux::{
    FileDescriptor, IEventHandler, IRunLoop, IRunLoopTrait, IRunLoop_iid, ITimerHandler,
    TimerInterval,
};

/// A two-interface class shaped exactly like `HostPlugFrame`: `IPlugFrame`
/// first, `IRunLoop` second.
struct TwoInterfaceClass;

#[cfg(target_os = "linux")]
impl Class for TwoInterfaceClass {
    type Interfaces = (IPlugFrame, IRunLoop);
}

#[cfg(not(target_os = "linux"))]
impl Class for TwoInterfaceClass {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for TwoInterfaceClass {
    unsafe fn resizeView(&self, _view: *mut IPlugView, _new_size: *mut ViewRect) -> tresult {
        kResultOk
    }
}

#[cfg(target_os = "linux")]
impl IRunLoopTrait for TwoInterfaceClass {
    unsafe fn registerEventHandler(
        &self,
        _handler: *mut IEventHandler,
        _fd: FileDescriptor,
    ) -> tresult {
        kResultOk
    }
    unsafe fn unregisterEventHandler(&self, _handler: *mut IEventHandler) -> tresult {
        kResultOk
    }
    unsafe fn registerTimer(&self, _handler: *mut ITimerHandler, _ms: TimerInterval) -> tresult {
        kResultOk
    }
    unsafe fn unregisterTimer(&self, _handler: *mut ITimerHandler) -> tresult {
        kResultOk
    }
}

/// Call `queryInterface` through the raw vtable, the way a C++ plugin does —
/// not through the crate's typed helpers, which could paper over an ABI bug.
///
/// # Safety
/// `unknown` must be a live `FUnknown` pointer for this object.
unsafe fn raw_query(unknown: *mut FUnknown, iid: &TUID) -> (tresult, *mut c_void) {
    let mut out: *mut c_void = std::ptr::null_mut();
    let result = ((*(*unknown).vtbl).queryInterface)(unknown, iid, &mut out);
    (result, out)
}

/// The **first** interface in the tuple must be reachable. If this fails the
/// harness itself is wrong, so it anchors the tests below.
#[test]
fn first_interface_is_queryable() {
    let wrapper = ComWrapper::new(TwoInterfaceClass);
    let frame = wrapper
        .as_com_ref::<IPlugFrame>()
        .expect("IPlugFrame must be exposed");
    let unknown = frame.as_ptr().cast::<FUnknown>();

    let (result, obj) = unsafe { raw_query(unknown, &IPlugFrame_iid) };
    assert_eq!(result, kResultOk, "queryInterface(IPlugFrame) must succeed");
    assert!(!obj.is_null(), "queryInterface(IPlugFrame) returned null");
}

/// The **second** interface must be reachable through the same `FUnknown`.
///
/// This is the exact call VSTGUI makes: it holds our `IPlugFrame*`, upcasts to
/// `FUnknown`, and asks for `IRunLoop`. If the offset arithmetic for index 1 is
/// wrong, this either fails or hands back a bogus pointer.
#[cfg(target_os = "linux")]
#[test]
fn second_interface_is_queryable_through_the_first() {
    let wrapper = ComWrapper::new(TwoInterfaceClass);
    let frame = wrapper
        .as_com_ref::<IPlugFrame>()
        .expect("IPlugFrame must be exposed");
    let unknown = frame.as_ptr().cast::<FUnknown>();

    let (result, obj) = unsafe { raw_query(unknown, &IRunLoop_iid) };
    assert_eq!(
        result, kResultOk,
        "queryInterface(IRunLoop) on the IPlugFrame pointer must succeed — \
         this is exactly what VSTGUI's FUnknownPtr<IRunLoop> does"
    );
    assert!(!obj.is_null(), "queryInterface(IRunLoop) returned null");
}

/// The pointer handed back for the second interface must actually be usable:
/// calling through its vtable must reach *our* implementation.
///
/// A returned-but-wrong pointer is the dangerous case — `queryInterface`
/// reports success, the caller stores it, and the crash happens later at the
/// first virtual call. That is the shape of the editor crash.
#[cfg(target_os = "linux")]
#[test]
fn second_interface_pointer_dispatches_to_our_impl() {
    let wrapper = ComWrapper::new(TwoInterfaceClass);
    let frame = wrapper
        .as_com_ref::<IPlugFrame>()
        .expect("IPlugFrame must be exposed");
    let unknown = frame.as_ptr().cast::<FUnknown>();

    let (result, obj) = unsafe { raw_query(unknown, &IRunLoop_iid) };
    assert_eq!(result, kResultOk);
    assert!(!obj.is_null());

    // Call `registerTimer` through the returned pointer. Our impl returns
    // `kResultOk`; a mis-offset pointer lands in another vtable slot and
    // returns something else (or crashes).
    let run_loop = obj.cast::<IRunLoop>();
    let rc = unsafe { ((*(*run_loop).vtbl).registerTimer)(run_loop, std::ptr::null_mut(), 16) };
    assert_eq!(
        rc, kResultOk,
        "calling registerTimer through the queried IRunLoop pointer did not \
         reach our implementation — the returned pointer is mis-offset"
    );
}

/// `FUnknown` itself must be queryable, and an unknown IID must be refused
/// rather than matched by accident.
#[test]
fn unknown_iid_is_refused() {
    let wrapper = ComWrapper::new(TwoInterfaceClass);
    let frame = wrapper
        .as_com_ref::<IPlugFrame>()
        .expect("IPlugFrame must be exposed");
    let unknown = frame.as_ptr().cast::<FUnknown>();

    let bogus: TUID = [0x7f; 16].map(|b| b as _);
    let (result, _) = unsafe { raw_query(unknown, &bogus) };
    assert_ne!(
        result, kResultOk,
        "an unrecognised IID must not be reported as supported"
    );
}
