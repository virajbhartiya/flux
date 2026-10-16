//! A job's log: masked, buffered and streamed to the server in chunks. The
//! uploads double as the lease heartbeat and carry cancellation back.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::client::Client;
use crate::mask::Masker;

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const FLUSH_BYTES: usize = 64 * 1024;
const MAX_CHUNK: usize = 1 << 20;
/// Idle heartbeat: well inside the server's 90s lease, and how quickly a
/// quiet job notices it was cancelled.
const HEARTBEAT: Duration = Duration::from_secs(5);
const MAX_LOG: u64 = 256 << 20;

#[derive(Default)]
struct State {
    masker: Masker,
    pending: Vec<u8>,
    written: u64,
    truncated: bool,
    closed: bool,
}

/// Cheap to clone; every clone writes to the same log.
#[derive(Clone, Default)]
pub struct JobLog {
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
}

impl JobLog {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mask `value` in everything written from now on.
    pub fn add_mask(&self, value: &str) {
        self.lock().masker.add(value);
    }

    pub fn write(&self, data: &[u8]) {
        let mut s = self.lock();
        let masked = s.masker.push(data);
        self.append(&mut s, masked);
    }

    pub fn line(&self, text: impl AsRef<str>) {
        let mut data = text.as_ref().as_bytes().to_vec();
        data.push(b'\n');
        self.write(&data);
    }

    /// Release bytes held back by masking (at the end of a step's output).
    pub fn sync(&self) {
        let mut s = self.lock();
        let rest = s.masker.finish();
        self.append(&mut s, rest);
    }

    fn append(&self, s: &mut State, mut data: Vec<u8>) {
        if s.truncated || data.is_empty() {
            return;
        }
        if s.written + data.len() as u64 > MAX_LOG {
            data.truncate((MAX_LOG - s.written) as usize);
            data.extend_from_slice(b"\n##[warning]The log is too large; further output is discarded.\n");
            s.truncated = true;
        }
        s.written += data.len() as u64;
        s.pending.extend_from_slice(&data);
        if s.pending.len() >= FLUSH_BYTES {
            self.wake.notify_one();
        }
    }

    /// No more writes: the uploader sends what is left and stops.
    pub fn close(&self) {
        self.sync();
        self.lock().closed = true;
        self.wake.notify_one();
    }

    fn take(&self) -> (Vec<u8>, bool) {
        let mut s = self.lock();
        let n = s.pending.len().min(MAX_CHUNK);
        let chunk: Vec<u8> = s.pending.drain(..n).collect();
        (chunk, s.closed && s.pending.is_empty())
    }

    fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Everything not yet uploaded (tests).
    #[cfg(test)]
    pub fn pending(&self) -> Vec<u8> {
        self.lock().pending.clone()
    }
}

/// Upload `log` for `job` until it is closed and drained. Sets `cancel`
/// when the server cancels the job or it is no longer ours. Returns `true`
/// if the job was lost (taken over or deleted).
pub fn spawn_uploader(client: Client, job: i64, log: JobLog, cancel: Arc<watch::Sender<bool>>) -> tokio::task::JoinHandle<bool> {
    tokio::spawn(async move {
        let mut seq = 0i64;
        let mut last_sent = Instant::now();
        let mut backoff = Duration::from_millis(500);
        let mut retry: Option<Vec<u8>> = None;
        loop {
            if !log.is_closed() {
                tokio::select! {
                    _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
                    _ = log.wake.notified() => {}
                }
            }
            let (chunk, done) = match retry.take() {
                Some(c) => (c, false),
                None => log.take(),
            };
            if chunk.is_empty() {
                if done {
                    return false;
                }
                if last_sent.elapsed() < HEARTBEAT {
                    continue;
                }
            }
            let next = if chunk.is_empty() { seq } else { seq + 1 };
            match client.log(job, next, chunk.clone()).await {
                Ok(cancelled) => {
                    seq = next;
                    last_sent = Instant::now();
                    backoff = Duration::from_millis(500);
                    if cancelled && !*cancel.borrow() {
                        tracing::info!(job, "job cancelled by the server");
                        cancel.send_replace(true);
                    }
                    if done {
                        return false;
                    }
                }
                Err(e) if e.retryable() => {
                    tracing::warn!(job, error = %e, "log upload failed; retrying");
                    if !chunk.is_empty() {
                        retry = Some(chunk);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
                Err(e) => {
                    tracing::error!(job, error = %e, "the server no longer accepts this job's log; abandoning it");
                    cancel.send_replace(true);
                    return true;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_and_buffers() {
        let log = JobLog::new();
        log.add_mask("topsecret");
        log.write(b"value: top");
        log.write(b"secret\n");
        log.line("again topsecret");
        log.write(b"tail top");
        log.sync();
        assert_eq!(String::from_utf8(log.pending()).unwrap(), "value: ***\nagain ***\ntail top");
        let (chunk, done) = log.take();
        assert_eq!(chunk.len(), "value: ***\nagain ***\ntail top".len());
        assert!(!done);
        log.close();
        assert!(log.take().1);
    }
}
