use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_LOG_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Driver {
    Node,
    Python,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub driver: Driver,
    pub script: String,
    #[serde(default = "default_schema")]
    pub input_schema: Value,
    #[serde(default)]
    pub required_tags: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}
fn default_schema() -> Value {
    serde_json::json!({"type":"object"})
}
fn default_timeout() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputFile {
    pub name: String,
    pub artifact_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadFile {
    pub name: String,
    pub url: String,
    pub size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub run_id: String,
    pub attempt_id: String,
    pub definition: DefinitionSpec,
    pub input: Value,
    #[serde(default)]
    pub files: Vec<DownloadFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        session_id: String,
        heartbeat_seconds: u64,
        heartbeat_timeout_seconds: u64,
        version: String,
    },
    Assign {
        assignment: Assignment,
    },
    Cancel {
        attempt_id: String,
    },
    FinishedAck {
        attempt_id: String,
        accepted: bool,
    },
    UploadUrl {
        request_id: String,
        artifact_id: String,
        url: String,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    Hello {
        worker_id: String,
        name: String,
        version: String,
        protocol_version: u32,
        tags: Vec<String>,
        drivers: Vec<Driver>,
    },
    Heartbeat {},
    Started {
        attempt_id: String,
    },
    Log {
        attempt_id: String,
        stream: String,
        text: String,
    },
    LogTruncated {
        attempt_id: String,
    },
    RequestUpload {
        request_id: String,
        attempt_id: String,
        name: String,
        size: u64,
    },
    Finished {
        attempt_id: String,
        outcome: Outcome,
        exit_code: Option<i32>,
        result: Option<Value>,
        error: Option<String>,
        artifact_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Canceled,
}

pub fn tags_match(required: &[String], available: &[String]) -> bool {
    let available: BTreeSet<_> = available.iter().collect();
    required.iter().all(|tag| available.contains(tag))
}

pub fn safe_file_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    !name.is_empty()
        && name.len() <= 180
        && name != "."
        && name != ".."
        && !name.ends_with('.')
        && !reserved
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_all_tags_not_substrings() {
        assert!(tags_match(
            &["gpu=amd".into()],
            &["gpu=amd".into(), "os=linux".into()]
        ));
        assert!(!tags_match(
            &["gpu=amd".into(), "os=windows".into()],
            &["gpu=amd".into()]
        ));
        assert!(!tags_match(&["gpu=amd".into()], &["gpu=amd-pro".into()]));
    }
    #[test]
    fn rejects_unsafe_paths() {
        for name in [
            "../secret",
            "a/b",
            "a\\b",
            ".",
            "..",
            "x:y",
            "",
            "NUL",
            "con.txt",
            "COM1",
            "file.",
        ] {
            assert!(!safe_file_name(name));
        }
        assert!(safe_file_name("result-01.json"));
    }
}
