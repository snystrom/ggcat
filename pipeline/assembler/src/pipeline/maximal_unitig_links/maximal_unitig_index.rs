use byteorder::ReadBytesExt;
use io::concurrent::structured_sequences::IdentSequenceWriter;
use io::concurrent::temp_reads::extra_data::{
    SequenceExtraData, SequenceExtraDataTempBufferManagement,
};
use io::varint::{decode_varint, encode_varint, VARINT_MAX_SIZE};
use parallel_processor::buckets::bucket_writer::BucketItem;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use utils::vec_slice::VecSlice;

#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct MaximalUnitigFlags(u8);

impl MaximalUnitigFlags {
    const FLIP_CURRENT: usize = 0;
    const FLIP_OTHER: usize = 1;

    #[inline(always)]
    fn get_bit(&self, pos: usize) -> bool {
        (self.0 & (1 << pos)) != 0
    }

    #[inline(always)]
    pub const fn new_direction(flip_current: bool, flip_other: bool) -> MaximalUnitigFlags {
        MaximalUnitigFlags(
            ((flip_current as u8) << Self::FLIP_CURRENT) | ((flip_other as u8) << Self::FLIP_OTHER),
        )
    }

    pub fn flip_other(&self) -> bool {
        self.get_bit(Self::FLIP_OTHER)
    }

    #[inline(always)]
    pub fn flip_current(&self) -> bool {
        self.get_bit(Self::FLIP_CURRENT)
    }
}

#[derive(Copy, Clone, Eq)]
pub struct MaximalUnitigIndex {
    index: u64,
    pub flags: MaximalUnitigFlags,
}

impl Hash for MaximalUnitigIndex {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.index());
    }
}

impl PartialEq for MaximalUnitigIndex {
    fn eq(&self, other: &Self) -> bool {
        other.index() == self.index()
    }
}

impl SequenceExtraData for MaximalUnitigIndex {
    type TempBuffer = ();

    fn decode_extended(_: &mut (), reader: &mut impl Read) -> Option<Self> {
        let index = decode_varint(|| reader.read_u8().ok())?;
        let flags = reader.read_u8().ok()?;
        Some(MaximalUnitigIndex::new(index, MaximalUnitigFlags(flags)))
    }

    fn encode_extended(&self, _: &(), writer: &mut impl Write) {
        encode_varint(|b| writer.write_all(b).ok(), self.index() as u64).unwrap();
        writer.write_all(&[self.flags.0]).unwrap();
    }

    fn max_size(&self) -> usize {
        VARINT_MAX_SIZE * 2
    }
}

impl Debug for MaximalUnitigIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "MaximalUnitigIndex {{ index: {} }}",
            self.index()
        ))
    }
}

impl MaximalUnitigIndex {
    #[inline]
    pub fn new(index: u64, flags: MaximalUnitigFlags) -> Self {
        Self { index, flags }
    }

    #[inline]
    pub fn index(&self) -> u64 {
        self.index
    }
}

#[derive(Clone, Debug)]
pub struct MaximalUnitigLink {
    index: u64,
    pub entries: VecSlice<MaximalUnitigIndex>,
}

impl MaximalUnitigLink {
    pub const fn new(index: u64, entries: VecSlice<MaximalUnitigIndex>) -> Self {
        Self { index, entries }
    }

    pub fn index(&self) -> u64 {
        self.index
    }
}

impl BucketItem for MaximalUnitigLink {
    type ExtraData = Vec<MaximalUnitigIndex>;
    type ReadBuffer = Vec<MaximalUnitigIndex>;
    type ExtraDataBuffer = ();
    type ReadType<'a> = Self;

    #[inline(always)]
    fn write_to(&self, bucket: &mut Vec<u8>, extra_data: &Self::ExtraData, _: &()) {
        encode_varint(|b| bucket.write_all(b), self.index()).unwrap();

        let entries = self.entries.get_slice(extra_data);
        encode_varint(|b| bucket.write_all(b), entries.len() as u64).unwrap();

        for entry in entries {
            encode_varint(|b| bucket.write_all(b), entry.index() as u64).unwrap();
            bucket.push(entry.flags.0);
        }
    }

