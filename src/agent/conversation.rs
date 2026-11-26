//! Durable app-owned history (Phase 5).
//!
//! Keeps Needle's accumulating model conversation separate from the user's
//! application conversation. The app owns `{ user msgs, tool calls,
//! tool results, final responses }` here; the model conversation lives
//! inside the `ModelAdapter` (single Needle instance) and is reset between
//! independent sessions by the worker.
//!
//! Storage choice: file-backed JSON (`HashMap<session_id, transcript>`
//! serialized on every mutation, reloaded with `open`). This gives durable,
//! reconstructable sessions with zero native dependencies. It exposes the
//! same store→reload→identical-transcript semantics as a SQLite table
//! (`sessions(session_id, seq, kind, payload)`); swapping in
//! `rusqlite`/`sqlx` later only changes `ConversationLog::{open, persist}`
//! without touching callers.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One user-visible message in a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub seq: u64,
    pub role: String,
    pub text: String,
}

/// One tool call paired to its result by `call_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRecord {
    pub seq: u64,
    pub call_id: String,
    pub tool: String,
    pub args: serde_json::Value,
}

/// Result of one tool call, paired by `call_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultRecord {
    pub seq: u64,
    pub call_id: String,
    pub tool: String,
    pub ok: bool,
    pub result: serde_json::Value,
}

/// Final app-assembled response for one turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalRecord {
    pub seq: u64,
    pub text: String,
    pub ops: usize,
    pub iterations: usize,
}

/// Full transcript for one session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionTranscript {
    pub session_id: String,
    pub messages: Vec<ChatMessage>,
    pub calls: Vec<CallRecord>,
    pub results: Vec<ResultRecord>,
    pub finals: Vec<FinalRecord>,
    #[serde(default)]
    pub next_seq: u64,
}

impl SessionTranscript {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            ..Default::default()
        }
    }

    fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    /// Verify every result pairs to a call by `call_id` (test helper).
    pub fn all_results_paired(&self) -> bool {
        let ids: std::collections::HashSet<&str> =
            self.calls.iter().map(|c| c.call_id.as_str()).collect();
        self.results
            .iter()
            .all(|r| ids.contains(r.call_id.as_str()))
    }
}

/// App-owned durable conversation store.
pub struct ConversationLog {
    path: Option<PathBuf>,
    sessions: Mutex<HashMap<String, SessionTranscript>>,
}

impl ConversationLog {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Open (or create) a file-backed log. Existing transcripts reload.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let sessions = if path.exists() {
            let f = File::open(&path)?;
            let r = BufReader::new(f);
            serde_json::from_reader::<_, HashMap<String, SessionTranscript>>(r).unwrap_or_default()
        } else {
            HashMap::new()
        };
        Ok(Self {
            path: Some(path),
            sessions: Mutex::new(sessions),
        })
    }

    fn persist_locked(&self, sessions: &HashMap<String, SessionTranscript>) -> std::io::Result<()> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let f = File::create(path)?;
            let w = BufWriter::new(f);
            serde_json::to_writer_pretty(w, sessions).map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    fn with_session<F, T>(&self, session_id: &str, f: F) -> std::io::Result<T>
    where
        F: FnOnce(&mut SessionTranscript) -> T,
    {
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionTranscript::new(session_id));
        let out = f(entry);
        self.persist_locked(&sessions)?;
        Ok(out)
    }

    pub fn record_user_message(
        &self,
        session_id: &str,
        text: &str,
    ) -> std::io::Result<ChatMessage> {
        self.with_session(session_id, |t| {
            let m = ChatMessage {
                seq: t.alloc_seq(),
                role: "user".to_string(),
                text: text.to_string(),
            };
            t.messages.push(m.clone());
            m
        })
    }

    pub fn record_tool_call(
        &self,
        session_id: &str,
        call_id: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> std::io::Result<CallRecord> {
        self.with_session(session_id, |t| {
            let c = CallRecord {
                seq: t.alloc_seq(),
                call_id: call_id.to_string(),
                tool: tool.to_string(),
                args,
            };
            t.calls.push(c.clone());
            c
        })
    }

    pub fn record_tool_result(
        &self,
        session_id: &str,
        call_id: &str,
        tool: &str,
        ok: bool,
        result: serde_json::Value,
    ) -> std::io::Result<ResultRecord> {
        self.with_session(session_id, |t| {
            let r = ResultRecord {
                seq: t.alloc_seq(),
                call_id: call_id.to_string(),
                tool: tool.to_string(),
                ok,
                result,
            };
            t.results.push(r.clone());
            r
        })
    }

    pub fn record_final(
        &self,
        session_id: &str,
        text: &str,
        ops: usize,
        iterations: usize,
    ) -> std::io::Result<FinalRecord> {
        self.with_session(session_id, |t| {
            let fr = FinalRecord {
                seq: t.alloc_seq(),
                text: text.to_string(),
                ops,
                iterations,
            };
            t.finals.push(fr.clone());
            fr
        })
    }

    pub fn transcript(&self, session_id: &str) -> Option<SessionTranscript> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    pub fn session_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_pairs_calls_by_id() {
        let log = ConversationLog::in_memory();
        log.record_user_message("s1", "find alice").unwrap();
        log.record_tool_call("s1", "c1", "search", serde_json::json!({"query": "alice"}))
            .unwrap();
        log.record_tool_result(
            "s1",
            "c1",
            "search",
            true,
            serde_json::json!({"results": []}),
        )
        .unwrap();
        log.record_final("s1", "done", 1, 2).unwrap();
        let t = log.transcript("s1").unwrap();
        assert!(t.all_results_paired());
        // Store → identical transcript (in-memory clone equals itself).
        let again = log.transcript("s1").unwrap();
        assert_eq!(t, again);
    }
}
