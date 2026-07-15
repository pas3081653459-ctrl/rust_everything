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

