# Deploying flux

flux is one binary (`flux-server`) that serves the web UI, REST, gRPC, MCP and git over HTTP on one port, with git over SSH on an optional second port. It does not terminate TLS itself, so put a reverse proxy in front of it.

## 1. Pick a layout

| | Metadata | Objects | Good for |
|---|---|---|---|
| **All-in-one** | SQLite | local segment files | one machine, small teams, trying it out |
| **Production** | Postgres | S3, R2, GCS (S3 API) or MinIO | durability, backups by your cloud, bigger teams |

All-in-one: `flux-server --data-dir /var/lib/flux`.
Production: `flux-server --meta-url postgres://… --blob-url s3://bucket/prefix --data-dir /var/lib/flux`. In this layout `--data-dir` only holds the server's keys.

**Keep the data dir.** It holds `secret.key`, which encrypts CI secrets and 2FA enrollments, and the SSH host key. If you lose `secret.key`, those secrets become unreadable. Every instance of one deployment must share the same key (`--secret-key-file`).

## 2. Run it

### Docker Compose (Postgres + MinIO + flux)

```sh
git clone https://github.com/virajbhartiya/flux && cd flux
FLUX_PUBLIC_URL=https://flux.example.com docker compose up -d --build
docker compose exec flux flux-server token --user admin --admin   # an admin token, printed once
```

Before exposing it, change the Postgres and MinIO passwords in `docker-compose.yml`. Or point `FLUX_META_URL` and `FLUX_BLOB_URL` at managed services (RDS, Cloud SQL, Neon; S3 or R2) and remove the bundled containers.

### A single VM with systemd

```sh
cargo build --release -p flux-server -p flux-cli -p flux-runner
sudo install target/release/flux-server target/release/flux /usr/local/bin/
sudo useradd --system --home /var/lib/flux --create-home flux
```

`/etc/systemd/system/flux.service`:

```ini
[Unit]
Description=flux
After=network-online.target

[Service]
User=flux
Environment=FLUX_DATA_DIR=/var/lib/flux
Environment=FLUX_LISTEN=127.0.0.1:7878
Environment=FLUX_PUBLIC_URL=https://flux.example.com
Environment=FLUX_AUTH=token
Environment=FLUX_SSH_LISTEN=0.0.0.0:2222
ExecStart=/usr/local/bin/flux-server
Restart=always
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl enable --now flux
```

## 3. TLS in front

[Caddy](https://caddyserver.com) gets certificates automatically. gRPC (used by the `flux` CLI) needs HTTP/2 to the backend:

```caddy
flux.example.com {
    reverse_proxy 127.0.0.1:7878 {
        transport http {
            versions h2c 2
        }
        flush_interval -1   # live logs and server-sent events
    }
}
```

With nginx, use `grpc_pass` for `application/grpc` requests, set `proxy_buffering off`, and set `client_max_body_size 0` so large pushes go through.

SSH does not go through the proxy: open port 2222 (or map 22 to it) and set `--ssh-host flux.example.com:2222` if the clone URL should differ.

## 4. Settings that matter in public

| Setting | Why |
|---|---|
| `--auth token` or `private` | **The default is `open` (no auth).** `token` allows anonymous reads of public repositories, with writes needing a token. `private` requires sign-in for everything. |
| `--public-url https://…` | Used in links, emails, webhook payloads and sign-in redirects. |
| `--signup closed` (default) or `open` | The first account to sign up at `/-/signup` becomes site admin, so create it right after first boot. |
| `--require-2fa` | Makes every password sign-in ask for TOTP. |
| `--github-client-id/-secret`, `--google-…`, `--oidc-issuer/-client-id/-client-secret` | "Continue with …" sign-in. The callback URL is `https://flux.example.com/-/oauth/{github,google,oidc}/callback`. |
| `--smtp-url smtps://user:pass@host:465 --smtp-from "flux <flux@example.com>"` | Notification email. |
| `--api-rate-per-minute` | REST and MCP rate limit per token (default 3000/min). |

Most flags can also be set with a `FLUX_…` environment variable; `flux-server --help` lists each one.

## 5. CI runners

Run runners on separate machines, not on the server:

```sh
curl -X POST https://flux.example.com/api/v1/runners -H "Authorization: Bearer $ADMIN" -d '{"name":"build-1"}'
FLUX_SERVER=https://flux.example.com FLUX_RUNNER_TOKEN=flxr_… flux-runner --concurrency 2
```

See [crates/flux-runner](../crates/flux-runner/README.md).

## 6. Operate it

- **Health:** `/-/healthz` and `/-/readyz` for load balancers. Prometheus metrics are at `/-/metrics`; restrict that path at the proxy.
- **Backups:** all-in-one: `flux-server backup /backups/$(date +%F)` (online, consistent). Production: Postgres PITR or `pg_dump`, plus bucket versioning. Back up `secret.key` separately.
- **Integrity:** run `flux-server fsck` on a schedule (`--full` re-hashes everything; both are safe online).
- **Upgrades:** stop, replace the binary, start. Migrations run on boot, and SIGTERM drains requests first.
- **Moving repositories in:** use `flux migrate github owner/repo` for history, issues, PRs and releases; see [migrating-from-github.md](migrating-from-github.md).

## Known limits

- Running several server instances against one Postgres and bucket hasn't been tested yet. Run one instance for now, and add read capacity with the cache tier (`flux-server cache --upstream …`).
- Shallow and partial git clones are not supported.
