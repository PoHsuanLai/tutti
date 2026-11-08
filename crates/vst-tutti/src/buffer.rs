//! Buffers to safely work with audio samples.

use num_traits::Float;

use std::slice;

/// `AudioBuffer` contains references to the audio buffers for all input and output channels.
///
/// To create an `AudioBuffer` in a host, use a [`HostBuffer`](../host/struct.HostBuffer.html).
#[derive(Debug)]
pub struct AudioBuffer<'a, T: 'a + Float> {
    inputs: &'a [*const T],
    outputs: &'a mut [*mut T],
    samples: usize,
}

impl<'a, T: 'a + Float> AudioBuffer<'a, T> {
    /// Create an `AudioBuffer` from raw pointers.
    /// Only really useful for interacting with the VST API.
    ///
    /// # Safety
    /// `inputs_raw` must point at `input_count` valid channel pointers and
    /// `outputs_raw` at `output_count`, each channel pointer addressing at
    /// least `samples` elements. The output channels must not alias each other
    /// or the inputs, and all of it must outlive `'a`.
    #[inline]
    pub unsafe fn from_raw(
        input_count: usize,
        output_count: usize,
        inputs_raw: *const *const T,
        outputs_raw: *mut *mut T,
        samples: usize,
    ) -> Self {
        Self {
            // SAFETY: the caller upholds this fn's `# Safety` contract: `inputs_raw` points
            // at `input_count` valid channel pointers that outlive `'a`.
            inputs: unsafe { slice::from_raw_parts(inputs_raw, input_count) },
            // SAFETY: as above for `outputs_raw`/`output_count`, which the caller
            // guarantees are not aliased elsewhere.
            outputs: unsafe { slice::from_raw_parts_mut(outputs_raw, output_count) },
            samples,
        }
    }

    /// The number of input channels that this buffer was created for
    #[inline]
    pub fn input_count(&self) -> usize {
        self.inputs.len()
    }

    /// The number of output channels that this buffer was created for
    #[inline]
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// The number of samples in this buffer (same for all channels)
    #[inline]
    pub fn samples(&self) -> usize {
        self.samples
    }

    /// The raw inputs to pass to processReplacing
    #[inline]
    pub(crate) fn raw_inputs(&self) -> &[*const T] {
        self.inputs
    }

    /// The raw outputs to pass to processReplacing
    #[inline]
    pub(crate) fn raw_outputs(&mut self) -> &mut [*mut T] {
        self.outputs
    }

    /// Split this buffer into separate inputs and outputs.
    #[inline]
    pub fn split<'b>(&'b mut self) -> (Inputs<'b, T>, Outputs<'b, T>)
    where
        'a: 'b,
    {
        (
            Inputs {
                bufs: self.inputs,
                samples: self.samples,
            },
            Outputs {
                bufs: self.outputs,
                samples: self.samples,
            },
        )
    }

    /// Create an iterator over pairs of input buffers and output buffers.
    #[inline]
    pub fn zip<'b>(&'b mut self) -> AudioBufferIterator<'a, 'b, T> {
        AudioBufferIterator {
            audio_buffer: self,
            index: 0,
        }
    }
}

/// Iterator over pairs of buffers of input channels and output channels.
#[derive(Debug)]
pub struct AudioBufferIterator<'a, 'b, T>
where
    T: 'a + Float,
    'a: 'b,
{
    audio_buffer: &'b mut AudioBuffer<'a, T>,
    index: usize,
}

impl<'a, 'b, T> Iterator for AudioBufferIterator<'a, 'b, T>
where
    T: 'b + Float,
{
    type Item = (&'b [T], &'b mut [T]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.audio_buffer.inputs.len()
            && self.index < self.audio_buffer.outputs.len()
        {
            // SAFETY: `AudioBuffer`'s constructor contract (`from_raw`/`HostBuffer`) says
            // each channel pointer addresses at least `samples` live elements; the index
            // is bounds-checked by the `if` above.
            let input = unsafe {
                slice::from_raw_parts(
                    self.audio_buffer.inputs[self.index],
                    self.audio_buffer.samples,
                )
            };
            // SAFETY: as for `input`; output channels do not alias each other or the
            // inputs (constructor contract), and the iterator yields each index once.
            let output = unsafe {
                slice::from_raw_parts_mut(
                    self.audio_buffer.outputs[self.index],
                    self.audio_buffer.samples,
                )
            };
            let val = (input, output);
            self.index += 1;
            Some(val)
        } else {
            None
        }
    }
}

