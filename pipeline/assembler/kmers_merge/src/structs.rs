use config::BucketIndexType;
use io::concurrent::temp_reads::creads_utils::CompressedReadsBucketHelper;
use io::concurrent::temp_reads::extra_data::SequenceExtraData;
use parallel_processor::buckets::bucket_writer::BucketItem;
use parallel_processor::buckets::writers::compressed_binary_writer::CompressedBinaryWriter;
use parallel_processor::buckets::LockFreeBucket;
use std::marker::PhantomData;
use std::path::PathBuf;
use utils::owned_drop::OwnedDrop;

pub struct ResultsBucket<X: SequenceExtraData> {
    pub read_index: u64,
    pub reads_writer: OwnedDrop<CompressedBinaryWriter>,
    pub temp_buffer: Vec<u8>,
    pub bucket_index: BucketIndexType,
    pub _phantom: PhantomData<X>,
}

impl<X: SequenceExtraData> ResultsBucket<X> {
    pub fn add_read(&mut self, el: X, read: &[u8], extra_buffer: &X::TempBuffer) -> u64 {
        self.temp_buffer.clear();
        CompressedReadsBucketHelper::<X, typenum::U0, false>::new(read, 0, 0).write_to(
            &mut self.temp_buffer,
            &el,
            extra_buffer,
        );
        self.reads_writer.write_data(self.temp_buffer.as_slice());

        let read_index = self.read_index;
        self.read_index += 1;
        read_index
    }

    pub fn get_bucket_index(&self) -> BucketIndexType {
        self.bucket_index
    }
}

impl<X: SequenceExtraData> Drop for ResultsBucket<X> {
    fn drop(&mut self) {
        unsafe { self.reads_writer.take().finalize() }
    }
}

pub struct RetType {
    pub sequences: Vec<PathBuf>,
    pub hashes: Vec<PathBuf>,
}
