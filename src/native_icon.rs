use eframe::egui;

#[cfg(target_os = "macos")]
mod imp {
    use super::egui;
    use crossbeam_channel::{
        Receiver, RecvTimeoutError, SendTimeoutError, Sender, TryRecvError, TrySendError, bounded,
    };
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const ICON_SIDE: usize = 64;
    const ICON_BYTE_COUNT: usize = ICON_SIDE * ICON_SIDE * 4;
    const REQUEST_CHANNEL_CAPACITY: usize = 64;
    const RESULT_CHANNEL_CAPACITY: usize = 64;
    const MAX_RESULTS_PER_FRAME: usize = 8;
    const MAX_READY_ICONS: usize = 512;
    const MAX_CACHE_ENTRIES: usize = MAX_READY_ICONS + REQUEST_CHANNEL_CAPACITY;
    const FAILED_RETRY_DELAY: Duration = Duration::from_secs(30);
    const CHANNEL_WAIT: Duration = Duration::from_millis(50);

    struct IconRequest {
        id: u64,
        path: String,
        revision: Option<i64>,
        generation: u64,
    }

    struct IconResult {
        id: u64,
        path: String,
        revision: Option<i64>,
        rgba: Option<Vec<u8>>,
    }

    enum CacheEntry {
        Pending {
            id: u64,
            revision: Option<i64>,
        },
        Ready {
            revision: Option<i64>,
            texture: egui::TextureHandle,
            last_used: u64,
        },
        Failed {
            revision: Option<i64>,
            failed_at: Instant,
            last_used: u64,
        },
    }

    impl CacheEntry {
        fn revision(&self) -> Option<i64> {
            match self {
                Self::Pending { revision, .. }
                | Self::Ready { revision, .. }
                | Self::Failed { revision, .. } => *revision,
            }
        }
    }

    pub struct NativeIconCache {
        entries: HashMap<String, CacheEntry>,
        request_tx: Sender<IconRequest>,
        result_rx: Receiver<IconResult>,
        stop_tx: Sender<()>,
        worker: Option<JoinHandle<()>>,
        generation: Arc<AtomicU64>,
        next_request_id: u64,
        access_clock: u64,
    }

    impl NativeIconCache {
        pub fn new(context: &egui::Context) -> Self {
            let (request_tx, request_rx) = bounded(REQUEST_CHANNEL_CAPACITY);
            let (result_tx, result_rx) = bounded(RESULT_CHANNEL_CAPACITY);
            let (stop_tx, stop_rx) = bounded(1);
            let repaint_context = context.clone();
            let generation = Arc::new(AtomicU64::new(1));
            let worker_generation = Arc::clone(&generation);

            let worker = thread::Builder::new()
                .name("finder-icon-loader".to_owned())
                .spawn(move || {
                    worker_loop(
                        request_rx,
                        result_tx,
                        stop_rx,
                        repaint_context,
                        worker_generation,
                    );
                })
                .ok();

            Self {
                entries: HashMap::new(),
                request_tx,
                result_rx,
                stop_tx,
                worker,
                generation,
                next_request_id: 1,
                access_clock: 0,
            }
        }

