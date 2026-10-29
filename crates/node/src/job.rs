//! Jobs as `GET /p1/jobs` returns them.

use std::fmt;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Delegate,
    Undelegate,
    /// A kind this version does not know: declined, so the order goes to another shop.
    #[serde(other)]
    Unknown,
}

impl JobKind {
    /// Part of the job fingerprint stored next to signed transactions: never change these.
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Delegate => "delegate",
            JobKind::Undelegate => "undelegate",
            JobKind::Unknown => "unknown",
        }
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Waiting for a taker.
    Pending,
    /// Taken by this shop, not finished yet.
    Taken,
    /// `done`, `declined`, `expired` or anything newer: nothing left to do.
    #[serde(other)]
    Closed,
}

impl JobStatus {
    pub fn is_open(self) -> bool {
        matches!(self, JobStatus::Pending | JobStatus::Taken)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: JobKind,
    pub from_address: Option<String>,
    pub target_address: String,
    pub energy: i64,
    pub balance_sun: Option<i64>,
    pub duration_seconds: Option<i32>,
    pub status: JobStatus,
}

#[derive(Debug, Deserialize)]
pub struct JobList {
    /// Parsed one by one: a job this agent cannot read must not hold up the others.
    pub data: Vec<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kinds_and_statuses_still_parse() {
        let j: Job = serde_json::from_value(serde_json::json!({
            "id": "j", "kind": "freeze", "target_address": "T", "energy": 1, "status": "expired",
        }))
        .unwrap();
        assert_eq!(j.kind, JobKind::Unknown);
        assert_eq!(j.status, JobStatus::Closed);
        assert!(!j.status.is_open());
        let j: Job = serde_json::from_value(serde_json::json!({
            "id": "j", "kind": "undelegate", "target_address": "T", "energy": 1, "status": "taken",
        }))
        .unwrap();
        assert_eq!(j.kind.as_str(), "undelegate");
        assert!(j.status.is_open());
    }
}
