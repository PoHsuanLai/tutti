//! Mixed Data Sets (M2-104 section 7.9): the fields of a chunk header as
//! types, a checked complete set, [`MixedDataPackets`] that writes it as
//! chunks, and [`MixedDataAssembler`] that reads chunks back into it.

use core::iter::FusedIterator;
use core::num::NonZeroU16;

use crate::{Data128, ManufacturerId, Message, MidiGroup, MixedDataHeader, U4, Ump};

/// The bytes of a Mixed Data Set UMP: a chunk's header is one UMP.
const UMP_BYTES: u16 = 16;
/// The data bytes one payload UMP carries: the UMP less its two prefix bytes
/// (message type, Group, status and MDS id).
const PAYLOAD_BYTES: usize = 14;
/// The most payload UMPs one chunk holds: its size, header included, is a
/// 16-bit field.
const MAX_UMPS: u16 = (u16::MAX - UMP_BYTES) / UMP_BYTES;
/// The most Groups a UMP stream has, so the most messages open at once when
/// each Group has one.
const LANES: usize = 16;

/// The size of a Mixed Data Set chunk in bytes, header included
/// (section 7.9, "Number of Valid Bytes in this Message Chunk").
///
/// A chunk is a header UMP of 16 bytes and then payload UMPs of 16 bytes
/// each, the last one padded: the size counts the bytes of those UMPs, the
/// two prefix bytes of every payload UMP among them. This is the reading of
/// the section's "multiple of 16" and its "16 (header bytes only)". So the
/// smallest chunk is 16 bytes, a header with no payload, and a size that ends
/// one byte into a payload UMP's prefix ([`new`](Self::new) refuses
/// `16 + 16n + 1`) has no data to carry.
///
/// ```
/// use tutti_ump::MdsChunkSize;
///
/// let size = MdsChunkSize::from_data_len(30).unwrap();
/// assert_eq!(size.get(), 16 + 2 * 16 + 4);
/// assert_eq!((size.payload_umps(), size.data_len()), (3, 30));
/// assert_eq!(MdsChunkSize::new(15), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u16", into = "u16"))]
pub struct MdsChunkSize(u16);

/// A word that is no [`MdsChunkSize`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a Mixed Data Set chunk size: at least 16, and not one byte into a payload UMP")]
pub struct NotChunkSize(pub u16);

impl MdsChunkSize {
    /// A chunk of a header and no payload: 16 bytes.
    pub const HEADER_ONLY: Self = Self(UMP_BYTES);

    /// The size `raw`, or `None` below 16 (a chunk has its header) or when it
    /// ends one byte into a payload UMP's two prefix bytes. Never clamps.
    #[must_use]
    pub const fn new(raw: u16) -> Option<Self> {
        if raw < UMP_BYTES || (raw - UMP_BYTES) % UMP_BYTES == 1 {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// The size of a literal or constant.
    ///
    /// # Panics
    ///
    /// If `raw` is not a chunk size.
    #[must_use]
    pub const fn of(raw: u16) -> Self {
        match Self::new(raw) {
            Some(size) => size,
            None => panic!("not a Mixed Data Set chunk size"),
        }
    }

    /// The size of a chunk that carries `len` data bytes, or `None` when it
    /// does not fit 16 bits.
    #[must_use]
    pub const fn from_data_len(len: usize) -> Option<Self> {
        let umps = len.div_ceil(PAYLOAD_BYTES);
        let bytes = UMP_BYTES as usize + len + 2 * umps;
        if bytes > u16::MAX as usize {
            None
        } else {
            Some(Self(bytes as u16))
        }
    }

    /// The size in bytes.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The payload UMPs that follow the header: as many as it takes to deliver
    /// the size.
    #[must_use]
    pub const fn payload_umps(self) -> u16 {
        (self.0 - UMP_BYTES).div_ceil(UMP_BYTES)
    }

    /// The data bytes the payload UMPs carry: the size less the header and
    /// the prefix of each payload UMP. The rest of the last UMP is padding.
    #[must_use]
    pub const fn data_len(self) -> u16 {
        self.0 - UMP_BYTES - 2 * self.payload_umps()
    }
}

impl TryFrom<u16> for MdsChunkSize {
    type Error = NotChunkSize;

    fn try_from(raw: u16) -> Result<Self, NotChunkSize> {
        Self::new(raw).ok_or(NotChunkSize(raw))
    }
}

impl From<MdsChunkSize> for u16 {
    fn from(size: MdsChunkSize) -> u16 {
        size.0
    }
}

/// How many chunks a set has (section 7.9, "Number of Chunks in Mixed Data
/// Set"): a count, or unknown for streaming data, which the last chunk
/// resolves by declaring its own number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MdsChunks {
    /// Zero on the wire: not known yet.
    Unknown,
    /// The count, from 1.
    Of(NonZeroU16),
}

impl MdsChunks {
    /// The count a wire word means: zero is unknown. Every word is one.
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        match NonZeroU16::new(raw) {
            Some(n) => Self::Of(n),
            None => Self::Unknown,
        }
    }

