#![feature(slice_group_by, int_log, type_alias_impl_trait)]

use crate::pipeline::build_unitigs::build_unitigs;
use crate::pipeline::compute_matchtigs::{compute_matchtigs_thread, MatchtigsStorageBackend};
use crate::pipeline::hashes_sorting::hashes_sorting;
use crate::pipeline::links_compaction::links_compaction;
use crate::pipeline::maximal_unitig_links::build_maximal_unitigs_links;
use crate::pipeline::reorganize_reads::reorganize_reads;
use ::static_dispatch::static_dispatch;
use colors::colors_manager::ColorsManager;
use colors::colors_manager::ColorsMergeManager;
use config::{
    get_compression_level_info, get_memory_mode, SwapPriority, DEFAULT_PER_CPU_BUFFER_SIZE,
    INTERMEDIATE_COMPRESSION_LEVEL_FAST, INTERMEDIATE_COMPRESSION_LEVEL_SLOW, KEEP_FILES,
    MAXIMUM_SECOND_BUCKETS_LOG, MINIMUM_LOG_DELTA_TIME,
};
use hashes::{HashFunctionFactory, MinimizerHashFunctionFactory};
use io::concurrent::structured_sequences::binary::StructSeqBinaryWriter;
use io::concurrent::structured_sequences::fasta::FastaWriter;
use io::concurrent::structured_sequences::StructuredSequenceWriter;
use io::{compute_stats_from_input_files, generate_bucket_names};
use kmers_merge::structs::RetType;
use parallel_processor::buckets::concurrent::BucketsThreadBuffer;
use parallel_processor::buckets::writers::compressed_binary_writer::CompressedCheckpointSize;
use parallel_processor::buckets::writers::lock_free_binary_writer::LockFreeBinaryWriter;
use parallel_processor::buckets::MultiThreadBuckets;
use parallel_processor::memory_data_size::MemoryDataSize;
use parallel_processor::memory_fs::{MemoryFs, RemoveFileMode};
use parallel_processor::phase_times_monitor::PHASES_TIMES_MONITOR;
use parallel_processor::utils::scoped_thread_local::ScopedThreadLocal;
use std::fs::remove_file;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

mod pipeline;
mod structs;

pub use pipeline::compute_matchtigs::MatchtigMode;

#[derive(PartialEq, PartialOrd)]
pub enum AssemblerStartingStep {
    MinimizerBucketing = 0,
    KmersMerge = 1,
    HashesSorting = 2,
    LinksCompaction = 3,
    ReorganizeReads = 4,
    BuildUnitigs = 5,
    MaximalUnitigsLinks = 6,
}

#[static_dispatch(BucketingHash = [
    hashes::cn_nthash::CanonicalNtHashIteratorFactory,
    #[cfg(not(feature = "devel-build"))] hashes::fw_nthash::ForwardNtHashIteratorFactory
], MergingHash = [
    #[cfg(not(feature = "devel-build"))] hashes::fw_seqhash::u16::ForwardSeqHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::fw_seqhash::u32::ForwardSeqHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::fw_seqhash::u64::ForwardSeqHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::fw_seqhash::u128::ForwardSeqHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::fw_rkhash::u32::ForwardRabinKarpHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::fw_rkhash::u64::ForwardRabinKarpHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::fw_rkhash::u128::ForwardRabinKarpHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::cn_seqhash::u16::CanonicalSeqHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::cn_seqhash::u32::CanonicalSeqHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::cn_seqhash::u64::CanonicalSeqHashFactory,
    hashes::cn_seqhash::u128::CanonicalSeqHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::cn_rkhash::u32::CanonicalRabinKarpHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::cn_rkhash::u64::CanonicalRabinKarpHashFactory,
    #[cfg(not(feature = "devel-build"))] hashes::cn_rkhash::u128::CanonicalRabinKarpHashFactory,
], AssemblerColorsManager = [
    #[cfg(not(feature = "devel-build"))] colors::bundles::multifile_building::ColorBundleMultifileBuilding,
    colors::non_colored::NonColoredManager,
])]
pub fn run_assembler<
    BucketingHash: MinimizerHashFunctionFactory,
    MergingHash: HashFunctionFactory,
    AssemblerColorsManager: ColorsManager,