        pub fn drain(&mut self, context: &egui::Context) {
            let mut processed = 0;
            for _ in 0..MAX_RESULTS_PER_FRAME {
                let result = match self.result_rx.try_recv() {
                    Ok(result) => result,
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                };
                processed += 1;

                let is_current = matches!(
                    self.entries.get(&result.path),
                    Some(CacheEntry::Pending { id, revision })
                        if *id == result.id && *revision == result.revision
                );
                if !is_current {
                    continue;
                }

                let Some(rgba) = result.rgba.filter(|pixels| pixels.len() == ICON_BYTE_COUNT) else {
                    let last_used = self.tick();
                    self.entries.insert(
                        result.path,
                        CacheEntry::Failed {
                            revision: result.revision,
                            failed_at: Instant::now(),
                            last_used,
                        },
                    );
                    continue;
                };

                let image =
                    egui::ColorImage::from_rgba_unmultiplied([ICON_SIDE, ICON_SIDE], &rgba);
                let texture = context.load_texture(
                    format!("finder-icon:{}:{}", result.id, result.path),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                let last_used = self.tick();
                self.entries.insert(
                    result.path,
                    CacheEntry::Ready {
                        revision: result.revision,
                        texture,
                        last_used,
                    },
                );
            }

            self.mark_pending_failed_if_worker_stopped();
            self.evict_entries();
            if processed == MAX_RESULTS_PER_FRAME && !self.result_rx.is_empty() {
                context.request_repaint();
            }
        }

        pub fn texture_for(
            &mut self,
            path: &str,
            modified_at: Option<i64>,
            request_if_missing: bool,
        ) -> Option<egui::TextureId> {
            let key = absolute_path_key(path)?;
            let access_clock = self.tick();
            let should_request;

            if let Some(entry) = self.entries.get_mut(&key) {
                if entry.revision() != modified_at {
                    should_request = request_if_missing;
                } else {
                    match entry {
                        CacheEntry::Ready {
                            texture,
                            last_used,
                            ..
                        } => {
                            *last_used = access_clock;
                            return Some(texture.id());
                        }
                        CacheEntry::Pending { .. } => return None,
                        CacheEntry::Failed {
                            failed_at,
                            last_used,
                            ..
                        } => {
                            *last_used = access_clock;
                            should_request = request_if_missing
                                && failed_at.elapsed() >= FAILED_RETRY_DELAY;
                        }
                    }
                }
            } else {
                should_request = request_if_missing;
            }

            if should_request {
                self.enqueue(key, modified_at);
            }
            None
        }

        pub fn clear(&mut self) {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.entries.clear();
            while self.result_rx.try_recv().is_ok() {}
        }

        fn enqueue(&mut self, path: String, revision: Option<i64>) {
            let id = self.next_request_id;
            self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
            let request = IconRequest {
                id,
                path: path.clone(),
                revision,
                generation: self.generation.load(Ordering::Acquire),
            };

            match self.request_tx.try_send(request) {
                Ok(()) => {
                    self.entries
                        .insert(path, CacheEntry::Pending { id, revision });
                }
                Err(TrySendError::Full(_)) => {
                    // Leave the previous state intact and retry on a later frame.
                }
                Err(TrySendError::Disconnected(_)) => {
                    let last_used = self.tick();
                    self.entries.insert(
                        path,
                        CacheEntry::Failed {
                            revision,
                            failed_at: Instant::now(),
                            last_used,
                        },
                    );
                }
            }
        }

        fn tick(&mut self) -> u64 {
            self.access_clock = self.access_clock.wrapping_add(1);
            self.access_clock
        }

        fn evict_entries(&mut self) {
            loop {
                let ready_count = self
                    .entries
                    .values()
                    .filter(|entry| matches!(entry, CacheEntry::Ready { .. }))
                    .count();
                if ready_count <= MAX_READY_ICONS {
                    break;
                }

                let oldest = self
                    .entries
                    .iter()
                    .filter_map(|(path, entry)| match entry {
                        CacheEntry::Ready { last_used, .. } => Some((path.clone(), *last_used)),
                        _ => None,
                    })
                    .min_by_key(|(_, last_used)| *last_used)
                    .map(|(path, _)| path);
                let Some(oldest) = oldest else {
                    break;
                };
                self.entries.remove(&oldest);
            }

            while self.entries.len() > MAX_CACHE_ENTRIES {
                let oldest = self
                    .entries
                    .iter()
                    .filter_map(|(path, entry)| match entry {
                        CacheEntry::Ready { last_used, .. }
                        | CacheEntry::Failed { last_used, .. } => {
                            Some((path.clone(), *last_used))
                        }
                        CacheEntry::Pending { .. } => None,
                    })
                    .min_by_key(|(_, last_used)| *last_used)
                    .map(|(path, _)| path);
                let Some(oldest) = oldest else {
                    break;
                };
                self.entries.remove(&oldest);
            }
        }

        fn mark_pending_failed_if_worker_stopped(&mut self) {
            let worker_stopped = match self.worker.as_ref() {
                Some(worker) => worker.is_finished(),
                None => true,
            };
            if !worker_stopped {
                return;
            }

            let now = Instant::now();
            let last_used = self.tick();
            for entry in self.entries.values_mut() {
                if let CacheEntry::Pending { revision, .. } = entry {
                    let revision = *revision;
                    *entry = CacheEntry::Failed {
                        revision,
                        failed_at: now,
                        last_used,
                    };
                }
            }
        }
    }

    impl Drop for NativeIconCache {
        fn drop(&mut self) {
            let _ = self.stop_tx.try_send(());
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn absolute_path_key(path: &str) -> Option<String> {
        Path::new(path).is_absolute().then(|| path.to_owned())
    }

    fn worker_loop(
        request_rx: Receiver<IconRequest>,
        result_tx: Sender<IconResult>,
        stop_rx: Receiver<()>,
        repaint_context: egui::Context,
        generation: Arc<AtomicU64>,
    ) {
        loop {
            if stop_requested(&stop_rx) {
                break;
            }

            let request = match request_rx.recv_timeout(CHANNEL_WAIT) {
                Ok(request) => request,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if request.generation != generation.load(Ordering::Acquire) {
                continue;
            }
            let rgba = load_finder_icon(&request.path);
            let mut result = IconResult {
                id: request.id,
                path: request.path,
                revision: request.revision,
                rgba,
            };

            loop {
                if stop_requested(&stop_rx) {
                    return;
                }
                match result_tx.send_timeout(result, CHANNEL_WAIT) {
                    Ok(()) => {
                        repaint_context.request_repaint();
                        break;
                    }
                    Err(SendTimeoutError::Timeout(returned)) => result = returned,
                    Err(SendTimeoutError::Disconnected(_)) => return,
                }
            }
        }
    }

    fn stop_requested(stop_rx: &Receiver<()>) -> bool {
        !matches!(stop_rx.try_recv(), Err(TryRecvError::Empty))
    }

    fn load_finder_icon(path: &str) -> Option<Vec<u8>> {
        use image::imageops::FilterType;
        use objc2::rc::autoreleasepool;
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::NSString;

        let tiff = autoreleasepool(|_| {
            let workspace = NSWorkspace::sharedWorkspace();
            let path = NSString::from_str(path);
            let image = workspace.iconForFile(&path);
            image.TIFFRepresentation().map(|data| data.to_vec())
        })?;

        let image = image::load_from_memory_with_format(&tiff, image::ImageFormat::Tiff).ok()?;
        let rgba = image
            .resize_exact(
                ICON_SIDE as u32,
                ICON_SIDE as u32,
                FilterType::Lanczos3,
            )
            .to_rgba8()
            .into_raw();
        (rgba.len() == ICON_BYTE_COUNT).then_some(rgba)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::egui;

    pub struct NativeIconCache;

    impl NativeIconCache {
        pub fn new(_context: &egui::Context) -> Self {
            Self
        }

        pub fn drain(&mut self, _context: &egui::Context) {}

        pub fn texture_for(
            &mut self,
            _path: &str,
            _modified_at: Option<i64>,
            _request_if_missing: bool,
        ) -> Option<egui::TextureId> {
            None
        }

        pub fn clear(&mut self) {}
    }
}

pub use imp::NativeIconCache;