    /// The wire word.
    #[must_use]
    pub const fn get(self) -> u16 {
        match self {
            Self::Unknown => 0,
            Self::Of(n) => n.get(),
        }
    }
}

/// A chunk's number (section 7.9, "Number of This Chunk"): counted from 1, or
/// zero in a final chunk of a set whose data the sender cannot vouch for
/// (section 7.9.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MdsChunk {
    /// Zero on the wire: the set ends here and its data is not verified.
    Unverified,
    /// The chunk's number, from 1.
    Number(NonZeroU16),
}

impl MdsChunk {
    /// The chunk a wire word means. Every word is one.
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        match NonZeroU16::new(raw) {
            Some(n) => Self::Number(n),
            None => Self::Unverified,
        }
    }

    /// The wire word.
    #[must_use]
    pub const fn get(self) -> u16 {
        match self {
            Self::Unverified => 0,
            Self::Number(n) => n.get(),
        }
    }
}

/// Whose data a Mixed Data Set is: the manufacturer, device and the two sub
/// ids of its header (section 7.9). With a Universal System Exclusive
/// manufacturer id the device id and sub ids are the MMA/AMEI specification's;
/// with a manufacturer's own they are the manufacturer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MdsIdentity {
    /// The 16-bit manufacturer id (section 7.10).
    pub manufacturer: ManufacturerId,
    /// The device id; `0xFFFF` calls every device.
    pub device: u16,
    /// Sub id 1.
    pub sub_id_1: u16,
    /// Sub id 2.
    pub sub_id_2: u16,
}

/// How many payload UMPs a chunk of [`MixedDataPackets`] holds (`1..=4094`):
/// the most a 16-bit chunk size reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MdsChunkUmps(u16);

