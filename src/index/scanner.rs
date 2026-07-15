use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use walkdir::WalkDir;

use crate::model::FileRecord;

use super::{Command, send_batch, send_scan_failed, send_scan_finished, send_scan_progress};

const BATCH_SIZE: usize = 1_000;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
const JOBS_PER_WORKER: usize = 64;

struct ScanJob {
    sequence: u64,
    path: PathBuf,
}

struct ScanOutcome {
    sequence: u64,
    record: Option<FileRecord>,
}

pub(super) fn spawn_scan(
    root: PathBuf,
    excluded_roots: Arc<[PathBuf]>,
    sender: Sender<Command>,
    scanning: Arc<AtomicBool>,
) {
    let thread_sender = sender.clone();
    let thread_scanning = scanning.clone();
    let result = thread::Builder::new()
        .name("file-scanner".to_owned())
        .spawn(move || {
            run_parallel_scan(
                root,
                excluded_roots,
                thread_sender,
                thread_scanning,
            );
        });

    if let Err(error) = result {
        scanning.store(false, Ordering::Release);
        send_scan_failed(&sender, format!("Unable to start file scanner: {error}"));
    }
}

fn run_parallel_scan(
    root: PathBuf,
    excluded_roots: Arc<[PathBuf]>,
    sender: Sender<Command>,
    scanning: Arc<AtomicBool>,
) {
    let worker_count = metadata_worker_count();
    let queue_capacity = worker_count * JOBS_PER_WORKER;
    let (job_tx, job_rx) = bounded::<ScanJob>(queue_capacity);
    let (outcome_tx, outcome_rx) = bounded::<ScanOutcome>(queue_capacity);
    let (permit_tx, permit_rx) = bounded::<()>(queue_capacity);
    // Keep the out-of-order buffer bounded even if one metadata lookup is
    // unusually slow. A permit is returned only after that sequence number is
    // committed to the ordered batch.
    for _ in 0..queue_capacity {
        let _ = permit_tx.send(());
    }
    let walk_skipped = Arc::new(AtomicU64::new(0));

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let worker_jobs = job_rx.clone();
            let worker_outcomes = outcome_tx.clone();
            let worker_scanning = Arc::clone(&scanning);
            scope.spawn(move || {
                metadata_worker(worker_jobs, worker_outcomes, worker_scanning);
            });
        }
        drop(job_rx);
        drop(outcome_tx);

        let producer_scanning = Arc::clone(&scanning);
        let producer_skipped = Arc::clone(&walk_skipped);
        scope.spawn(move || {
            produce_scan_jobs(
                root,
                excluded_roots,
                job_tx,
                permit_rx,
                producer_scanning,
                producer_skipped,
            );
        });

        collect_scan_outcomes(
            outcome_rx,
            permit_tx,
            &sender,
            &scanning,
            &walk_skipped,
        );
    });
}

fn metadata_worker(
    jobs: Receiver<ScanJob>,
    outcomes: Sender<ScanOutcome>,
    scanning: Arc<AtomicBool>,
) {
    while let Ok(job) = jobs.recv() {
        let record = scanning
            .load(Ordering::Acquire)
            .then(|| FileRecord::from_path(&job.path))
            .flatten();
        if outcomes
            .send(ScanOutcome {
                sequence: job.sequence,
                record,
            })
            .is_err()
        {
            break;
        }
    }
}

fn produce_scan_jobs(
    root: PathBuf,
    excluded_roots: Arc<[PathBuf]>,
    jobs: Sender<ScanJob>,
    permits: Receiver<()>,
    scanning: Arc<AtomicBool>,
    skipped: Arc<AtomicU64>,
) {
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !super::is_excluded_path(entry.path(), &excluded_roots));
    let mut sequence = 0_u64;

    for entry in walker {
        if !scanning.load(Ordering::Acquire) {
            break;
        }
        match entry {
            Ok(entry) => {
                if permits.recv().is_err() {
                    break;
                }
                if jobs
                    .send(ScanJob {
                        sequence,
                        path: entry.into_path(),
                    })
                    .is_err()
                {
                    break;
                }
                sequence = sequence.saturating_add(1);
            }
            Err(_) => {
                skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn collect_scan_outcomes(
    outcomes: Receiver<ScanOutcome>,
    permits: Sender<()>,
    sender: &Sender<Command>,
    scanning: &AtomicBool,
    walk_skipped: &AtomicU64,
) {
    let mut pending = BTreeMap::<u64, Option<FileRecord>>::new();
    let mut next_sequence = 0_u64;
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut indexed = 0_u64;
    let mut metadata_skipped = 0_u64;
    let mut database_available = true;
    let mut last_progress = Instant::now();

    while let Ok(outcome) = outcomes.recv() {
        pending.insert(outcome.sequence, outcome.record);
        while let Some(record) = pending.remove(&next_sequence) {
            next_sequence = next_sequence.saturating_add(1);
            if permits.send(()).is_err() {
                scanning.store(false, Ordering::Release);
                return;
            }
            if let Some(record) = record {
                if database_available {
                    batch.push(record);
                    indexed = indexed.saturating_add(1);
                }
            } else {
                metadata_skipped = metadata_skipped.saturating_add(1);
            }

            if database_available && batch.len() >= BATCH_SIZE {
                if !send_batch(sender, std::mem::take(&mut batch)) {
                    database_available = false;
                    scanning.store(false, Ordering::Release);
                } else {
                    batch.reserve(BATCH_SIZE);
                }
            }
        }

        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            let skipped = metadata_skipped.saturating_add(walk_skipped.load(Ordering::Relaxed));
            send_scan_progress(sender, indexed.saturating_add(skipped), skipped);
            last_progress = Instant::now();
        }
    }

    if !database_available || !scanning.load(Ordering::Acquire) {
        return;
    }
    if !batch.is_empty() && !send_batch(sender, batch) {
        return;
    }
    let skipped = metadata_skipped.saturating_add(walk_skipped.load(Ordering::Relaxed));
    send_scan_progress(sender, indexed.saturating_add(skipped), skipped);
    send_scan_finished(sender, indexed, skipped);
}

fn metadata_worker_count() -> usize {
    thread::available_parallelism()
        .map(|parallelism| parallelism.get().saturating_sub(1).clamp(1, 8))
        .unwrap_or(4)
}
