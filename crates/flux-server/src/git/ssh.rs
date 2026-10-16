//! git over SSH: `git clone ssh://git@host:2222/acme/app.git`.
//!
//! Users authenticate with the SSH keys on their account (any SSH user
//! name works; `git` is conventional). The server runs the same fetch and
//! push code as smart HTTP; only the transport differs: SSH negotiation is
//! stateful, so fetch uses git's basic ACK/NAK exchange (one ACK for the
//! first common commit, NAK per round until then) before streaming the
//! pack, and push reads the pack until the client closes its side.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use flux_store::User;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::objects::parse_hex;
use crate::auth::Principal;
use crate::core::{Flux, FluxError, FluxResult};
use crate::rbac::Role;

const MAX_PUSH: usize = 2 << 30;

/// Load the host key, creating an Ed25519 key on first start.
pub fn host_key(path: &std::path::Path) -> anyhow::Result<PrivateKey> {
    if path.exists() {
        return Ok(russh::keys::load_secret_key(path, None)?);
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    let kp = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let key = PrivateKey::from(kp);
    let pem = key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, pem.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    tracing::info!(path = %path.display(), "generated SSH host key");
    Ok(key)
}

/// Serve git over SSH on `listener` until `shutdown` resolves.
pub async fn serve(
    flux: Arc<Flux>,
    listener: tokio::net::TcpListener,
    key: PrivateKey,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let config = Arc::new(russh::server::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(600)),
        auth_rejection_time: std::time::Duration::from_millis(500),
        auth_rejection_time_initial: Some(std::time::Duration::from_millis(0)),
        keys: vec![key],
        methods: russh::MethodSet::from([russh::MethodKind::PublicKey].as_slice()),
        nodelay: true,
        window_size: 8 << 20,
        ..Default::default()
    });
    let mut server = Server { flux };
    let running = server.run_on_socket(config, &listener);
    let handle = running.handle();
    tokio::spawn(async move {
        shutdown.await;
        handle.shutdown("server shutting down".into());
    });
    running.await?;
    Ok(())
}

struct Server {
    flux: Arc<Flux>,
}

impl russh::server::Server for Server {
    type Handler = Conn;
    fn new_client(&mut self, peer: Option<std::net::SocketAddr>) -> Conn {
        Conn { flux: self.flux.clone(), user: None, channels: HashMap::new(), peer }
    }
}

struct Conn {
    flux: Arc<Flux>,
    user: Option<User>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    peer: Option<std::net::SocketAddr>,
}

impl Conn {
    async fn user_for(&self, key: &russh::keys::PublicKey) -> Option<(User, i64)> {
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        let k = self.flux.meta.ssh_key_by_fingerprint(&fp).await.ok()??;
        let acct = self.flux.meta.get_account_by_id(k.user_id).await.ok()??;
        (!acct.disabled).then_some((acct.user, k.id))
    }
}

impl russh::server::Handler for Conn {
    type Error = russh::Error;

