//! JSON log lines, written by `tracing-subscriber`'s `fmt` layer with a formatter of our own, so
//! no JSON feature (and no new crate) is needed. A [`RotatingFile`] bounds the log on disk: once
//! the file would pass `max_bytes`, it is renamed to `.1`, older ones shift up, and at most
//! `keep` old files are kept.
//!
//! A line is one JSON object with `ts_ms`, `level`, `target`, `message` and the event's fields,
//! keys in sorted order. Callers never log keys, seeds, signatures of unsent transactions or RPC
//! URLs.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Rotate at 10 MB.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
/// Keep 5 old files.
pub const KEEP_LOGS: u32 = 5;

/// One JSON object per event.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonLines;

struct Fields(Map<String, Value>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_owned(), Value::from(format!("{value:?}")));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The JSON object of an event's fields, with the timestamp, level and target, which no field
/// can overwrite.
#[must_use]
pub fn line(ts_ms: u64, level: &str, target: &str, fields: Map<String, Value>) -> String {
    let mut out = Map::new();
    out.insert("ts_ms".into(), Value::from(ts_ms));
    out.insert("level".into(), Value::from(level));
    out.insert("target".into(), Value::from(target));
    for (k, v) in fields {
        // A field may not overwrite the three above.
        if !out.contains_key(&k) {
            out.insert(k, v);
        }
    }
    Value::Object(out).to_string()
}

impl<S, N> FormatEvent<S, N> for JsonLines
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let mut fields = Fields(Map::new());
        event.record(&mut fields);
        let meta = event.metadata();
        writeln!(
            writer,
            "{}",
            line(now_ms(), meta.level().as_str(), meta.target(), fields.0)
        )
    }
}

/// A log file rotated by size. Wrap it in a `Mutex` to use it as a `MakeWriter`.
#[derive(Debug)]
pub struct RotatingFile {
    path: PathBuf,
    file: File,
    size: u64,
    max_bytes: u64,
    keep: u32,
}

fn rotated(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

impl RotatingFile {
    /// Opens `path` for appending.
    ///
    /// # Errors
    ///
    /// The I/O error.
    pub fn open(path: &Path, max_bytes: u64, keep: u32) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_owned(),
            file,
            size,
            max_bytes,
            keep,
        })
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        if self.keep == 0 {
            std::fs::remove_file(&self.path)?;
        } else {
            let mut n = self.keep;
            while n > 1 {
                let older = n.saturating_sub(1);
                let from = rotated(&self.path, older);
                if from.exists() {
                    std::fs::rename(&from, rotated(&self.path, n))?;
                }
                n = older;
            }
            std::fs::rename(&self.path, rotated(&self.path, 1))?;
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.size > 0 && self.size.saturating_add(len) > self.max_bytes {
            self.rotate()?;
        }
        let written = self.file.write(buf)?;
        self.size = self
            .size
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Installs the JSON subscriber: to `file` with rotation when given, else to stdout. `RUST_LOG`
/// is not read; the level is INFO.
///
/// # Errors
///
/// The I/O error of opening the file.
pub fn init(file: Option<&Path>) -> std::io::Result<()> {
    let builder = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .event_format(JsonLines);
    match file {
        Some(path) => {
            let writer = RotatingFile::open(path, MAX_LOG_BYTES, KEEP_LOGS)?;
            builder.with_writer(std::sync::Mutex::new(writer)).init();
        }
        None => builder.init(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::disallowed_macros,
        clippy::indexing_slicing,
        clippy::unwrap_used
    )]

    use super::*;

    #[test]
    fn a_line_is_one_json_object() {
        let mut fields = Map::new();
        fields.insert("message".into(), Value::from("quoted"));
        fields.insert("nonce".into(), Value::from(7u64));
        fields.insert("level".into(), Value::from("spoof"));
        let l = line(5, "INFO", "demo", fields);
        let v: Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v["ts_ms"], 5);
        assert_eq!(v["level"], "INFO", "a field cannot overwrite the level");
        assert_eq!(v["message"], "quoted");
        assert_eq!(v["nonce"], 7);
        assert!(!l.contains('\n'));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("quotebook-demo-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("maker.jsonl")
    }

    #[test]
    fn the_file_rotates_by_size_and_keeps_a_bounded_number() {
        let path = scratch("rotate");
        let mut f = RotatingFile::open(&path, 10, 2).unwrap();
        for i in 0..7u8 {
            f.write_all(&[b'0' + i; 6]).unwrap();
        }
        f.flush().unwrap();
        // Each 6-byte write after the first in a file passes 10 bytes, so every write starts a
        // new file; the newest is current, then `.1` and `.2`, and nothing older survives.
        assert_eq!(std::fs::read(&path).unwrap(), b"666666");
        assert_eq!(std::fs::read(rotated(&path, 1)).unwrap(), b"555555");
        assert_eq!(std::fs::read(rotated(&path, 2)).unwrap(), b"444444");
        assert!(!rotated(&path, 3).exists());
    }

    #[test]
    fn reopening_counts_what_is_already_there() {
        let path = scratch("reopen");
        std::fs::write(&path, b"12345678").unwrap();
        let mut f = RotatingFile::open(&path, 10, 1).unwrap();
        f.write_all(b"abc").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert_eq!(std::fs::read(rotated(&path, 1)).unwrap(), b"12345678");
    }
}
