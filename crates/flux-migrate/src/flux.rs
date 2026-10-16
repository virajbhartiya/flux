//! The flux REST calls a migration makes (`/api/v1`).

use std::fmt;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};

use crate::convert::{BranchRule, ImportBatch};

pub struct FluxApi {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl fmt::Debug for FluxApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FluxApi").field("base", &self.base).finish_non_exhaustive()
    }
}

/// Percent-encode one URL path segment.
fn seg(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A release on flux: `(id, asset names)`.
pub type ReleaseAssets = (i64, Vec<String>);

impl FluxApi {
    pub fn new(server: &str, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("flux-migrate/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { http, base: server.trim_end_matches('/').to_string(), token: token.filter(|t| !t.is_empty()) })
    }

    pub fn server(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, format!("{}/api/v1/{path}", self.base));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn finish(r: reqwest::RequestBuilder, what: &str) -> Result<(u16, Value)> {
        let resp = r.send().await.with_context(|| format!("flux: {what}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::String(text))))
    }

    async fn call(&self, method: Method, path: &str, body: Option<&(impl Serialize + ?Sized)>) -> Result<(u16, Value)> {
        let what = format!("{method} {path}");
        let mut r = self.request(method, path);
        if let Some(b) = body {
            r = r.json(b);
        }
        Self::finish(r, &what).await
    }

    fn check((status, v): (u16, Value), what: &str) -> Result<Value> {
        if (200..300).contains(&status) {
            return Ok(v);
        }
        let msg = v["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
        bail!("flux: {what}: {msg} ({status})")
    }

    fn repo(name: &str) -> String {
        format!("repos/{}", name.split('/').map(seg).collect::<Vec<_>>().join("/"))
    }

    /// The repository, created with these settings if missing: `(info, created)`.
    pub async fn ensure_repo(
        &self,
        name: &str,
        visibility: &str,
        description: &str,
        default_branch: &str,
    ) -> Result<(Value, bool)> {
        let path = Self::repo(name);
        let (status, v) = self.call(Method::GET, &path, None::<&Value>).await?;
        if status == 200 {
            return Ok((v, false));
        }
        if status != 404 {
            return Self::check((status, v), &format!("reading {name}")).map(|v| (v, false));
        }
        let body = json!({
            "name": name,
            "visibility": visibility,
            "description": description,
            "default_branch": default_branch,
            "seed_labels": false,
        });
        let v = Self::check(self.call(Method::POST, "repos", Some(&body)).await?, &format!("creating {name}"))?;
        Ok((v, true))
    }

    pub async fn patch_repo(&self, name: &str, body: &Value) -> Result<Value> {
        Self::check(self.call(Method::PATCH, &Self::repo(name), Some(body)).await?, &format!("updating {name}"))
    }

    pub async fn import(&self, name: &str, batch: &ImportBatch) -> Result<Value> {
        let path = format!("{}/import", Self::repo(name));
        Self::check(self.call(Method::POST, &path, Some(batch)).await?, "bulk import")
    }

    /// Configured branch rules (`None`: the built-in defaults apply).
    pub async fn protection(&self, name: &str) -> Result<Option<Vec<BranchRule>>> {
        let path = format!("{}/protection", Self::repo(name));
        let v = Self::check(self.call(Method::GET, &path, None::<&Value>).await?, "reading branch protection")?;
        Ok(serde_json::from_value(v["rules"].clone()).unwrap_or(None))
    }

    pub async fn set_protection(&self, name: &str, rules: &[BranchRule]) -> Result<()> {
        let path = format!("{}/protection", Self::repo(name));
        Self::check(self.call(Method::PUT, &path, Some(&json!({ "rules": rules }))).await?, "setting branch protection")?;
        Ok(())
    }

    pub async fn release(&self, name: &str, tag: &str) -> Result<Option<ReleaseAssets>> {
        let path = format!("{}/releases/tags/{}", Self::repo(name), seg(tag));
        let (status, v) = self.call(Method::GET, &path, None::<&Value>).await?;
        if status == 404 {
            return Ok(None);
        }
        let v = Self::check((status, v), &format!("reading release {tag}"))?;
        let names = v["assets"].as_array().into_iter().flatten().filter_map(|a| a["name"].as_str().map(str::to_string)).collect();
        Ok(Some((v["id"].as_i64().unwrap_or_default(), names)))
    }

    pub async fn upload_asset(&self, name: &str, release: i64, asset: &str, content_type: &str, data: Bytes) -> Result<()> {
        let path = format!("{}/releases/{release}/assets?name={}", Self::repo(name), seg(asset));
        let ctype = if content_type.is_empty() { "application/octet-stream" } else { content_type };
        let r = self.request(Method::POST, &path).header(reqwest::header::CONTENT_TYPE, ctype).body(data);
        Self::check(Self::finish(r, "uploading an asset").await?, &format!("uploading {asset}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments() {
        assert_eq!(FluxApi::repo("acme/widget"), "repos/acme/widget");
        assert_eq!(seg("v1.0/rc 1+x"), "v1.0%2Frc%201%2Bx");
    }
}