    async fn auth_publickey_offered(&mut self, _user: &str, key: &russh::keys::PublicKey) -> Result<Auth, Self::Error> {
        Ok(if self.user_for(key).await.is_some() { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, _user: &str, key: &russh::keys::PublicKey) -> Result<Auth, Self::Error> {
        match self.user_for(key).await {
            Some((user, key_id)) => {
                let _ = self.flux.meta.touch_ssh_key(key_id, crate::auth::now_ms()).await;
                tracing::debug!(user = %user.name, peer = ?self.peer, "ssh: authenticated");
                self.user = Some(user);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        // `ssh git@host` greets instead of opening a shell.
        let name = self.user.as_ref().map(|u| u.name.clone()).unwrap_or_default();
        let msg = format!("Hi {name}! You've authenticated with flux, which does not provide shell access.\r\n");
        session.channel_success(channel)?;
        session.data(channel, Bytes::from(msg.into_bytes()))?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        self.channels.remove(&channel);
        Ok(())
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        let Some(ch) = self.channels.remove(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        let Some(user) = self.user.clone() else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let principal = Principal { user: Some(user), mode: self.flux.auth_mode, scopes: "all".into(), kind: "ssh".into() };
        let flux = self.flux.clone();
        let command = String::from_utf8_lossy(data).into_owned();
        tokio::spawn(async move {
            let (mut read, write) = ch.split();
            let mut stdout = write.make_writer();
            let mut stderr = write.make_writer_ext(Some(1));
            let code = {
                let mut stdin = read.make_reader();
                match run(&flux, &principal, &command, &mut stdin, &mut stdout).await {
                    Ok(()) => 0,
                    Err(e) => {
                        tracing::info!(error = %e, command = %command, "ssh git command failed");
                        let _ = stderr.write_all(format!("flux: {e}\n").as_bytes()).await;
                        128
                    }
                }
            };
            let _ = stdout.flush().await;
            let _ = stderr.flush().await;
            drop(stdout);
            drop(stderr);
            let _ = write.exit_status(code).await;
            let _ = write.eof().await;
            let _ = write.close().await;
        });
        Ok(())
    }
}

/// Parse `git-upload-pack '/acme/app.git'` (also `git upload-pack`).
fn parse_command(cmd: &str) -> FluxResult<(&'static str, String)> {
    let cmd = cmd.trim();
    let (service, rest) = [
        ("git-upload-pack", "git-upload-pack"),
        ("git upload-pack", "git-upload-pack"),
        ("git-receive-pack", "git-receive-pack"),
        ("git receive-pack", "git-receive-pack"),
    ]
    .into_iter()
    .find_map(|(prefix, svc)| Some((svc, cmd.strip_prefix(prefix)?.trim())))
    .ok_or_else(|| FluxError::InvalidArgument(format!("unsupported command {cmd:?}: only git fetch and push are available")))?;
    let path = rest.trim_matches(|c| c == '\'' || c == '"');
    let path = path.trim_start_matches("~/").trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() || path.contains("..") {
        return Err(FluxError::InvalidArgument("missing repository path".into()));
    }
    Ok((service, path.to_string()))
}

async fn run<R, W>(flux: &Arc<Flux>, who: &Principal, command: &str, stdin: &mut R, stdout: &mut W) -> FluxResult<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if let Some(rest) = command.trim().strip_prefix("git-lfs-authenticate ") {
        let mut args = rest.split_whitespace();
        let repo = args.next().unwrap_or_default().trim_matches(|c| c == '\'' || c == '"');
        let repo = repo.trim_start_matches('/').trim_end_matches('/');
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        let op = args.next().unwrap_or("download");
        let out = super::lfs::ssh_authenticate(flux, who, repo, op).await?;
        stdout.write_all(out.as_bytes()).await.map_err(|e| FluxError::Internal(e.into()))?;
        stdout.write_all(b"\n").await.map_err(|e| FluxError::Internal(e.into()))?;
        return Ok(());
    }
    let (service, name) = parse_command(command)?;
    let repo = flux.repo(&name).await.map_err(|_| FluxError::NotFound(format!("repository {name} not found")))?;
    let write = service == "git-receive-pack";
    flux.authorize(who, repo, if write { Role::Write } else { Role::Read }).await?;
    if super::http::path_restricted(flux, repo, who).await {
        return Err(FluxError::PermissionDenied(
            "this repository has path restrictions that apply to you; use `flux attach` (sparse) instead of git".into(),
        ));
    }
    let io = |e: std::io::Error| FluxError::Internal(e.into());
    let adv = super::http::advertisement(flux, repo, service, false).await?;
    stdout.write_all(&adv).await.map_err(io)?;
    stdout.flush().await.map_err(io)?;
    if write { receive(flux, repo, who, stdin, stdout).await } else { upload(flux, repo, stdin, stdout).await }
}

/// Read one pkt-line: `Ok(None)` at a flush packet, `Err` at EOF.
async fn read_pkt<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = usize::from_str_radix(std::str::from_utf8(&len).unwrap_or("x"), 16)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad pkt-line length"))?;
    if n < 4 {
        return Ok(None);
    }
    let mut buf = vec![0u8; n - 4];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

fn pkt_line(data: &[u8]) -> Vec<u8> {
    let mut v = format!("{:04x}", data.len() + 4).into_bytes();
    v.extend_from_slice(data);
    v
}

async fn upload<R, W>(flux: &Arc<Flux>, repo: flux_store::RepoId, stdin: &mut R, stdout: &mut W) -> FluxResult<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let io = |e: std::io::Error| FluxError::Internal(e.into());
    // Wants, up to a flush. A bare flush (or EOF) means "nothing to fetch"
    // (`git ls-remote`).
    let mut request = Vec::new();
    let mut any_want = false;
    loop {
        match read_pkt(stdin).await {
            Ok(Some(l)) => {
                if l.starts_with(b"deepen") || l.starts_with(b"shallow") {
                    return Err(FluxError::InvalidArgument(
                        "shallow clones are not supported yet; use flux attach --sparse".into(),
                    ));
                }
                any_want |= l.starts_with(b"want ");
                request.extend(pkt_line(&l));
            }
            Ok(None) => break,
            Err(_) => return Ok(()),
        }
    }
    if !any_want {
        return Ok(());
    }
    request.extend_from_slice(b"0000");
    // Negotiation: ACK the first common commit, NAK each round until then.
    let mut acked = false;
    loop {
        match read_pkt(stdin).await {
            Ok(Some(l)) => {
                let line = l.strip_suffix(b"\n").unwrap_or(&l);
                if line == b"done" {
                    break;
                }
                if let Some(rest) = line.strip_prefix(b"have ") {
                    request.extend(pkt_line(&l));
                    if !acked
                        && let Some(sha) = parse_hex(&rest[..rest.len().min(40)])
                        && flux.meta.git_commits_lookup(repo, &[sha]).await?.first().is_some_and(Option::is_some)
                    {
                        acked = true;
                        stdout
                            .write_all(&pkt_line(format!("ACK {}\n", super::objects::hex(&sha)).as_bytes()))
                            .await
                            .map_err(io)?;
                        stdout.flush().await.map_err(io)?;
                    }
                }
            }
            Ok(None) => {
                if !acked {
                    stdout.write_all(&pkt_line(b"NAK\n")).await.map_err(io)?;
                    stdout.flush().await.map_err(io)?;
                }
            }
            // The client hung up (it had everything).
            Err(_) => return Ok(()),
        }
    }
    request.extend(pkt_line(b"done\n"));
    let (preamble, pack) = super::http::upload_pack_stream(flux, repo, &request).await?;
    // After `done`: NAK if nothing was common; the ACK was already sent.
    if !acked {
        stdout.write_all(&preamble).await.map_err(io)?;
    }
    if let Some(mut rx) = pack {
        while let Some(chunk) = rx.recv().await {
            let chunk: Bytes = chunk.map_err(io)?;
            stdout.write_all(&chunk).await.map_err(io)?;
        }
    }
    stdout.flush().await.map_err(io)?;
    Ok(())
}

async fn receive<R, W>(
    flux: &Arc<Flux>,
    repo: flux_store::RepoId,
    who: &Principal,
    stdin: &mut R,
    stdout: &mut W,
) -> FluxResult<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let io = |e: std::io::Error| FluxError::Internal(e.into());
    let mut body = Vec::new();
    let mut needs_pack = false;
    let mut commands = 0;
    loop {
        match read_pkt(stdin).await {
            Ok(Some(l)) => {
                // `<old> <new> <ref>`: anything but a delete brings a pack.
                if l.len() >= 82 && &l[41..81] != b"0000000000000000000000000000000000000000" {
                    needs_pack = true;
                }
                commands += 1;
                body.extend(pkt_line(&l));
            }
            Ok(None) => break,
            Err(_) => return Ok(()),
        }
    }
    if commands == 0 {
        return Ok(());
    }
    body.extend_from_slice(b"0000");
    if needs_pack {
        // The client closes its side after the pack.
        let mut limited = stdin.take(MAX_PUSH as u64 + 1);
        limited.read_to_end(&mut body).await.map_err(io)?;
        if body.len() > MAX_PUSH {
            return Err(FluxError::InvalidArgument("push too large".into()));
        }
    }
    let report = super::http::receive_pack_report(flux, repo, who, &body).await?;
    stdout.write_all(&report).await.map_err(io)?;
    stdout.flush().await.map_err(io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse_command("git-upload-pack '/acme/app.git'").unwrap(), ("git-upload-pack", "acme/app".into()));
        assert_eq!(parse_command("git-receive-pack 'app'").unwrap(), ("git-receive-pack", "app".into()));
        assert_eq!(parse_command("git upload-pack '~/app.git/'").unwrap(), ("git-upload-pack", "app".into()));
        assert!(parse_command("rm -rf /").is_err());
        assert!(parse_command("git-upload-pack '../etc'").is_err());
    }
}