impl MdsChunkUmps {
    /// The count, or `None` for zero (a chunk would carry no data) or more
    /// than 4094 (its size would not fit 16 bits).
    #[must_use]
    pub const fn new(raw: u16) -> Option<Self> {
        if raw >= 1 && raw <= MAX_UMPS {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// The count of a literal or constant.
    ///
    /// # Panics
    ///
    /// If `raw` is zero or above 4094.
    #[must_use]
    pub const fn of(raw: u16) -> Self {
        match Self::new(raw) {
            Some(n) => n,
            None => panic!("a chunk holds 1 to 4094 payload UMPs"),
        }
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The most data bytes such a chunk carries.
    const fn data_len(self) -> usize {
        self.0 as usize * PAYLOAD_BYTES
    }
}

impl From<MdsChunkUmps> for u16 {
    fn from(n: MdsChunkUmps) -> u16 {
        n.0
    }
}

/// A complete Mixed Data Set: its Group, MDS id, identity and data bytes.
///
/// A sender builds one and writes it with [`packets`](Self::packets); the
/// [`MixedDataAssembler`] hands one back, borrowing its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixedDataSet<'a> {
    /// The Group it travels on.
    pub group: MidiGroup,
    /// Which of the Group's 16 simultaneous sets it is.
    pub mds_id: U4,
    /// Whose data it is.
    pub identity: MdsIdentity,
    /// The data: the payload of every chunk, joined, without padding.
    pub data: &'a [u8],
}

impl<'a> MixedDataSet<'a> {
    /// The set as chunks of up to `chunk` payload UMPs each: see
    /// [`MixedDataPackets`]. `None` when the set would need more than 65535
    /// chunks, the most a 16-bit chunk count names.
    #[must_use]
    pub fn packets(self, chunk: MdsChunkUmps) -> Option<MixedDataPackets<'a>> {
        let per_chunk = chunk.data_len();
        let total = u16::try_from(self.data.len().div_ceil(per_chunk).max(1)).ok()?;
        Some(MixedDataPackets {
            set: self,
            per_chunk,
            total,
            chunk: 0,
            pending: &[],
            rest: self.data,
            header_due: true,
        })
    }
}

/// The UMPs of a [`MixedDataSet`] (section 7.9): for each chunk a header and
/// its payload UMPs, built by [`MixedDataSet::packets`].
///
/// Every chunk but the last carries the full payload of its UMPs. The chunks
/// are numbered from 1, each header states the set's chunk count (never
/// unknown), and the last chunk's number equals it, which ends the set. An
/// empty set is one header-only chunk. Nothing is allocated.
/// [`MixedDataAssembler`] is the inverse.
///
/// ```
/// use tutti_ump::{
///     MdsChunkUmps, MdsIdentity, ManufacturerId, MidiGroup, MixedDataSet, U4,
/// };
///
/// let set = MixedDataSet {
///     group: MidiGroup::of(0),
///     mds_id: U4::of(3),
///     identity: MdsIdentity {
///         manufacturer: ManufacturerId::NON_COMMERCIAL,
///         device: 0xFFFF,
///         sub_id_1: 0,
///         sub_id_2: 0,
///     },
///     data: &[7; 30],
/// };
/// // Fourteen bytes to a payload UMP: three UMPs and a header, in one chunk.
/// assert_eq!(set.packets(MdsChunkUmps::of(8)).unwrap().len(), 4);
/// // Two payload UMPs to a chunk: two chunks, five UMPs.
/// assert_eq!(set.packets(MdsChunkUmps::of(2)).unwrap().len(), 5);
/// ```
#[derive(Clone, Debug)]
pub struct MixedDataPackets<'a> {
    set: MixedDataSet<'a>,
    /// The data bytes of a full chunk.
    per_chunk: usize,
    total: u16,
    /// The number of the chunk whose header was last written; 0 before any.
    chunk: u16,
    /// The data of the current chunk not yet written.
    pending: &'a [u8],
    /// The data of the chunks after the current one.
    rest: &'a [u8],
    header_due: bool,
}

impl Iterator for MixedDataPackets<'_> {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        if self.header_due {
            if self.chunk == self.total {
                return None;
            }
            self.chunk += 1;
            let (this, rest) = self.rest.split_at(self.rest.len().min(self.per_chunk));
            self.pending = this;
            self.rest = rest;
            self.header_due = this.is_empty();
            let chunk_size = MdsChunkSize::from_data_len(this.len())
                .expect("a chunk holds at most 4094 payload UMPs");
            return Some(Ump::from(Message::Data128(
                self.set.group,
                Data128::MixedDataHeader(MixedDataHeader {
                    mds_id: self.set.mds_id,
                    chunk_size,
                    chunks: MdsChunks::new(self.total),
                    chunk: MdsChunk::new(self.chunk),
                    identity: self.set.identity,
                }),
            )));
        }
        let (bytes, rest) = self.pending.split_at(self.pending.len().min(PAYLOAD_BYTES));
        self.pending = rest;
        self.header_due = rest.is_empty();
        let mut padded = [0u8; PAYLOAD_BYTES];
        padded[..bytes.len()].copy_from_slice(bytes);
        Some(Ump::from(Message::Data128(
            self.set.group,
            Data128::MixedDataPayload {
                mds_id: self.set.mds_id,
                bytes: padded,
            },
        )))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let headers = usize::from(self.total - self.chunk);
        let n = headers
            + self.pending.len().div_ceil(PAYLOAD_BYTES)
            + self.rest.len().div_ceil(PAYLOAD_BYTES);
        (n, Some(n))
    }
}

impl ExactSizeIterator for MixedDataPackets<'_> {}
impl FusedIterator for MixedDataPackets<'_> {}

/// What one UMP did to a [`MixedDataAssembler`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixedDataStep<'a> {
    /// Not a Mixed Data Set UMP: for the caller to read as usual.
    NotMixedData,
    /// A Mixed Data Set UMP, taken in with no set to hand on yet: a part of
    /// one, or of one that was dropped (and counted).
    Held,
    /// A set completed.
    Complete(MixedDataSet<'a>),
}

