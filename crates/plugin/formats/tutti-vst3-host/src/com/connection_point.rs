//! IConnectionPoint COM implementation. Used by the host to bridge plugin
//! component/controller halves when they are separate objects.

use parking_lot::Mutex;
use vst3::Steinberg::{
    kNotImplemented, kResultOk, tresult,
    Vst::{
        IAttributeList, IAttributeListTrait, IConnectionPoint, IConnectionPointTrait, IMessage,
        IMessageTrait,
    },
};
use vst3::{Class, ComPtr, ComWrapper};

type MessageCallback = Mutex<Option<Box<dyn Fn(&[u8]) + Send + Sync>>>;

pub struct ConnectionPoint {
    connected: Mutex<Option<ComPtr<IConnectionPoint>>>,
    message_callback: MessageCallback,
}

impl Class for ConnectionPoint {
    type Interfaces = (IConnectionPoint,);
}

impl ConnectionPoint {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            connected: Mutex::new(None),
            message_callback: Mutex::new(None),
        })
    }

    pub fn set_message_callback<F>(&self, callback: F)
    where
        F: Fn(&[u8]) + Send + Sync + 'static,
    {
        *self.message_callback.lock() = Some(Box::new(callback));
    }

    pub fn is_connected(&self) -> bool {
        self.connected.lock().is_some()
    }
}

impl IConnectionPointTrait for ConnectionPoint {
    unsafe fn connect(&self, other: *mut IConnectionPoint) -> tresult {
        // Borrowed pointer in; addref via `to_com_ptr` so we own a copy for the
        // lifetime of the connection.
        let stored = vst3::ComRef::<IConnectionPoint>::from_raw(other).map(|r| r.to_com_ptr());
        *self.connected.lock() = stored;
        kResultOk
    }

    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        *self.connected.lock() = None;
        kResultOk
    }

    unsafe fn notify(&self, message: *mut IMessage) -> tresult {
        if message.is_null() {
            return kNotImplemented;
        }
        let msg_ref = match vst3::ComRef::<IMessage>::from_raw(message) {
            Some(m) => m,
            None => return kNotImplemented,
        };
        let _message_id = msg_ref.getMessageID();

        if let Some(ref cb) = *self.message_callback.lock() {
            let attrs_ptr = msg_ref.getAttributes();
            let attrs = match vst3::ComRef::<IAttributeList>::from_raw(attrs_ptr) {
                Some(a) => a,
                None => return kResultOk,
            };
            let data_key = c"data".as_ptr();
            let mut data_ptr: *const std::ffi::c_void = std::ptr::null();
            let mut data_size: u32 = 0;
            let result = attrs.getBinary(data_key, &mut data_ptr, &mut data_size);
            if result == kResultOk && !data_ptr.is_null() && data_size > 0 {
                let slice = std::slice::from_raw_parts(data_ptr as *const u8, data_size as usize);
                cb(slice);
            }
        }
        kResultOk
    }
}
