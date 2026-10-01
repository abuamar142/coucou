// Small append-only log at ~/.local/share/coucou/coucou.log — nothing leaves
// the machine.

use std::io::Write;

use crate::settings;

pub fn line(message: impl AsRef<str>) {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let dir = settings::local_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("coucou.log");
    // Keep it from growing forever: start fresh past ~1 MB.
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{stamp} {}", message.as_ref());
    }
}