/// The state of one open set.
#[derive(Clone, Copy, Debug)]
enum Run {
    /// A set that is being collected, its data in the entry's lane.
    Assembling(Open),
    /// A set that was dropped and counted: its remaining UMPs are skipped.
    Discarding {
        /// Payload UMPs of the current chunk still to come.
        skip: u16,
        /// Whether the current chunk is the set's last.
        ends: bool,
    },
}

#[derive(Clone, Copy, Debug)]
struct Open {
    identity: MdsIdentity,
    /// The number of the chunk last accepted.
    chunk: u16,
    /// Data bytes held in the lane.
    len: usize,
    /// Payload UMPs of the current chunk still to come.
    umps: u16,
    /// Data bytes of the current chunk still to come.
    data_left: u16,
    /// Whether the current chunk is the set's last.
    ends: bool,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    group: MidiGroup,
    mds_id: U4,
    run: Run,
}

/// Reassembles Mixed Data Sets (section 7.9) that [`MixedDataPackets`] writes
/// into whole sets, in a buffer the caller owns and that is never grown. It
/// allocates nothing.
///
/// A consumer feeds every UMP of its stream to [`push`](Self::push). The
/// assembler reads the Mixed Data Set UMPs of message type `0x5` and
/// answers [`MixedDataStep::NotMixedData`] to the rest (a System Exclusive 8
/// packet included). A completed set borrows the assembler until the next
/// `push`.
///
/// # Rules (section 7.9 and 7.9.1)
///
/// - **A set is one MDS id on one Group:** sixteen sets are open at once on a
///   Group, and Groups are independent. Other UMPs between a set's UMPs end
///   nothing (section 7.9.1 allows them).
/// - **A set is chunks numbered 1, 2, 3, ...** in order. A header whose number
///   is not the next one drops the set, counted. A header numbered 1 begins a
///   set, and drops an open one.
/// - **A chunk is its header and then as many payload UMPs as its size says**
///   ([`MdsChunkSize::payload_umps`]). The next header before they are all in,
///   or a payload UMP past them, drops the set, counted.
/// - **The set ends** at the chunk whose number equals the chunk count its
///   header gives. A chunk count of [`MdsChunks::Unknown`] lets the set go on
///   until a chunk declares a count equal to its own number. A chunk number
///   above the declared count drops the set.
/// - **A chunk numbered [`MdsChunk::Unverified`] ends the set with its data
///   not vouched for:** the set is dropped, counted if one was open.
/// - The identity delivered is the first chunk's: the section does not say
///   a later header repeats it, so a differing one is not an error.
/// - **Over capacity the whole set is dropped,** never cut: more data than
///   `max_data` bytes, or no free lane. Its remaining UMPs are skipped.
/// - A payload UMP, or a header that is not numbered 1, with no open set is
///   dropped, counted once for the set: the UMPs after it are skipped until a
///   header numbered 1 or one that ends the set.
///
/// At most `lanes` sets are open at once (at most sixteen); a dropped set that
/// is still being skipped holds one of them too. When all are held, a UMP of
/// a set with no lane is counted on its own, so [`dropped`](Self::dropped)
/// can exceed the sets lost.
///
/// ```
/// use tutti_ump::{
///     ManufacturerId, MdsChunkUmps, MdsIdentity, MidiGroup, MixedDataAssembler, MixedDataSet,
///     MixedDataStep, U4,
/// };
///
/// let set = MixedDataSet {
///     group: MidiGroup::of(0),
///     mds_id: U4::of(1),
///     identity: MdsIdentity {
///         manufacturer: ManufacturerId::NON_COMMERCIAL,
///         device: 0xFFFF,
///         sub_id_1: 1,
///         sub_id_2: 2,
///     },
///     data: b"a mixed data set of 33 bytes....!",
/// };
/// let mut assembler = MixedDataAssembler::new([0u8; 64], 64, 1);
/// let mut done = 0;
/// for ump in set.packets(MdsChunkUmps::of(1)).unwrap() {
///     if let MixedDataStep::Complete(whole) = assembler.push(ump) {
///         assert_eq!(whole, set);
///         done += 1;
///     }
/// }
/// assert_eq!(done, 1);
/// ```
#[derive(Debug)]
pub struct MixedDataAssembler<B> {
    buf: B,
    lanes: usize,
    max_data: usize,
    entries: [Option<Entry>; LANES],
    dropped: u64,
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> MixedDataAssembler<B> {
    /// An assembler over `buf` for sets of up to `max_data` data bytes, with up
    /// to `lanes` open at once (at most sixteen). It needs `lanes * max_data`
    /// bytes: as many lanes as `buf` holds are used.
    #[must_use]
    pub fn new(buf: B, max_data: usize, lanes: usize) -> Self {
        let fit = buf.as_ref().len().checked_div(max_data).unwrap_or(0);
        Self {
            buf,
            lanes: lanes.min(LANES).min(fit),
            max_data,
            entries: [None; LANES],
            dropped: 0,
        }
    }

    /// The buffer length that holds `lanes` open sets of `max_data` bytes.
    #[must_use]
    pub const fn room_for(max_data: usize, lanes: usize) -> usize {
        max_data * lanes
    }

    /// Sets dropped whole since this assembler was made: over capacity, out of
    /// order, cut short or over long in a chunk, ended unverified, cut by
    /// [`lost_tail`](Self::lost_tail), open at a [`reset`](Self::reset), or
    /// found no lane.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// An upstream queue dropped UMPs after the ones just pushed: drop every
    /// open set, counted.
    pub fn lost_tail(&mut self) {
        for entry in &mut self.entries {
            if let Some(Entry {
                run: Run::Assembling(_),
                ..
            }) = entry
            {
                self.dropped += 1;
            }
            *entry = None;
        }
    }

    /// The consumer was reset: drop the open sets (counted).
    pub fn reset(&mut self) {
        self.lost_tail();
    }

    /// Take in one UMP of the stream, whatever it is. Never allocates.
    pub fn push(&mut self, ump: Ump) -> MixedDataStep<'_> {
        match ump.decode() {
            Ok(Message::Data128(group, Data128::MixedDataHeader(h))) => self.header(group, h),
            Ok(Message::Data128(group, Data128::MixedDataPayload { mds_id, bytes })) => {
                self.payload(group, mds_id, &bytes)
            }
            _ => MixedDataStep::NotMixedData,
        }
    }

