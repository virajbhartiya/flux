//! The runner protocol over HTTP (`/api/v1/ci/*`).

use std::time::Duration;

use serde_json::{Value, json};

use crate::spec::ClaimedJob;

/// A failed request: `status` is the HTTP status, if there was a response.
#[derive(Debug)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "HTTP {s}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// Network trouble, timeouts, rate limits and server errors are worth
    /// retrying; other client errors are not.
    pub fn retryable(&self) -> bool {
        match self.status {
            None => true,
            Some(s) => s >= 500 || s == 408 || s == 429,
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError { status: e.status().map(|s| s.as_u16()), message: e.to_string() }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(server: &str, token: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("flux-runner/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Client { http, base: server.trim_end_matches('/').to_string(), token: token.to_string() })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("message").or_else(|| v.get("error")).and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| text.trim().chars().take(500).collect());
        Err(ApiError { status: Some(status.as_u16()), message })
    }

    /// Long-poll for a job (`wait` seconds at most on the server).
    pub async fn claim(&self, labels: &[String], wait: u64) -> Result<Option<ClaimedJob>, ApiError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/ci/claim", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(wait + 30))
            .json(&json!({ "labels": labels, "wait": wait }))
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let body = resp.bytes().await?;
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|e| ApiError { status: None, message: format!("bad job from server: {e}") })
    }

    /// Upload a log chunk (or an empty heartbeat). Returns whether the job
    /// was cancelled.
    pub async fn log(&self, job: i64, seq: i64, data: Vec<u8>) -> Result<bool, ApiError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/ci/jobs/{job}/log?seq={seq}", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(60))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .send()
            .await?;
        let v: Value = Self::check(resp).await?.json().await?;
        Ok(v.get("cancelled").and_then(Value::as_bool).unwrap_or(false))
    }

    pub async fn finish(&self, job: i64, conclusion: &str, exit_code: Option<i64>) -> Result<(), ApiError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/ci/jobs/{job}/finish", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(60))
            .json(&json!({ "conclusion": conclusion, "exit_code": exit_code }))
            .send()
            .await?;
        Self::check(resp).await.map(|_| ())
    }

    /// The jobs of a run, read with the job's repository token.
    pub async fn run_jobs(&self, repo: &str, run: i64, token: &str) -> Result<Vec<Value>, ApiError> {
        let resp = self
            .http
            .get(format!("{}/api/v1/repos/{repo}/runs/{run}", self.base))
            .bearer_auth(token)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let v: Value = Self::check(resp).await?.json().await?;
        Ok(v.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default())
    }
}