use std::ops::{Index, IndexMut};

/// Wrapper type to access the buffers for the input channels of an `AudioBuffer` in a safe way.
/// Behaves like a slice.
#[derive(Copy, Clone, Debug)]
pub struct Inputs<'a, T: 'a> {
    bufs: &'a [*const T],
    samples: usize,
}

impl<'a, T> Inputs<'a, T> {
    /// Number of channels
    pub fn len(&self) -> usize {
        self.bufs.len()
    }

    /// Returns true if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Access channel at the given index
    pub fn get(&self, i: usize) -> &'a [T] {
        // SAFETY: `i` is bounds-checked by the indexing; the `AudioBuffer` constructor
        // contract says each channel pointer addresses `samples` live elements for `'a`.
        unsafe { slice::from_raw_parts(self.bufs[i], self.samples) }
    }

    /// Split borrowing at the given index, like for slices
    pub fn split_at(&self, i: usize) -> (Inputs<'a, T>, Inputs<'a, T>) {
        let (l, r) = self.bufs.split_at(i);
        (
            Inputs {
                bufs: l,
                samples: self.samples,
            },
            Inputs {
                bufs: r,
                samples: self.samples,
            },
        )
    }
}

impl<'a, T> Index<usize> for Inputs<'a, T> {
    type Output = [T];

    fn index(&self, i: usize) -> &Self::Output {
        self.get(i)
    }
}

/// Iterator over buffers for input channels of an `AudioBuffer`.
#[derive(Debug)]
pub struct InputIterator<'a, T: 'a> {
    data: Inputs<'a, T>,
    i: usize,
}

impl<'a, T> Iterator for InputIterator<'a, T> {
    type Item = &'a [T];

    fn next(&mut self) -> Option<Self::Item> {
        if self.i < self.data.len() {
            let val = self.data.get(self.i);
            self.i += 1;
            Some(val)
        } else {
            None
        }
    }
}

impl<'a, T: Sized> IntoIterator for Inputs<'a, T> {
    type Item = &'a [T];
    type IntoIter = InputIterator<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        InputIterator { data: self, i: 0 }
    }
}

/// Wrapper type to access the buffers for the output channels of an `AudioBuffer` in a safe way.
/// Behaves like a slice.
#[derive(Debug)]
pub struct Outputs<'a, T: 'a> {
    bufs: &'a [*mut T],
    samples: usize,
}

impl<'a, T> Outputs<'a, T> {
    /// Number of channels
    pub fn len(&self) -> usize {
        self.bufs.len()
    }

    /// Returns true if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Access channel at the given index
    pub fn get(&self, i: usize) -> &'a [T] {
        // SAFETY: `i` is bounds-checked by the indexing; the `AudioBuffer` constructor
        // contract says each channel pointer addresses `samples` live elements for `'a`.
        unsafe { slice::from_raw_parts(self.bufs[i], self.samples) }
    }

    /// Mutably access channel at the given index.
    ///
    /// The slice borrows `self`, so two live slices of one channel cannot exist:
    ///
    /// ```compile_fail
    /// # use vst_tutti::host::HostBuffer;
    /// // Mutation (run): return `&'a mut [T]` from `get_mut` again and this
    /// // compiles, so the doctest fails.
    /// let mut host_buffer: HostBuffer<f32> = HostBuffer::new(0, 1);
    /// let inputs: Vec<Vec<f32>> = vec![];
    /// let mut outputs = vec![vec![0.0f32; 4]];
    /// let mut buffer = host_buffer.bind(&inputs, &mut outputs);
    /// let (_, mut out) = buffer.split();
    /// let a = out.get_mut(0);
    /// let b = out.get_mut(0);
    /// a[0] = b[0];
    /// ```
    pub fn get_mut(&mut self, i: usize) -> &mut [T] {
        // SAFETY: bounds and length are as in `get`. The output channels do not alias each
        // other (the `AudioBuffer` constructor contract), and the returned slice borrows
        // `&mut self`, so a second call for the same `i` cannot overlap it.
        unsafe { slice::from_raw_parts_mut(self.bufs[i], self.samples) }
    }

    /// Split borrowing at the given index, like for slices
    pub fn split_at_mut(self, i: usize) -> (Outputs<'a, T>, Outputs<'a, T>) {
        let (l, r) = self.bufs.split_at(i);
        (
            Outputs {
                bufs: l,
                samples: self.samples,
            },
            Outputs {
                bufs: r,
                samples: self.samples,
            },
        )
    }
}

