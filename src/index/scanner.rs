use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crossbeam_channel::Sender;
use walkdir::WalkDir;

use crate::model::FileRecord;

use super::{Command, send_batch, send_scan_failed, send_scan_finished, send_scan_progress};

const BATCH_SIZE: usize = 1_000;
const PROGRESS_INTERVAL: u64 = 1_000;

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
            let mut batch = Vec::with_capacity(BATCH_SIZE);
            let mut indexed = 0_u64;
            let mut skipped = 0_u64;
            let mut last_progress = 0_u64;

            let walker = WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    !super::is_excluded_path(entry.path(), &excluded_roots)
                });

            for entry in walker {
                if !thread_scanning.load(Ordering::Acquire) {
                    break;
                }

                match entry {
                    Ok(entry) => {
                        if let Some(record) = FileRecord::from_path(entry.path()) {
                            batch.push(record);
                            indexed += 1;
                        } else {
                            skipped += 1;
                        }
                    }
                    Err(_) => skipped += 1,
                }

                if batch.len() >= BATCH_SIZE {
                    if !send_batch(&thread_sender, std::mem::take(&mut batch)) {
                        return;
                    }
                    batch.reserve(BATCH_SIZE);
                }

                let discovered = indexed + skipped;
                if discovered.saturating_sub(last_progress) >= PROGRESS_INTERVAL {
                    send_scan_progress(&thread_sender, discovered, skipped);
                    last_progress = discovered;
                }
            }

            if !batch.is_empty() && !send_batch(&thread_sender, batch) {
                return;
            }
            send_scan_progress(&thread_sender, indexed + skipped, skipped);
            send_scan_finished(&thread_sender, indexed, skipped);
        });

    if let Err(error) = result {
        scanning.store(false, Ordering::Release);
        send_scan_failed(&sender, format!("Unable to start file scanner: {error}"));
    }
}
