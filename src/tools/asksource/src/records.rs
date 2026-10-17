use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub mod field {
    pub const NODES: &str = "nodes";
    pub const DEFINITIONS: &str = "definitions";

    const _: () = assert!(NODES.len() != DEFINITIONS.len());
}

pub mod buffer {
    // Bound hashing memory to one 64 KiB I/O chunk, independently of stream size.
    pub const BYTES: usize = 64 * 1024;
}

fn path(metadata: &Path, field: &str) -> Result<PathBuf, String> {
    if ![field::NODES, field::DEFINITIONS].contains(&field) {
        return Err("unknown compiler record field".into());
    }
    Ok(metadata.with_extension(format!("{field}.jsonl")))
}

fn fingerprint(path: &Path) -> Result<(String, u64), String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hash = Sha256::new();
    let mut records = 0;
    let mut buffer = [0; buffer::BYTES];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 { break; }
        let bytes = &buffer[..count];
        hash.update(bytes);
        records += bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    }
    Ok((hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect(), records))
}

pub(super) fn validate(metadata: &Value, destination: &Path) -> Result<(), String> {
    for field in [field::NODES, field::DEFINITIONS] {
        let descriptor = &metadata[field];
        let expected = path(destination, field)?;
        if descriptor.as_object().is_none_or(|fields| fields.len() != 3)
            || descriptor["path"] != json!(expected) {
            return Err(format!("missing or malformed compiler {field} stream"));
        }
        let (hash, count) = fingerprint(&expected)?;
        if descriptor["sha256"] != hash || descriptor["count"].as_u64() != Some(count) {
            return Err(format!("compiler {field} stream changed or is truncated"));
        }
    }
    Ok(())
}

pub(super) struct Writer {
    writer: BufWriter<File>,
    destination: PathBuf,
    partial: PathBuf,
    count: u64,
    error: Option<String>,
}

impl Writer {
    pub(super) fn new(metadata: &Path, field: &str) -> Result<Self, String> {
        let destination = path(metadata, field)?;
        let partial = destination.with_extension("partial");
        let file = File::create(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
        Ok(Self { writer: BufWriter::new(file), destination, partial, count: 0, error: None })
    }

    pub(super) fn push(&mut self, record: Value) {
        if self.error.is_some() { return; }
        let result = serde_json::to_writer(&mut self.writer, &record)
            .map_err(|e| e.to_string())
            .and_then(|()| self.writer.write_all(b"\n").map_err(|e| e.to_string()));
        match result {
            Ok(()) => self.count += 1,
            Err(error) => self.error = Some(error),
        }
    }

    pub(super) fn check(&self) -> Result<(), String> {
        self.error.as_ref().map_or(Ok(()), |error| Err(error.clone()))
    }

    pub(super) fn finish(mut self) -> Result<Value, String> {
        self.check()?;
        self.writer.flush().map_err(|e| e.to_string())?;
        drop(self.writer);
        let (hash, count) = fingerprint(&self.partial)?;
        if count != self.count { return Err("compiler record count changed while writing".into()); }
        fs::rename(&self.partial, &self.destination).map_err(|e| e.to_string())?;
        Ok(json!({"path": self.destination, "sha256": hash, "count": count}))
    }
}