impl<'a, T> Index<usize> for Outputs<'a, T> {
    type Output = [T];

    fn index(&self, i: usize) -> &Self::Output {
        self.get(i)
    }
}

impl<'a, T> IndexMut<usize> for Outputs<'a, T> {
    fn index_mut(&mut self, i: usize) -> &mut Self::Output {
        self.get_mut(i)
    }
}

/// Iterator over buffers for output channels of an `AudioBuffer`.
#[derive(Debug)]
pub struct OutputIterator<'a, 'b, T>
where
    T: 'a,
    'a: 'b,
{
    data: &'b mut Outputs<'a, T>,
    i: usize,
}

impl<'a, 'b, T> Iterator for OutputIterator<'a, 'b, T>
where
    T: 'b,
{
    type Item = &'b mut [T];

    fn next(&mut self) -> Option<Self::Item> {
        if self.i < self.data.len() {
            // SAFETY: `i` is in bounds (checked above). `data` is borrowed mutably for `'b`
            // and `i` only grows, so each channel is handed out once per iterator and no
            // two items overlap; the output channels do not alias each other (the
            // `AudioBuffer` constructor contract), each addressing `samples` elements.
            let val =
                unsafe { slice::from_raw_parts_mut(self.data.bufs[self.i], self.data.samples) };
            self.i += 1;
            Some(val)
        } else {
            None
        }
    }
}

impl<'a, 'b, T: Sized> IntoIterator for &'b mut Outputs<'a, T> {
    type Item = &'b mut [T];
    type IntoIter = OutputIterator<'a, 'b, T>;

    fn into_iter(self) -> Self::IntoIter {
        OutputIterator { data: self, i: 0 }
    }
}

use crate::event::{Event, MidiEvent, SysExEvent};

/// This is used as a placeholder to pre-allocate space for a fixed number of
/// midi events in the re-useable `SendEventBuffer`, because `SysExEvent` is
/// larger than `MidiEvent`, so either one can be stored in a `SysExEvent`.
pub type PlaceholderEvent = api::SysExEvent;

/// This trait is used by `SendEventBuffer::send_events` to accept iterators over midi events
pub trait WriteIntoPlaceholder {
    /// writes an event into the given placeholder memory location
    fn write_into(&self, out: &mut PlaceholderEvent);
}

impl<T: WriteIntoPlaceholder> WriteIntoPlaceholder for &T {
    fn write_into(&self, out: &mut PlaceholderEvent) {
        (*self).write_into(out);
    }
}

impl WriteIntoPlaceholder for MidiEvent {
    fn write_into(&self, out: &mut PlaceholderEvent) {
        // SAFETY: `PlaceholderEvent` (`SysExEvent`, 48 bytes, align 8) is larger than
        // and at least as aligned as `api::MidiEvent` (32 bytes, align 4), so `out` can
        // be viewed as one; the target type is inferred from the assignment below.
        let out = unsafe { &mut *(out as *mut _ as *mut _) };
        *out = api::MidiEvent {
            event_type: api::EventType::Midi,
            byte_size: mem::size_of::<api::MidiEvent>() as i32,
            delta_frames: self.delta_frames,
            flags: if self.live {
                api::MidiEventFlags::REALTIME_EVENT.bits()
            } else {
                0
            },
            note_length: self.note_length.unwrap_or(0),
            note_offset: self.note_offset.unwrap_or(0),
            midi_data: self.data,
            _midi_reserved: 0,
            detune: self.detune,
            note_off_velocity: self.note_off_velocity,
            _reserved1: 0,
            _reserved2: 0,
        };
    }
}

impl<'a> WriteIntoPlaceholder for SysExEvent<'a> {
    fn write_into(&self, out: &mut PlaceholderEvent) {
        *out = PlaceholderEvent {
            event_type: api::EventType::SysEx,
            byte_size: mem::size_of::<PlaceholderEvent>() as i32,
            delta_frames: self.delta_frames,
            _flags: 0,
            data_size: self.payload.len() as i32,
            _reserved1: 0,
            // `as_ptr()` is already `*const u8`; only the const→mut step is
            // real. VST 2.4 declares `system_data` mutable although the host
            // must not write it.
            system_data: self.payload.as_ptr() as *mut u8,
            _reserved2: 0,
        };
    }
}

