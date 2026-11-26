//! Append-only audit log (Phase 2): one record per mutation attempt.
//! Backed by an in-memory vec plus an optional JSON-lines file so the log
//! survives restarts.

use crate::rdf::{GraphName, TripleDelta};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How a mutation attempt ended. Only `Committed` means the store changed
/// because of this request. Model success messages never count as proof —
/// only this field does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitStatus {
    Committed,
    DryRun,
    Denied,
    ValidationFailed,
}

/// One audit record. All fields are always present (snapshot-tested).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub request_id: String,
    pub principal: String,
    pub target_uri: String,
    pub graph: GraphName,
    pub op: String,
    pub delta_added: usize,
    pub delta_removed: usize,
    pub delta: TripleDelta,
    pub validation_conforms: bool,
    pub validation_violations: usize,
    pub commit_status: CommitStatus,
    pub note: String,
}

/// Append-only log. `path` enables JSONL persistence; `None` keeps it
/// in-memory only (unit tests).
pub struct AuditLog {
    path: Option<PathBuf>,
    records: Mutex<Vec<AuditRecord>>,
}

impl AuditLog {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            records: Mutex::new(Vec::new()),
        }
    }

    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut records = Vec::new();
        if path.exists() {
            let f = File::open(&path)?;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(rec) = serde_json::from_str::<AuditRecord>(&line) {
                    records.push(rec);
                }
            }
        }
        Ok(Self {
            path: Some(path),
            records: Mutex::new(records),
        })
    }

    /// Append one record (fsync-equivalent: flush after each write).
    pub fn append(&self, record: AuditRecord) -> std::io::Result<()> {
        if let Some(path) = &self.path {
            let mut f = OpenOptions::new().create(true).append(true).open(path)?;
            serde_json::to_writer(&mut f, &record).map_err(std::io::Error::other)?;
            f.write_all(b"\n")?;
            f.flush()?;
        }
        self.records.lock().unwrap().push(record);
        Ok(())
    }

    pub fn list(&self) -> Vec<AuditRecord> {
        self.records.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.records.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_append_and_list() {
        let log = AuditLog::in_memory();
        assert!(log.is_empty());
        log.append(AuditRecord {
            request_id: "r1".into(),
            principal: "alice".into(),
            target_uri: "http://example.com/A".into(),
            graph: GraphName::tenant("acme").unwrap(),
            op: "create".into(),
            delta_added: 1,
            delta_removed: 0,
            delta: TripleDelta::default(),
            validation_conforms: true,
            validation_violations: 0,
            commit_status: CommitStatus::Committed,
            note: String::new(),
        })
        .unwrap();
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn file_log_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        {
            let log = AuditLog::open(&p).unwrap();
            log.append(AuditRecord {
                request_id: "r1".into(),
                principal: "alice".into(),
                target_uri: "http://example.com/A".into(),
                graph: GraphName::tenant("acme").unwrap(),
                op: "create".into(),
                delta_added: 2,
                delta_removed: 0,
                delta: TripleDelta::default(),
                validation_conforms: true,
                validation_violations: 0,
                commit_status: CommitStatus::Committed,
                note: String::new(),
            })
            .unwrap();
        }
        let reopened = AuditLog::open(&p).unwrap();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.list()[0].request_id, "r1");
    }
}
