use std::ffi::{CStr, OsStr, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::slice;

use anyhow::{Result, bail};
use crossbeam_channel::Sender;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2_core_foundation::{CFArray, CFString};
use objc2_core_services::{
    ConstFSEventStreamRef, FSEventStreamContext, FSEventStreamCreate,
    FSEventStreamEventFlags, FSEventStreamEventId, FSEventsGetCurrentEventId,
    FSEventStreamInvalidate, FSEventStreamRef, FSEventStreamRelease,
    FSEventStreamSetDispatchQueue, FSEventStreamStart, FSEventStreamStop,
    kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagNoDefer,
    kFSEventStreamCreateFlagWatchRoot, kFSEventStreamEventFlagEventIdsWrapped,
    kFSEventStreamEventFlagHistoryDone, kFSEventStreamEventFlagItemCreated,
    kFSEventStreamEventFlagItemIsDir, kFSEventStreamEventFlagItemRemoved,
    kFSEventStreamEventFlagItemRenamed, kFSEventStreamEventFlagKernelDropped,
    kFSEventStreamEventFlagMustScanSubDirs, kFSEventStreamEventFlagRootChanged,
    kFSEventStreamEventFlagUserDropped,
};

use super::{Command, FsChange, is_excluded_path};

type EventsCallback = Box<dyn FnMut(Vec<RawEvent>) + Send>;

#[derive(Debug)]
struct RawEvent {
    path: PathBuf,
    flags: FSEventStreamEventFlags,
    id: FSEventStreamEventId,
}

pub(super) struct NativeWatcher {
    stream: FSEventStreamRef,
    _queue: DispatchRetained<DispatchQueue>,
}

impl NativeWatcher {
    pub(super) fn start(
        root: &Path,
        since_event_id: FSEventStreamEventId,
        excluded_roots: &[PathBuf],
        sender: Sender<Command>,
    ) -> Result<Self> {
        let root_text = root.to_string_lossy().into_owned();
        let excluded_roots = excluded_roots.to_vec();
        let callback: EventsCallback = Box::new(move |events| {
            let mut changes = Vec::with_capacity(events.len());
            let mut last_event_id = since_event_id;
            let mut rescan_required = false;

            for event in events {
                last_event_id = last_event_id.max(event.id);
                if has_flag(event.flags, kFSEventStreamEventFlagHistoryDone) {
                    continue;
                }
                if has_flag(event.flags, kFSEventStreamEventFlagEventIdsWrapped)
                    || has_flag(event.flags, kFSEventStreamEventFlagRootChanged)
                    || has_flag(event.flags, kFSEventStreamEventFlagUserDropped)
                    || has_flag(event.flags, kFSEventStreamEventFlagKernelDropped)
                {
                    rescan_required = true;
                    continue;
                }
                if is_excluded_path(&event.path, &excluded_roots) {
                    continue;
                }

                let is_directory = has_flag(event.flags, kFSEventStreamEventFlagItemIsDir);
                let structural_change = has_flag(event.flags, kFSEventStreamEventFlagItemCreated)
                    || has_flag(event.flags, kFSEventStreamEventFlagItemRemoved)
                    || has_flag(event.flags, kFSEventStreamEventFlagItemRenamed);
                changes.push(FsChange {
                    path: event.path,
                    rescan_tree: has_flag(
                        event.flags,
                        kFSEventStreamEventFlagMustScanSubDirs,
                    ) || (is_directory && structural_change),
                });
            }

            let command = if rescan_required {
                Command::RescanRequired { last_event_id }
            } else if changes.is_empty() {
                Command::WatcherAdvanced { last_event_id }
            } else {
                Command::FileEvents {
                    changes,
                    last_event_id,
                }
            };
            let _ = sender.send(command);
        });

        unsafe extern "C-unwind" fn release_callback(info: *const c_void) {
            let _callback: Box<EventsCallback> = unsafe { Box::from_raw(info as *mut _) };
        }

        unsafe extern "C-unwind" fn raw_callback(
            _stream: ConstFSEventStreamRef,
            callback_info: *mut c_void,
            event_count: usize,
            event_paths: NonNull<c_void>,
            event_flags: NonNull<FSEventStreamEventFlags>,
            event_ids: NonNull<FSEventStreamEventId>,
        ) {
            let paths = unsafe {
                slice::from_raw_parts(event_paths.as_ptr().cast::<*const i8>(), event_count)
            };
            let flags = unsafe { slice::from_raw_parts(event_flags.as_ptr(), event_count) };
            let ids = unsafe { slice::from_raw_parts(event_ids.as_ptr(), event_count) };
            let events = paths
                .iter()
                .zip(flags)
                .zip(ids)
                .map(|((&path, &flags), &id)| RawEvent {
                    path: PathBuf::from(OsStr::from_bytes(unsafe {
                        CStr::from_ptr(path).to_bytes()
                    })),
                    flags,
                    id,
                })
                .collect();
            let callback = unsafe { (callback_info as *mut EventsCallback).as_mut() }
                .expect("FSEvents callback context must be present");
            callback(events);
        }

        let paths = [CFString::from_str(&root_text)];
        let paths = CFArray::from_retained_objects(&paths);
        let mut context = FSEventStreamContext {
            version: 0,
            info: Box::leak(Box::new(callback)) as *mut _ as *mut c_void,
            retain: None,
            release: Some(release_callback),
            copyDescription: None,
        };
        let stream = unsafe {
            FSEventStreamCreate(
                None,
                Some(raw_callback),
                &mut context,
                paths.as_opaque(),
                since_event_id,
                0.15,
                kFSEventStreamCreateFlagNoDefer
                    | kFSEventStreamCreateFlagFileEvents
                    | kFSEventStreamCreateFlagWatchRoot,
            )
        };
        let queue = DispatchQueue::new("rust-everything-fsevents", DispatchQueueAttr::SERIAL);
        unsafe { FSEventStreamSetDispatchQueue(stream, Some(&queue)) };
        if !unsafe { FSEventStreamStart(stream) } {
            unsafe {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
            }
            bail!("FSEventStreamStart returned false");
        }
        Ok(Self {
            stream,
            _queue: queue,
        })
    }
}

impl Drop for NativeWatcher {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
        }
    }
}

pub(super) fn current_event_id() -> FSEventStreamEventId {
    unsafe { FSEventsGetCurrentEventId() }
}

fn has_flag(flags: FSEventStreamEventFlags, flag: FSEventStreamEventFlags) -> bool {
    flags & flag != 0
}