impl<'a> WriteIntoPlaceholder for Event<'a> {
    fn write_into(&self, out: &mut PlaceholderEvent) {
        match *self {
            Event::Midi(ref ev) => {
                ev.write_into(out);
            }
            Event::SysEx(ref ev) => {
                ev.write_into(out);
            }
            Event::Deprecated(e) => {
                // SAFETY: `PlaceholderEvent` is larger than and at least as aligned as
                // `api::Event` (32 bytes), the type inferred from the assignment.
                let out = unsafe { &mut *(out as *mut _ as *mut _) };
                *out = e;
            }
        };
    }
}

use crate::{api, host::Host};
use std::mem;

/// This buffer is used for sending midi events through the VST interface.
/// The purpose of this is to convert outgoing midi events from `event::Event` to `api::Events`.
/// It only allocates memory in new() and reuses the memory between calls.
pub struct SendEventBuffer {
    /// The `api::Events` header and its pointer array, in `u64` words: the storage is
    /// aligned for `api::Events` (8) and holds at least `size_of::<api::Events>()`
    /// bytes whatever the capacity.
    buf: Vec<u64>,
    api_events: Vec<PlaceholderEvent>, // using SysExEvent to store both because it's larger than MidiEvent
}

impl std::fmt::Debug for SendEventBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendEventBuffer")
            .field("api_events", &self.api_events)
            .finish_non_exhaustive()
    }
}

impl Default for SendEventBuffer {
    fn default() -> Self {
        SendEventBuffer::new(1024)
    }
}

impl SendEventBuffer {
    /// Creates a buffer for sending up to the given number of midi events per frame
    #[inline(always)]
    pub fn new(capacity: usize) -> Self {
        let header_size = mem::size_of::<api::Events>() - (mem::size_of::<*mut api::Event>() * 2);
        let body_size = mem::size_of::<*mut api::Event>() * capacity;
        let bytes = (header_size + body_size).max(mem::size_of::<api::Events>());
        let mut buf = vec![0u64; bytes.div_ceil(mem::size_of::<u64>())];
        // SAFETY: all-zero is a valid `SysExEvent`: integers, null pointers and
        // `EventType::_Placeholder` (discriminant 0).
        let api_events = vec![unsafe { mem::zeroed::<PlaceholderEvent>() }; capacity];
        {
            let ptrs = {
                let e = Self::buf_as_api_events(&mut buf);
                e.num_events = capacity as i32;
                e.events_raw_mut()
            };
            for (ptr, event) in ptrs.iter_mut().zip(&api_events) {
                let (ptr, event): (&mut *const PlaceholderEvent, &PlaceholderEvent) = (ptr, event);
                *ptr = event;
            }
        }
        Self { buf, api_events }
    }

    /// Sends events to the host. See the `fwd_midi` example.
    ///
    /// # Example
    /// ```no_run
    /// # use vst_tutti::plugin::{Info, Plugin, HostCallback};
    /// # use vst_tutti::buffer::{AudioBuffer, SendEventBuffer};
    /// # use vst_tutti::host::Host;
    /// # use vst_tutti::event::*;
    /// # struct ExamplePlugin { host: HostCallback, send_buffer: SendEventBuffer }
    /// # impl Plugin for ExamplePlugin {
    /// #     fn new(host: HostCallback) -> Self { Self { host, send_buffer: Default::default() } }
    /// #
    /// #     fn get_info(&self) -> Info { Default::default() }
    /// #
    /// fn process(&mut self, buffer: &mut AudioBuffer<f32>){
    ///     let events: Vec<MidiEvent> = vec![
    ///         // ...
    ///     ];
    ///     self.send_buffer.send_events(&events, &mut self.host);
    /// }
    /// # }
    /// ```
    #[inline(always)]
    pub fn send_events<T: IntoIterator<Item = U>, U: WriteIntoPlaceholder>(
        &mut self,
        events: T,
        host: &mut dyn Host,
    ) {
        self.store_events(events);
        host.process_events(self.events());
    }

