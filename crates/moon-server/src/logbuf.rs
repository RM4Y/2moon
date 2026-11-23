//! Keeps the most recent log lines in memory (for the admin panel) while
//! still writing everything to stderr, and to a log file if one is given.

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

const MAX_LINES: usize = 500;

fn buffer() -> &'static Mutex<VecDeque<String>> {
    static B: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)))
}

/// Last `n` log lines (oldest first), without ANSI colour codes.
pub fn recent(n: usize) -> Vec<String> {
    let b = buffer().lock().unwrap();
    b.iter().skip(b.len().saturating_sub(n)).cloned().collect()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A log file shared by every writer.
pub type LogFile = Arc<Mutex<std::fs::File>>;

/// Open `path` for the log, keeping the previous run's log as `<name>.old`.
pub fn open_log_file(path: &Path) -> std::io::Result<LogFile> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    if path.exists() {
        let _ = std::fs::rename(path, path.with_extension("old.log"));
    }
    Ok(Arc::new(Mutex::new(std::fs::File::create(path)?)))
}

pub struct TeeWriter(pub Option<LogFile>);

impl Write for TeeWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(data);
        if let Some(f) = &self.0 {
            let _ = f.lock().unwrap().write_all(data);
        }
        let text = strip_ansi(&String::from_utf8_lossy(data));
        let mut b = buffer().lock().unwrap();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if b.len() == MAX_LINES {
                b.pop_front();
            }
            b.push_back(line.to_string());
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_colours_and_keeps_tail() {
        use std::io::Write;
        let mut w = super::TeeWriter(None);
        w.write_all(b"\x1b[32m INFO\x1b[0m hello\n").unwrap();
        assert_eq!(super::recent(1), vec![" INFO hello".to_string()]);
    }
}
