//! IDataExchangeHandler COM implementation — VST 3.7+ audio→edit controller
//! block transport.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use vst3::Steinberg::{
    kInvalidArgument, kResultOk, tresult, TBool,
    Vst::{
        DataExchangeBlock, DataExchangeBlockID, DataExchangeQueueID, DataExchangeUserContextID,
        IAudioProcessor, IDataExchangeHandler, IDataExchangeHandlerTrait,
    },
};
use vst3::{Class, ComWrapper};

#[derive(Debug, Clone)]
pub struct DataBlock {
    pub user_context_id: u32,
    pub data: Vec<u8>,
}

struct Queue {
    block_size: u32,
    blocks: Vec<Vec<u8>>,
    next_block_id: u32,
    locked_blocks: HashMap<u32, Vec<u8>>,
}

impl Queue {
    fn new(block_size: u32, num_blocks: u32) -> Self {
        let blocks = (0..num_blocks)
            .map(|_| vec![0u8; block_size as usize])
            .collect();
        Self {
            block_size,
            blocks,
            next_block_id: 0,
            locked_blocks: HashMap::new(),
        }
    }
}

pub struct DataExchangeHandler {
    next_queue_id: AtomicU32,
    queues: Mutex<HashMap<u32, Queue>>,
    data_sender: Sender<DataBlock>,
}

impl Class for DataExchangeHandler {
    type Interfaces = (IDataExchangeHandler,);
}

impl DataExchangeHandler {
    pub fn new() -> (ComWrapper<Self>, Receiver<DataBlock>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let wrapper = ComWrapper::new(Self {
            next_queue_id: AtomicU32::new(1),
            queues: Mutex::new(HashMap::new()),
            data_sender: tx,
        });
        (wrapper, rx)
    }
}

impl IDataExchangeHandlerTrait for DataExchangeHandler {
    unsafe fn openQueue(
        &self,
        _processor: *mut IAudioProcessor,
        block_size: u32,
        num_blocks: u32,
        _alignment: u32,
        user_context_id: DataExchangeUserContextID,
        out_id: *mut DataExchangeQueueID,
    ) -> tresult {
        let queue_id = self.next_queue_id.fetch_add(1, Ordering::SeqCst);
        let queue = Queue::new(block_size, num_blocks);
        self.queues.lock().insert(user_context_id, queue);
        if !out_id.is_null() {
            *out_id = queue_id;
        }
        kResultOk
    }

    unsafe fn closeQueue(&self, queue_id: DataExchangeQueueID) -> tresult {
        self.queues.lock().remove(&queue_id);
        kResultOk
    }

    unsafe fn lockBlock(
        &self,
        queue_id: DataExchangeQueueID,
        block: *mut DataExchangeBlock,
    ) -> tresult {
        if block.is_null() {
            return kInvalidArgument;
        }
        let mut queues = self.queues.lock();
        if let Some(queue) = queues.get_mut(&queue_id) {
            if let Some(mut data) = queue.blocks.pop() {
                let block_id = queue.next_block_id;
                queue.next_block_id += 1;
                (*block).data = data.as_mut_ptr() as *mut c_void;
                (*block).size = queue.block_size;
                (*block).blockID = block_id;
                queue.locked_blocks.insert(block_id, data);
                return kResultOk;
            }
        }
        kInvalidArgument
    }

    unsafe fn freeBlock(
        &self,
        queue_id: DataExchangeQueueID,
        block_id: DataExchangeBlockID,
        send_to_controller: TBool,
    ) -> tresult {
        let mut queues = self.queues.lock();
        if let Some(queue) = queues.get_mut(&queue_id) {
            if let Some(data) = queue.locked_blocks.remove(&block_id) {
                if send_to_controller != 0 {
                    let _ = self.data_sender.send(DataBlock {
                        user_context_id: queue_id,
                        data: data.clone(),
                    });
                }
                queue.blocks.push(data);
                return kResultOk;
            }
        }
        kInvalidArgument
    }
}
