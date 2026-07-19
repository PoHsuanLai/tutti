//! `IPlugFrame` host-side impl. The plugin calls `resizeView` from its
//! UI thread; we queue the size for the main thread to drain. The
//! main thread is responsible for resizing the parent window and
//! calling `onSize` — we don't call it reentrantly from here, since
//! many plugins re-issue `resizeView` from their own `onSize`
//! handler, which causes a feedback loop.

use crossbeam_channel::{Receiver, Sender};
use vst3::Steinberg::{kResultOk, tresult, IPlugFrame, IPlugView, ViewRect};
use vst3::{Class, ComWrapper};

use crate::types::EditorSize;

pub(crate) struct HostPlugFrame {
    sender: Sender<EditorSize>,
}

impl Class for HostPlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl HostPlugFrame {
    pub(crate) fn new() -> (ComWrapper<Self>, Receiver<EditorSize>) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let wrapper = ComWrapper::new(Self { sender });
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