    fn read_from<'a, S: Read>(
        mut stream: S,
        read_buffer: &'a mut Self::ReadBuffer,
        _: &mut (),
    ) -> Option<Self::ReadType<'a>> {
        let entry = decode_varint(|| stream.read_u8().ok())?;

        let len = decode_varint(|| stream.read_u8().ok())? as usize;

        let start = read_buffer.len();
        for _i in 0..len {
            let index = decode_varint(|| stream.read_u8().ok())?;
            let flags = stream.read_u8().ok()?;
            read_buffer.push(MaximalUnitigIndex::new(index, MaximalUnitigFlags(flags)));
        }

        Some(Self::new(entry, VecSlice::new(start, len)))
    }

    fn get_size(&self, _: &Vec<MaximalUnitigIndex>) -> usize {
        16 + self.entries.len() * (VARINT_MAX_SIZE + 1)
    }
}

#[derive(Clone, Debug)]
pub struct DoubleMaximalUnitigLinks(pub [MaximalUnitigLink; 2]);

impl DoubleMaximalUnitigLinks {
    pub const EMPTY: Self = Self([
        MaximalUnitigLink::new(0, VecSlice::new(0, 0)),
        MaximalUnitigLink::new(0, VecSlice::new(0, 0)),
    ]);
}

impl SequenceExtraDataTempBufferManagement<Vec<MaximalUnitigIndex>> for DoubleMaximalUnitigLinks {
    fn new_temp_buffer() -> Vec<MaximalUnitigIndex> {
        vec![]
    }

    fn clear_temp_buffer(buffer: &mut Vec<MaximalUnitigIndex>) {
        buffer.clear()
    }

    fn copy_temp_buffer(dest: &mut Vec<MaximalUnitigIndex>, src: &Vec<MaximalUnitigIndex>) {
        dest.clear();
        dest.extend_from_slice(&src);
    }

    fn copy_extra_from(
        extra: Self,
        src: &Vec<MaximalUnitigIndex>,
        dst: &mut Vec<MaximalUnitigIndex>,
    ) -> Self {
        Self {
            0: [
                {
                    let entries = extra.0[0].entries.get_slice(src);
                    let start = dst.len();
                    dst.extend_from_slice(entries);
                    MaximalUnitigLink::new(extra.0[0].index(), VecSlice::new(start, entries.len()))
                },
                {
                    let entries = extra.0[1].entries.get_slice(src);
                    let start = dst.len();
                    dst.extend_from_slice(entries);
                    MaximalUnitigLink::new(extra.0[1].index(), VecSlice::new(start, entries.len()))
                },
            ],
        }
    }
}

impl SequenceExtraData for DoubleMaximalUnitigLinks {
    type TempBuffer = Vec<MaximalUnitigIndex>;

    fn decode_extended(_buffer: &mut Self::TempBuffer, _reader: &mut impl Read) -> Option<Self> {
        unimplemented!()
    }

    fn encode_extended(&self, _buffer: &Self::TempBuffer, _writer: &mut impl Write) {
        unimplemented!()
    }

    fn max_size(&self) -> usize {
        unimplemented!()
    }
}

impl IdentSequenceWriter for DoubleMaximalUnitigLinks {
    fn write_as_ident(&self, stream: &mut impl Write, extra_buffer: &Self::TempBuffer) {
        for entries in &self.0 {
            let entries = entries.entries.get_slice(extra_buffer);
            for entry in entries {
                write!(
                    stream,
                    " L:{}:{}:{}",
                    if entry.flags.flip_current() { "-" } else { "+" },
                    entry.index,
                    if entry.flags.flip_other() { "-" } else { "+" },
                )
                .unwrap();
            }
        }
    }

    #[allow(unused_variables)]
    fn write_as_gfa(&self, stream: &mut impl Write, extra_buffer: &Self::TempBuffer) {
        todo!()
    }

    fn parse_as_ident<'a>(_ident: &[u8], _extra_buffer: &mut Self::TempBuffer) -> Option<Self> {
        unimplemented!()
    }

    fn parse_as_gfa<'a>(_ident: &[u8], _extra_buffer: &mut Self::TempBuffer) -> Option<Self> {
        unimplemented!()
    }
}