    /// Stores events in the buffer, replacing the buffer's current content.
    /// Use this in [`process_events`](crate::Plugin::process_events) to store received input events, then read them in [`process`](crate::Plugin::process) using [`events`](SendEventBuffer::events).
    #[inline(always)]
    pub fn store_events<T: IntoIterator<Item = U>, U: WriteIntoPlaceholder>(&mut self, events: T) {
        #[allow(clippy::suspicious_map)]
        let count = events
            .into_iter()
            .zip(self.api_events.iter_mut())
            .map(|(ev, out)| ev.write_into(out))
            .count();
        self.set_num_events(count);
    }

    /// Returns a reference to the stored events
    #[inline(always)]
    pub fn events(&self) -> &api::Events {
        // SAFETY: `buf` is a `Vec<u64>`, aligned to 8 as `api::Events` is, and `new` sizes
        // it to at least `size_of::<api::Events>()` bytes (all of them initialised: zeroed
        // words, then `num_events` and the pointers written through this same type), and
        // every bit pattern of its integer and pointer fields is valid. The reference
        // borrows `self`, so the words are not written while it lives.
        unsafe { &*(self.buf.as_ptr() as *const api::Events) }
    }

    /// Clears the buffer
    #[inline(always)]
    pub fn clear(&mut self) {
        self.set_num_events(0);
    }

    #[inline(always)]
    fn buf_as_api_events(buf: &mut [u64]) -> &mut api::Events {
        // SAFETY: as for `events`: `buf` is aligned for `api::Events` and `new` sizes it to
        // at least `size_of::<api::Events>()` bytes; the borrow of `buf` is the reference's.
        unsafe { &mut *(buf.as_mut_ptr() as *mut api::Events) }
    }

    #[inline(always)]
    fn set_num_events(&mut self, events_len: usize) {
        use std::cmp::min;
        let e = Self::buf_as_api_events(&mut self.buf);
        e.num_events = min(self.api_events.len(), events_len) as i32;
    }
}

#[cfg(test)]
mod tests {
    use crate::buffer::AudioBuffer;

    /// Size of buffers used in tests.
    const SIZE: usize = 1024;

    /// Test that creating and zipping buffers works.
    ///
    /// This test creates a channel for 2 inputs and 2 outputs.
    /// The input channels are simply values
    /// from 0 to `SIZE-1` (e.g. [0, 1, 2, 3, 4, .. , SIZE - 1])
    /// and the output channels are just 0.
    /// This test assures that when the buffers are zipped together,
    /// the input values do not change.
    #[test]
    fn buffer_zip() {
        let in1: Vec<f32> = (0..SIZE).map(|x| x as f32).collect();
        let in2 = in1.clone();

        let mut out1 = vec![0.0; SIZE];
        let mut out2 = out1.clone();

        let inputs = [in1.as_ptr(), in2.as_ptr()];
        let mut outputs = vec![out1.as_mut_ptr(), out2.as_mut_ptr()];
        // SAFETY: `inputs`/`outputs` hold the stated counts of pointers to live Vecs of
        // `SIZE` samples, distinct per channel, all outliving `buffer`.
        let mut buffer =
            unsafe { AudioBuffer::from_raw(2, 2, inputs.as_ptr(), outputs.as_mut_ptr(), SIZE) };

        for (input, output) in buffer.zip() {
            input
                .iter()
                .zip(output.iter_mut())
                .fold(0, |acc, (input, output)| {
                    assert_eq!(*input, acc as f32);
                    assert_eq!(*output, 0.0);
                    acc + 1
                });
        }
    }

    // Test that the `zip()` method returns an iterator that gives `n` elements
    // where n is the number of inputs when this is lower than the number of outputs.
    #[test]
    fn buffer_zip_fewer_inputs_than_outputs() {
        let in1 = vec![1.0; SIZE];
        let in2 = vec![2.0; SIZE];

        let mut out1 = vec![3.0; SIZE];
        let mut out2 = vec![4.0; SIZE];
        let mut out3 = vec![5.0; SIZE];

        let inputs = [in1.as_ptr(), in2.as_ptr()];
        let mut outputs = vec![out1.as_mut_ptr(), out2.as_mut_ptr(), out3.as_mut_ptr()];
        // SAFETY: `inputs`/`outputs` hold the stated counts of pointers to live Vecs of
        // `SIZE` samples, distinct per channel, all outliving `buffer`.
        let mut buffer =
            unsafe { AudioBuffer::from_raw(2, 3, inputs.as_ptr(), outputs.as_mut_ptr(), SIZE) };

        let mut iter = buffer.zip();
        if let Some((observed_in1, observed_out1)) = iter.next() {
            assert_eq!(1.0, observed_in1[0]);
            assert_eq!(3.0, observed_out1[0]);
        } else {
            unreachable!();
        }

        if let Some((observed_in2, observed_out2)) = iter.next() {
            assert_eq!(2.0, observed_in2[0]);
            assert_eq!(4.0, observed_out2[0]);
        } else {
            unreachable!();
        }

        assert_eq!(None, iter.next());
    }

