use std::path::Path;
use std::process::Command;

pub fn open(path: &Path) -> std::io::Result<()> {
    Command::new("open").arg(path).spawn().map(|_| ())
}

pub fn reveal_in_finder(path: &Path) -> std::io::Result<()> {
    Command::new("open").arg("-R").arg(path).spawn().map(|_| ())
}

pub fn copy_to_clipboard(context: &eframe::egui::Context, path: &Path) {
    context.copy_text(path.to_string_lossy().into_owned());
}

#[cfg(target_os = "macos")]
pub fn begin_file_drag(path: &Path) -> Result<(), String> {
    file_drag::begin(path)
}

#[cfg(not(target_os = "macos"))]
pub fn begin_file_drag(_path: &Path) -> Result<(), String> {
    Err("Dragging files outside the app is only supported on macOS".to_owned())
}

#[cfg(target_os = "macos")]
mod file_drag {
    use std::cell::RefCell;
    use std::path::Path;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::{
        AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class,
        msg_send,
    };
    use objc2_app_kit::{
        NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
        NSDraggingSource, NSPasteboardWriting, NSWorkspace,
    };
    use objc2_foundation::{
        NSArray, NSFileManager, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
        NSURL,
    };

    #[derive(Default)]
    struct FileDragSourceIvars {
        dragged_url: RefCell<Option<Retained<NSURL>>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = FileDragSourceIvars]
        struct FileDragSource;

        unsafe impl NSObjectProtocol for FileDragSource {}

        unsafe impl NSDraggingSource for FileDragSource {
            #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
            fn draggingSession_sourceOperationMaskForDraggingContext(
                &self,
                _session: &NSDraggingSession,
                _context: NSDraggingContext,
            ) -> NSDragOperation {
                NSDragOperation::Copy | NSDragOperation::Delete
            }

            #[unsafe(method(draggingSession:endedAtPoint:operation:))]
            fn draggingSession_endedAtPoint_operation(
                &self,
                _session: &NSDraggingSession,
                _screen_point: NSPoint,
                operation: NSDragOperation,
            ) {
                let dragged_url = self.ivars().dragged_url.borrow_mut().take();
                if operation.contains(NSDragOperation::Delete) {
                    if let Some(url) = dragged_url {
                        let _ = NSFileManager::defaultManager()
                            .trashItemAtURL_resultingItemURL_error(&url, None);
                    }
                }
            }
        }
    );

    impl FileDragSource {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(FileDragSourceIvars::default());
            unsafe { msg_send![super(this), init] }
        }
    }

    thread_local! {
        static DRAG_SOURCE: Retained<FileDragSource> = {
            let mtm = MainThreadMarker::new().expect("drag source must be created on the main thread");
            FileDragSource::new(mtm)
        };
    }

    pub(super) fn begin(path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Err(format!("Cannot drag a missing item: {}", path.display()));
        }

        let mtm = MainThreadMarker::new()
            .ok_or_else(|| "File dragging must start on the macOS main thread".to_owned())?;
        let application = NSApplication::sharedApplication(mtm);
        let event = application
            .currentEvent()
            .ok_or_else(|| "No mouse event is available to start the drag".to_owned())?;
        let window = application
            .keyWindow()
            .or_else(|| application.mainWindow())
            .ok_or_else(|| "No application window is available to start the drag".to_owned())?;
        let view = window
            .contentView()
            .ok_or_else(|| "The application window has no content view".to_owned())?;
        let url = if path.is_dir() {
            NSURL::from_directory_path(path)
        } else {
            NSURL::from_file_path(path)
        }
        .ok_or_else(|| format!("Unable to create a file URL for {}", path.display()))?;

        let writer = ProtocolObject::<dyn NSPasteboardWriting>::from_ref(&*url);
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
        let full_path = NSString::from_str(path.to_string_lossy().as_ref());
        let icon = NSWorkspace::sharedWorkspace().iconForFile(&full_path);
        let icon_size = NSSize::new(48.0, 48.0);
        icon.setSize(icon_size);
        let location = view.convertPoint_fromView(event.locationInWindow(), None);
        let frame = NSRect::new(
            NSPoint::new(location.x - icon_size.width / 2.0, location.y - icon_size.height / 2.0),
            icon_size,
        );
        unsafe {
            let contents: &AnyObject = icon.as_super().as_super();
            item.setDraggingFrame_contents(frame, Some(contents));
        }
        let items = NSArray::from_retained_slice(&[item]);

        DRAG_SOURCE.with(|source| {
            source
                .ivars()
                .dragged_url
                .replace(Some(Retained::clone(&url)));
            let source = ProtocolObject::<dyn NSDraggingSource>::from_ref::<FileDragSource>(
                &**source,
            );
            let _session = view.beginDraggingSessionWithItems_event_source(&items, &event, source);
        });
        Ok(())
    }
}
