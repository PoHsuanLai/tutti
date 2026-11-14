/// One cache line (64 bytes).
///
/// Each reader's slot and the `current` pointer get one of their own, so a
/// lend writes a line no other reader reads or writes.
#[repr(align(64))]
pub(crate) struct Padded<T>(pub(crate) T);
