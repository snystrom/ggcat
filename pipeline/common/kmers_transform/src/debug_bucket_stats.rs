use config::{
    BucketIndexType, DEFAULT_OUTPUT_BUFFER_SIZE, DEFAULT_PREFETCH_AMOUNT, READ_FLAG_INCL_END,
    USE_SECOND_BUCKET,
};
use hashes::{
    ExtendableHashTraitType, HashFunction, HashFunctionFactory, HashableSequence,
    MinimizerHashFunctionFactory,
};
use io::compressed_read::CompressedRead;
use io::concurrent::temp_reads::creads_utils::CompressedReadsBucketHelper;
use parallel_processor::buckets::readers::async_binary_reader::{
    AsyncBinaryReader, AsyncReaderThread,
};
use parallel_processor::memory_fs::RemoveFileMode;
use std::collections::HashSet;
use std::path::PathBuf;

fn get_sequence_bucket<C, H: MinimizerHashFunctionFactory>(
    k: usize,
    m: usize,
    seq_data: &(u8, u8, C, CompressedRead),
    used_hash_bits: usize,
    bucket_bits_count: usize,
) -> BucketIndexType {
    let read = &seq_data.3;
    let flags = seq_data.0;
    let decr_val = ((read.bases_count() == k) && (flags & READ_FLAG_INCL_END) == 0) as usize;

    let hashes = H::new(read.sub_slice((1 - decr_val)..(k - decr_val)), m);

    let minimizer = hashes
        .iter()
        .min_by_key(|k| H::get_full_minimizer(k.to_unextendable()))
        .unwrap();

    H::get_bucket(
        used_hash_bits,
        bucket_bits_count,
        minimizer.to_unextendable(),
    )
}

pub fn compute_stats_for_bucket<H: MinimizerHashFunctionFactory, MH: HashFunctionFactory>(
    bucket: PathBuf,
    bucket_index: usize,
    buckets_count: usize,
    second_buckets_log_max: usize,
    k: usize,
    m: usize,
) {
    let reader = AsyncBinaryReader::new(
        &bucket,
        true,
        RemoveFileMode::Remove { remove_fs: false },
        DEFAULT_PREFETCH_AMOUNT,
    );

    let file_size = reader.get_file_size();

    let reader_thread = AsyncReaderThread::new(DEFAULT_OUTPUT_BUFFER_SIZE, 4);

    let second_buckets_max = 1 << second_buckets_log_max;

    let mut hash_maps = (0..second_buckets_max)
        .map(|_| HashSet::new())
        .collect::<Vec<_>>();

    let mut items_iterator = reader.get_items_stream::<CompressedReadsBucketHelper<
        (),
        typenum::U2,
        { USE_SECOND_BUCKET },
    >>(reader_thread.clone(), Vec::new(), ());

    let mut total_counters = vec![0; second_buckets_max];

    while let Some((read_info, _)) = items_iterator.next() {
        let orig_bucket = get_sequence_bucket::<(), H>(
            k,
            m,
            &read_info,
            buckets_count.ilog2() as usize,
            second_buckets_log_max,
        ) as usize;

        let hashes = MH::new(read_info.3, k);

        for hash in hashes.iter() {
            total_counters[orig_bucket] += 1;
            hash_maps[orig_bucket].insert(hash.to_unextendable());
        }
    }

    let counters_string = hash_maps
        .iter()
        .zip(total_counters.iter())
        .map(|(h, t)| format!("({}/{})", h.len(), t))
        .collect::<Vec<String>>()
        .join(";");

    let tot_seqs = total_counters.iter().sum::<usize>();
    let uniq_seqs = hash_maps.iter().map(|h| h.len()).sum::<usize>();

    println!("Stats for bucket: {}", bucket_index);
    println!(
        "FSIZE: {} SEQUENCES: {}/{} UNIQUE_RATIO: {} COMPR_RATIO: {} ",
        file_size,
        tot_seqs,
        uniq_seqs,
        (tot_seqs as f64 / uniq_seqs as f64),
        (file_size as f64 / tot_seqs as f64)
    );
    println!("Results: {}", counters_string);
}
