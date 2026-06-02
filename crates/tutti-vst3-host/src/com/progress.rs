//! Standalone `IProgress` COM implementation. [`ComponentHandler`] also
//! exposes `IProgress`; this handler is used only by the unit-test harness.

use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender};
use vst3::Steinberg::{
    kResultOk, tresult,
    Vst::{
        IProgress, IProgressTrait,
        IProgress_::{ProgressType, ID},
        ParamValue,
    },
};
use vst3::{Class, ComWrapper};

use crate::helpers::utf16_to_string;

/// Long-running progress notifications emitted by plugins (sample loading,
/// offline rendering, etc.). Delivered via
/// [`Vst3Loaded::progress_event_receiver`](crate::Vst3Loaded::progress_event_receiver).
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A new progress operation has begun. `id` uniquely identifies this
    /// operation across its lifetime; `progress_type` is the raw VST3
    /// `ProgressType` value; `description` is a human-readable label.
    Started {
        id: u64,
        progress_type: u32,
        description: String,
    },
    /// Progress update, normalized to `0.0..=1.0`.
    Updated { id: u64, progress: f64 },
    /// The operation with this id has finished.
    Finished { id: u64 },
}

pub struct ProgressHandler {
    next_id: AtomicU64,
    event_sender: Sender<ProgressEvent>,
}

impl Class for ProgressHandler {
    type Interfaces = (IProgress,);
}

impl ProgressHandler {
    pub fn new() -> (ComWrapper<Self>, Receiver<ProgressEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let wrapper = ComWrapper::new(Self {
            next_id: AtomicU64::new(1),
            event_sender: tx,
        });
        (wrapper, rx)
    }
}

impl IProgressTrait for ProgressHandler {
    unsafe fn start(
        &self,
        r#type: ProgressType,
        optional_description: *const u16,
        out_id: *mut ID,
    ) -> tresult {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let desc = if optional_description.is_null() {
            String::new()
        } else {
            let mut len = 0;
            let mut ptr = optional_description;
            while *ptr != 0 {
                len += 1;
                ptr = ptr.add(1);
            }
            utf16_to_string(std::slice::from_raw_parts(optional_description, len))
        };
        let _ = self.event_sender.send(ProgressEvent::Started {
            id,
            progress_type: r#type,
            description: desc,
        });
        if !out_id.is_null() {
            *out_id = id;
        }
        kResultOk
    }

    unsafe fn update(&self, id: ID, norm_value: ParamValue) -> tresult {
        let _ = self.event_sender.send(ProgressEvent::Updated {
            id,
            progress: norm_value,
        });
        kResultOk
    }

    unsafe fn finish(&self, id: ID) -> tresult {
        let _ = self.event_sender.send(ProgressEvent::Finished { id });
        kResultOk
    }
}