    fn find(&self, group: MidiGroup, mds_id: U4) -> Option<usize> {
        (0..self.lanes)
            .find(|&i| matches!(self.entries[i], Some(e) if e.group == group && e.mds_id == mds_id))
    }

    fn free(&self) -> Option<usize> {
        (0..self.lanes).find(|&i| self.entries[i].is_none())
    }

    fn lane(&mut self, i: usize) -> &mut [u8] {
        &mut self.buf.as_mut()[i * self.max_data..][..self.max_data]
    }

    /// Set `i` is dropped: counted if it was being collected, then skipped.
    fn discard(&mut self, i: usize, group: MidiGroup, h: &MixedDataHeader, count: bool) {
        if count {
            self.dropped += 1;
        }
        let ends = ends(h);
        let skip = h.chunk_size.payload_umps();
        self.entries[i] = if skip == 0 && ends {
            None
        } else {
            Some(Entry {
                group,
                mds_id: h.mds_id,
                run: Run::Discarding { skip, ends },
            })
        };
    }

    fn header(&mut self, group: MidiGroup, h: MixedDataHeader) -> MixedDataStep<'_> {
        let first = h.chunk == MdsChunk::Number(NonZeroU16::MIN);
        let Some(i) = self.find(group, h.mds_id) else {
            // No open set: only chunk 1 begins one.
            if first {
                if let Some(i) = self.free() {
                    return self.begin(i, group, &h);
                }
                self.dropped += 1;
            } else if let Some(i) = self.free() {
                self.discard(i, group, &h, h.chunk != MdsChunk::Unverified);
            } else if h.chunk != MdsChunk::Unverified {
                self.dropped += 1;
            }
            return MixedDataStep::Held;
        };
        let Some(Entry { run, .. }) = self.entries[i] else {
            return MixedDataStep::Held;
        };
        match run {
            Run::Discarding { .. } if first => self.begin(i, group, &h),
            Run::Discarding { .. } => {
                self.discard(i, group, &h, false);
                MixedDataStep::Held
            }
            Run::Assembling(_) if first => {
                self.dropped += 1;
                self.begin(i, group, &h)
            }
            Run::Assembling(open) => {
                let in_order = open.umps == 0
                    && matches!(h.chunk, MdsChunk::Number(n) if n.get() == open.chunk.wrapping_add(1));
                let past_end = matches!(
                    (h.chunks, h.chunk),
                    (MdsChunks::Of(total), MdsChunk::Number(n)) if n > total
                );
                if in_order && !past_end {
                    self.accept(i, group, &h, open)
                } else {
                    self.discard(i, group, &h, true);
                    MixedDataStep::Held
                }
            }
        }
    }

    /// A set begins with chunk 1 in free entry `i`.
    fn begin(&mut self, i: usize, group: MidiGroup, h: &MixedDataHeader) -> MixedDataStep<'_> {
        let open = Open {
            identity: h.identity,
            chunk: 0,
            len: 0,
            umps: 0,
            data_left: 0,
            ends: false,
        };
        self.accept(i, group, h, open)
    }

    /// Chunk `h` follows `open`'s: take its header in.
    fn accept(
        &mut self,
        i: usize,
        group: MidiGroup,
        h: &MixedDataHeader,
        mut open: Open,
    ) -> MixedDataStep<'_> {
        let MdsChunk::Number(n) = h.chunk else {
            unreachable!("chunk 0 is never accepted")
        };
        open.chunk = n.get();
        open.umps = h.chunk_size.payload_umps();
        open.data_left = h.chunk_size.data_len();
        open.ends = ends(h);
        self.entries[i] = Some(Entry {
            group,
            mds_id: h.mds_id,
            run: Run::Assembling(open),
        });
        if open.umps == 0 && open.ends {
            return self.deliver(i);
        }
        MixedDataStep::Held
    }

    fn payload(&mut self, group: MidiGroup, mds_id: U4, bytes: &[u8; 14]) -> MixedDataStep<'_> {
        let Some(i) = self.find(group, mds_id) else {
            // A payload UMP with no header before it.
            self.dropped += 1;
            if let Some(i) = self.free() {
                self.entries[i] = Some(Entry {
                    group,
                    mds_id,
                    run: Run::Discarding {
                        skip: 0,
                        ends: false,
                    },
                });
            }
            return MixedDataStep::Held;
        };
        let Some(Entry { run, .. }) = self.entries[i] else {
            return MixedDataStep::Held;
        };
        match run {
            Run::Discarding { skip, ends } => {
                let skip = skip.saturating_sub(1);
                self.entries[i] = if skip == 0 && ends {
                    None
                } else {
                    Some(Entry {
                        group,
                        mds_id,
                        run: Run::Discarding { skip, ends },
                    })
                };
                MixedDataStep::Held
            }
            Run::Assembling(mut open) => {
                let take = usize::from(open.data_left).min(PAYLOAD_BYTES);
                if open.umps == 0 || open.len + take > self.max_data {
                    // Past the chunk's payload, or past the buffer.
                    self.dropped += 1;
                    let skip = open.umps.saturating_sub(1);
                    self.entries[i] = if open.umps != 0 && skip == 0 && open.ends {
                        None
                    } else {
                        Some(Entry {
                            group,
                            mds_id,
                            run: Run::Discarding {
                                skip,
                                ends: open.ends && open.umps != 0,
                            },
                        })
                    };
                    return MixedDataStep::Held;
                }
                let len = open.len;
                self.lane(i)[len..len + take].copy_from_slice(&bytes[..take]);
                open.len += take;
                open.umps -= 1;
                open.data_left -= take as u16;
                self.entries[i] = Some(Entry {
                    group,
                    mds_id,
                    run: Run::Assembling(open),
                });
                if open.umps == 0 && open.ends {
                    return self.deliver(i);
                }
                MixedDataStep::Held
            }
        }
    }

    /// Entry `i`'s set is whole: free it and hand it on.
    fn deliver(&mut self, i: usize) -> MixedDataStep<'_> {
        let Some(Entry {
            group,
            mds_id,
            run: Run::Assembling(open),
        }) = self.entries[i].take()
        else {
            return MixedDataStep::Held;
        };
        let max = self.max_data;
        let data = &self.buf.as_ref()[i * max..][..open.len];
        MixedDataStep::Complete(MixedDataSet {
            group,
            mds_id,
            identity: open.identity,
            data,
        })
    }
}

/// Whether chunk `h` is the set's last: its number equals the chunk count it
/// declares (section 7.9), or it is numbered zero (section 7.9.1).
fn ends(h: &MixedDataHeader) -> bool {
    match (h.chunk, h.chunks) {
        (MdsChunk::Unverified, _) => true,
        (MdsChunk::Number(n), MdsChunks::Of(total)) => n == total,
        (MdsChunk::Number(_), MdsChunks::Unknown) => false,
    }
}