    // Test that the `zip()` method returns an iterator that gives `n` elements
    // where n is the number of outputs when this is lower than the number of inputs.
    #[test]
    fn buffer_zip_more_inputs_than_outputs() {
        let in1 = vec![1.0; SIZE];
        let in2 = vec![2.0; SIZE];
        let in3 = vec![3.0; SIZE];

        let mut out1 = vec![4.0; SIZE];
        let mut out2 = vec![5.0; SIZE];

        let inputs = [in1.as_ptr(), in2.as_ptr(), in3.as_ptr()];
        let mut outputs = vec![out1.as_mut_ptr(), out2.as_mut_ptr()];
        // SAFETY: `inputs`/`outputs` hold the stated counts of pointers to live Vecs of
        // `SIZE` samples, distinct per channel, all outliving `buffer`.
        let mut buffer =
            unsafe { AudioBuffer::from_raw(3, 2, inputs.as_ptr(), outputs.as_mut_ptr(), SIZE) };

        let mut iter = buffer.zip();

        if let Some((observed_in1, observed_out1)) = iter.next() {
            assert_eq!(1.0, observed_in1[0]);
            assert_eq!(4.0, observed_out1[0]);
        } else {
            unreachable!();
        }

        if let Some((observed_in2, observed_out2)) = iter.next() {
            assert_eq!(2.0, observed_in2[0]);
            assert_eq!(5.0, observed_out2[0]);
        } else {
            unreachable!();
        }

        assert_eq!(None, iter.next());
    }

    /// Test that creating buffers from raw pointers works.
    #[test]
    fn from_raw() {
        let in1: Vec<f32> = (0..SIZE).map(|x| x as f32).collect();
        let in2 = in1.clone();

        let mut out1 = vec![0.0; SIZE];
        let mut out2 = out1.clone();

        let inputs = [in1.as_ptr(), in2.as_ptr()];
        let mut outputs = vec![out1.as_mut_ptr(), out2.as_mut_ptr()];
        // SAFETY: `inputs`/`outputs` hold the stated counts of pointers to live Vecs of
        // `SIZE` samples, distinct per channel, all outliving `buffer`.
        let mut buffer =
            unsafe { AudioBuffer::from_raw(2, 2, inputs.as_ptr(), outputs.as_mut_ptr(), SIZE) };

        for (input, output) in buffer.zip() {
            input
                .iter()
                .zip(output.iter_mut())
                .fold(0, |acc, (input, output)| {
                    assert_eq!(*input, acc as f32);
                    assert_eq!(*output, 0.0);
                    acc + 1
                });
        }
    }

    /// `SendEventBuffer`'s header storage is aligned for `api::Events` and holds
    /// the whole struct at every capacity, `0` and `1` included (they formed a
    /// `&mut Events` over a 16-byte buffer).
    ///
    /// Mutation (run): size the buffer as `header_size + body_size` alone and
    /// make it a `Vec<u8>` again: the size assertion fails for capacity 0 and 1
    /// (the alignment half is the allocator's luck and cannot be shown).
    #[test]
    fn send_event_buffer_storage_holds_an_events_header_at_any_capacity() {
        use super::SendEventBuffer;
        for capacity in 0..4 {
            let buffer = SendEventBuffer::new(capacity);
            let events = buffer.events() as *const crate::api::Events;
            assert!(events.is_aligned(), "capacity {capacity}");
            assert!(
                buffer.buf.len() * std::mem::size_of::<u64>()
                    >= std::mem::size_of::<crate::api::Events>(),
                "capacity {capacity}"
            );
            assert_eq!(buffer.events().num_events, capacity as i32);
        }
    }
}