>(
    k: usize,
    m: usize,
    step: AssemblerStartingStep,
    last_step: AssemblerStartingStep,
    input: Vec<PathBuf>,
    output_file: PathBuf,
    temp_dir: PathBuf,
    threads_count: usize,
    min_multiplicity: usize,
    buckets_count_log: Option<usize>,
    loopit_number: Option<usize>,
    default_compression_level: Option<u32>,
    generate_maximal_unitigs_links: bool,
    compute_tigs_mode: Option<MatchtigMode>,
    only_bstats: bool,
) {
    PHASES_TIMES_MONITOR.write().init();

    let file_stats = compute_stats_from_input_files(&input);

    let buckets_count_log = buckets_count_log.unwrap_or_else(|| file_stats.best_buckets_count_log);

    if let Some(default_compression_level) = default_compression_level {
        INTERMEDIATE_COMPRESSION_LEVEL_SLOW.store(default_compression_level, Ordering::Relaxed);
        INTERMEDIATE_COMPRESSION_LEVEL_FAST.store(default_compression_level, Ordering::Relaxed);
    }

    let buckets_count = 1 << buckets_count_log;

    let color_names: Vec<_> = input
        .iter()
        .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
        .collect();

    let global_colors_table = Arc::new(
        AssemblerColorsManager::ColorsMergeManagerType::create_colors_table(
            output_file.with_extension("colors.dat"),
            color_names,
        ),
    );

    let (buckets, counters) = if step <= AssemblerStartingStep::MinimizerBucketing {
        assembler_minimizer_bucketing::static_dispatch::minimizer_bucketing::<
            BucketingHash,
            AssemblerColorsManager,
        >(
            input,
            temp_dir.as_path(),
            buckets_count,
            threads_count,
            k,
            m,
        )
    } else {
        (
            generate_bucket_names(temp_dir.join("bucket"), buckets_count, None),
            temp_dir.join("buckets-counters.dat"),
        )
    };

    println!(
        "Temp buckets files size: {:.2}",
        MemoryDataSize::from_bytes(fs_extra::dir::get_size(&temp_dir).unwrap_or(0) as usize)
    );

    if last_step <= AssemblerStartingStep::MinimizerBucketing {
        PHASES_TIMES_MONITOR
            .write()
            .print_stats("Completed minimizer bucketing.".to_string());
        return;
    } else {
        MemoryFs::flush_all_to_disk();
        MemoryFs::free_memory();
    }

    if only_bstats {
        use rayon::prelude::*;
        buckets.par_iter().enumerate().for_each(|(index, bucket)| {
            kmers_transform::debug_bucket_stats::compute_stats_for_bucket::<
                BucketingHash,
                MergingHash,
            >(
                bucket.clone(),
                index,
                buckets.len(),
                MAXIMUM_SECOND_BUCKETS_LOG,
                k,
                m,
            );
        });
        return;
    }

    let RetType { sequences, hashes } = if step <= AssemblerStartingStep::KmersMerge {
        kmers_merge::kmers_merge::<BucketingHash, MergingHash, AssemblerColorsManager, _>(
            buckets,
            counters,
            global_colors_table.clone(),
            buckets_count,
            min_multiplicity,
            temp_dir.as_path(),
            k,
            m,
            threads_count,
        )
    } else {
        RetType {
            sequences: generate_bucket_names(temp_dir.join("result"), buckets_count, None),
            hashes: generate_bucket_names(temp_dir.join("hashes"), buckets_count, None),
        }
    };
    if last_step <= AssemblerStartingStep::KmersMerge {
        PHASES_TIMES_MONITOR
            .write()
            .print_stats("Completed kmers merge.".to_string());
        return;
    } else {
        MemoryFs::flush_all_to_disk();
        MemoryFs::free_memory();
    }

    AssemblerColorsManager::ColorsMergeManagerType::print_color_stats(&global_colors_table);

    drop(global_colors_table);

    let mut links = if step <= AssemblerStartingStep::HashesSorting {
        hashes_sorting::<MergingHash, _>(hashes, temp_dir.as_path(), buckets_count)
    } else {
        generate_bucket_names(temp_dir.join("links"), buckets_count, None)
    };
    if last_step <= AssemblerStartingStep::HashesSorting {
        PHASES_TIMES_MONITOR
            .write()
            .print_stats("Hashes sorting.".to_string());
        return;
    } else {
        MemoryFs::flush_all_to_disk();
        MemoryFs::free_memory();
    }

    let mut loop_iteration = loopit_number.unwrap_or(0);

    let unames = generate_bucket_names(temp_dir.join("unitigs_map"), buckets_count, None);
    let rnames = generate_bucket_names(temp_dir.join("results_map"), buckets_count, None);

    // let mut links_manager = UnitigLinksManager::new(buckets_count);

    let (unitigs_map, reads_map) = if step <= AssemblerStartingStep::LinksCompaction {
        for file in unames {
            let _ = remove_file(file);
        }

        for file in rnames {
            let _ = remove_file(file);
        }

        let result_map_buckets = Arc::new(MultiThreadBuckets::<LockFreeBinaryWriter>::new(
            buckets_count,
            temp_dir.join("results_map"),
            &(
                get_memory_mode(SwapPriority::FinalMaps),
                LockFreeBinaryWriter::CHECKPOINT_SIZE_UNLIMITED,
            ),
        ));

        let final_buckets = Arc::new(MultiThreadBuckets::<LockFreeBinaryWriter>::new(
            buckets_count,
            temp_dir.join("unitigs_map"),
            &(
                get_memory_mode(SwapPriority::FinalMaps),
                LockFreeBinaryWriter::CHECKPOINT_SIZE_UNLIMITED,
            ),
        ));

        if loop_iteration != 0 {
            links = generate_bucket_names(
                temp_dir.join(format!("linksi{}", loop_iteration - 1)),
                buckets_count,
                None,
            );
        }

        PHASES_TIMES_MONITOR
            .write()
            .start_phase("phase: links compaction".to_string());

        let mut log_timer = Instant::now();

        let links_scoped_buffer = ScopedThreadLocal::new(move || {
            BucketsThreadBuffer::new(DEFAULT_PER_CPU_BUFFER_SIZE, buckets_count)
        });
        let results_map_scoped_buffer = ScopedThreadLocal::new(move || {
            BucketsThreadBuffer::new(DEFAULT_PER_CPU_BUFFER_SIZE, buckets_count)
        });

        let result = loop {
            let do_logging = if log_timer.elapsed() > MINIMUM_LOG_DELTA_TIME {
                log_timer = Instant::now();
                true
            } else {
                false
            };

            if do_logging {
                println!("Iteration: {}", loop_iteration);
            }

            let (new_links, remaining) = links_compaction(
                links,
                temp_dir.as_path(),
                buckets_count,
                loop_iteration,
                &result_map_buckets,
                &final_buckets,
                // &links_manager,
                &links_scoped_buffer,
                &results_map_scoped_buffer,
            );

            if do_logging {
                println!(
                    "Remaining: {} {}",
                    remaining,
                    PHASES_TIMES_MONITOR
                        .read()
                        .get_formatted_counter_without_memory()
                );
            }

            links = new_links;
            if remaining == 0 {
                println!("Completed compaction with {} iters", loop_iteration);
                break (final_buckets.finalize(), result_map_buckets.finalize());
            }
            loop_iteration += 1;
        };

        for link_file in links {
            MemoryFs::remove_file(
                &link_file,
                RemoveFileMode::Remove {
                    remove_fs: !KEEP_FILES.load(Ordering::Relaxed),
                },
            )
            .unwrap();
        }
        result
    } else {
        (unames, rnames)
    };

    if last_step <= AssemblerStartingStep::LinksCompaction {
        PHASES_TIMES_MONITOR
            .write()
            .print_stats("Links Compaction.".to_string());
        return;
    } else {
        MemoryFs::flush_all_to_disk();
        MemoryFs::free_memory();
    }

    let final_unitigs_file = StructuredSequenceWriter::new(match output_file.extension() {
        Some(ext) => match ext.to_string_lossy().to_string().as_str() {
            "lz4" => FastaWriter::new_compressed_lz4(&output_file, 2),
            "gz" => FastaWriter::new_compressed_gzip(&output_file, 2),
            _ => FastaWriter::new_plain(&output_file),
        },
        None => FastaWriter::new_plain(&output_file),
    });

    // Temporary file to store maximal unitigs data without links info, if further processing is requested
    let compressed_temp_unitigs_file =
        if generate_maximal_unitigs_links || compute_tigs_mode.is_some() {
            Some(StructuredSequenceWriter::new(StructSeqBinaryWriter::new(
                temp_dir.join("maximal_unitigs.tmp"),
                &(
                    get_memory_mode(SwapPriority::FinalMaps as usize),
                    CompressedCheckpointSize::new_from_size(MemoryDataSize::from_mebioctets(4)),
                    get_compression_level_info(),
                ),
            )))
        } else {
            None
        };

    let (reorganized_reads, _final_unitigs_bucket) = if step
        <= AssemblerStartingStep::ReorganizeReads
    {
        if generate_maximal_unitigs_links || compute_tigs_mode.is_some() {
            reorganize_reads::<
                BucketingHash,
                MergingHash,
                AssemblerColorsManager,
                StructSeqBinaryWriter<_, _>,
            >(
                sequences,
                reads_map,
                temp_dir.as_path(),
                compressed_temp_unitigs_file.as_ref().unwrap(),
                buckets_count,
            )
        } else {
            reorganize_reads::<BucketingHash, MergingHash, AssemblerColorsManager, FastaWriter<_, _>>(
                sequences,
                reads_map,
                temp_dir.as_path(),
                &final_unitigs_file,
                buckets_count,
            )
        }
    } else {
        (
            generate_bucket_names(temp_dir.join("reads_bucket"), buckets_count, Some("tmp")),
            (generate_bucket_names(temp_dir.join("reads_bucket_lonely"), 1, Some("tmp"))
                .into_iter()
                .next()
                .unwrap()),
        )
    };

    if last_step <= AssemblerStartingStep::ReorganizeReads {
        PHASES_TIMES_MONITOR
            .write()
            .print_stats("Reorganize reads.".to_string());
        return;
    } else {
        MemoryFs::flush_all_to_disk();
        MemoryFs::free_memory();
    }

    // links_manager.compute_id_offsets();

    if step <= AssemblerStartingStep::BuildUnitigs {
        if generate_maximal_unitigs_links || compute_tigs_mode.is_some() {
            build_unitigs::<
                BucketingHash,
                MergingHash,
                AssemblerColorsManager,
                StructSeqBinaryWriter<_, _>,
            >(
                reorganized_reads,
                unitigs_map,
                temp_dir.as_path(),
                compressed_temp_unitigs_file.as_ref().unwrap(),
                k,
            );
        } else {
            build_unitigs::<BucketingHash, MergingHash, AssemblerColorsManager, FastaWriter<_, _>>(
                reorganized_reads,
                unitigs_map,
                temp_dir.as_path(),
                &final_unitigs_file,
                k,
            );
        }
    }

    if step <= AssemblerStartingStep::MaximalUnitigsLinks {
        if generate_maximal_unitigs_links || compute_tigs_mode.is_some() {
            let compressed_temp_unitigs_file = compressed_temp_unitigs_file.unwrap();
            let temp_path = compressed_temp_unitigs_file.get_path();
            compressed_temp_unitigs_file.finalize();

            if let Some(compute_tigs_mode) = compute_tigs_mode {
                let matchtigs_backend = MatchtigsStorageBackend::new();

                let matchtigs_receiver = matchtigs_backend.get_receiver();

                let handle = std::thread::Builder::new()
                    .name("greedy_matchtigs".to_string())
                    .spawn(move || {
                        compute_matchtigs_thread::<
                            BucketingHash,
                            MergingHash,
                            AssemblerColorsManager,
                            _,
                        >(
                            k,
                            threads_count,
                            matchtigs_receiver,
                            &final_unitigs_file,
                            compute_tigs_mode,
                        );
                    })
                    .unwrap();

                build_maximal_unitigs_links::<
                    BucketingHash,
                    MergingHash,
                    AssemblerColorsManager,
                    MatchtigsStorageBackend<_>,
                >(
                    temp_path,
                    temp_dir.as_path(),
                    &StructuredSequenceWriter::new(matchtigs_backend),
                    k,
                );

                handle.join().unwrap();
            } else if generate_maximal_unitigs_links {
                final_unitigs_file.finalize();

                let final_unitigs_file =
                    StructuredSequenceWriter::new(match output_file.extension() {
                        Some(ext) => match ext.to_string_lossy().to_string().as_str() {
                            "lz4" => FastaWriter::new_compressed_lz4(&output_file, 2),
                            "gz" => FastaWriter::new_compressed_gzip(&output_file, 2),
                            _ => FastaWriter::new_plain(&output_file),
                        },
                        None => FastaWriter::new_plain(&output_file),
                    });

                build_maximal_unitigs_links::<
                    BucketingHash,
                    MergingHash,
                    AssemblerColorsManager,
                    FastaWriter<_, _>,
                >(temp_path, temp_dir.as_path(), &final_unitigs_file, k);
                final_unitigs_file.finalize();
            }
        } else {
            final_unitigs_file.finalize();
        }
    } else {
        final_unitigs_file.finalize();
    }

    let _ = std::fs::remove_dir(temp_dir.as_path());

    PHASES_TIMES_MONITOR
        .write()
        .print_stats("Compacted De Bruijn graph construction completed.".to_string());

    println!("Final output saved to: {}", output_file.display());
}
