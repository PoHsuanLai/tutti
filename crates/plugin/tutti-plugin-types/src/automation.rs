//! Parameter automation primitives passed into a plugin's process call.

use smallvec::SmallVec;

/// One automation sample: the value at a specific sample offset within
/// the current process block.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParameterPoint {
    pub sample_offset: i32,
    pub value: f64,
}

/// Ordered list of [`ParameterPoint`]s for a single parameter id within
/// one block. Inline storage keeps a full block's automation run
/// allocation-free on the RT path.
///
/// Sized for the densest producer: `ParamAutomationSource` samples one point
/// per `SAMPLE_STRIDE` (8) samples plus the final sample, so a full
/// `MAX_BUFFER_SIZE` (64) block yields `64/8 + 1 = 9` points — 10 inline
/// leaves headroom and never spills mid-block.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParameterQueue {
    pub param_id: u32,
    /// Points in ascending `sample_offset` order (caller maintains order).
    pub points: SmallVec<[ParameterPoint; 10]>,
}

impl ParameterQueue {
    pub fn new(param_id: u32) -> Self {
        Self {
            param_id,
            points: SmallVec::new(),
        }
    }

    pub fn add_point(&mut self, sample_offset: i32, value: f64) {
        self.points.push(ParameterPoint {
            sample_offset,
            value,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn clear(&mut self) {
        self.points.clear();
    }
}

/// Collection of [`ParameterQueue`]s, one per parameter id, passed into
/// or returned from a plugin's process call.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParameterChanges {
    /// One queue per parameter; the first 16 live inline.
    pub queues: SmallVec<[ParameterQueue; 16]>,
}

impl ParameterChanges {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a point to the queue for `param_id`, creating the queue if
    /// it doesn't exist yet.
    pub fn add_change(&mut self, param_id: u32, sample_offset: i32, value: f64) {
        if let Some(queue) = self.queues.iter_mut().find(|q| q.param_id == param_id) {
            queue.add_point(sample_offset, value);
        } else {
            let mut queue = ParameterQueue::new(param_id);
            queue.add_point(sample_offset, value);
            self.queues.push(queue);
        }
    }

    pub fn add_queue(&mut self, queue: ParameterQueue) -> &mut Self {
        self.queues.push(queue);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.queues.is_empty() || self.queues.iter().all(|q| q.is_empty())
    }

    pub fn len(&self) -> usize {
        self.queues.len()
    }

    pub fn clear(&mut self) {
        self.queues.clear();
    }

    pub fn get_queue(&self, param_id: u32) -> Option<&ParameterQueue> {
        self.queues.iter().find(|q| q.param_id == param_id)
    }

    pub fn get_queue_mut(&mut self, param_id: u32) -> Option<&mut ParameterQueue> {
        self.queues.iter_mut().find(|q| q.param_id == param_id)
    }
}
