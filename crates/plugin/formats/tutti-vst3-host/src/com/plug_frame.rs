//! `IPlugFrame` host-side impl. The plugin calls `resizeView` from its
//! UI thread; the size is queued for the main thread to drain. The
//! main thread is responsible for resizing the parent window and
//! calling `onSize` — it is not called reentrantly from here, since
//! many plugins re-issue `resizeView` from their own `onSize`
//! handler, which causes a feedback loop.
//!
//! On Linux this object also carries `IRunLoop`, delegating to the shared
//! [`RunLoop`](super::run_loop::RunLoop). Some plugin toolkits look for the run
//! loop on the frame rather than on the host context, so it is answered here too —
//! but note the frame is *not* the path that prevents the editor-open crash;
//! see `run_loop.rs` for why the host context is the load-bearing one.
//!
//! Registration is all the frame does. It exposes no way to *pump* the loop:
//! the frame is per-editor-session and comes and goes with `open_editor`, while
//! handlers registered through the host context outlive it. Pumping is
//! therefore driven from the library-scoped loop, via
//! [`Vst3Loaded::run_editor_loop_iteration`](crate::Vst3Loaded::run_editor_loop_iteration).

use crossbeam_channel::{Receiver, Sender};
use vst3::Steinberg::{kResultOk, tresult, IPlugFrame, IPlugView, ViewRect};
use vst3::{Class, ComWrapper};

use crate::types::EditorSize;

#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use vst3::Steinberg::Linux::{
    FileDescriptor, IEventHandler, IRunLoop, ITimerHandler, TimerInterval,
};

#[cfg(target_os = "linux")]
use super::run_loop::RunLoop;

pub(crate) struct HostPlugFrame {
    sender: Sender<EditorSize>,
    /// Shared with `HostApplication` — see `run_loop.rs`.
    #[cfg(target_os = "linux")]
    run_loop: Arc<RunLoop>,
}

#[cfg(target_os = "linux")]
impl Class for HostPlugFrame {
    // `IRunLoop` alongside `IPlugFrame`: the plugin's toolkit finds it by
    // querying the frame it was handed via `IPlugView::setFrame`.
    type Interfaces = (IPlugFrame, IRunLoop);
}

#[cfg(not(target_os = "linux"))]
impl Class for HostPlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl HostPlugFrame {
    pub(crate) fn new(
        #[cfg(target_os = "linux")] run_loop: Arc<RunLoop>,
    ) -> (ComWrapper<Self>, Receiver<EditorSize>) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let wrapper = ComWrapper::new(Self {
            sender,
            #[cfg(target_os = "linux")]
            run_loop,
        });
        (wrapper, receiver)
    }
}

impl vst3::Steinberg::IPlugFrameTrait for HostPlugFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        if new_size.is_null() {
            return kResultOk;
        }
        let rect = unsafe { *new_size };
        let _ = self.sender.send(EditorSize {
            width: (rect.right - rect.left) as u32,
            height: (rect.bottom - rect.top) as u32,
        });
        kResultOk
    }
}

#[cfg(target_os = "linux")]
impl vst3::Steinberg::Linux::IRunLoopTrait for HostPlugFrame {
    unsafe fn registerEventHandler(
        &self,
        handler: *mut IEventHandler,
        fd: FileDescriptor,
    ) -> tresult {
        self.run_loop.register_event_handler(handler, fd)
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
        self.run_loop.unregister_event_handler(handler)
    }

    unsafe fn registerTimer(
        &self,
        handler: *mut ITimerHandler,
        milliseconds: TimerInterval,
    ) -> tresult {
        self.run_loop.register_timer(handler, milliseconds)
    }

    unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
        self.run_loop.unregister_timer(handler)
    }
}
