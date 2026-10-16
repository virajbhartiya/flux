//! The job as the server hands it out (`POST /api/v1/ci/claim`). Mirrors
//! `flux_server::ci::{ClaimedJob, JobSpec}`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimedJob {
    pub id: i64,
    pub run_id: i64,
    pub repo: String,
    pub clone_url: String,
    pub changeset: String,
    #[serde(default)]
    pub git_sha: String,
    /// A read-only token for the repository, valid while the job runs.
    pub token: String,
    pub spec: JobSpec,
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobSpec {
    #[serde(default)]
    pub workflow: String,
    #[serde(default)]
    pub job: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub r#if: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub matrix: BTreeMap<String, Value>,
    #[serde(default = "default_timeout")]
    pub timeout_minutes: u64,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub steps: Vec<StepSpec>,
    #[serde(default)]
    pub continue_on_error: bool,
    /// `github.*` context values.
    #[serde(default)]
    pub context: BTreeMap<String, Value>,
}

fn default_timeout() -> u64 {
    360
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepSpec {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub with: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub r#if: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub timeout_minutes: Option<u64>,
}
