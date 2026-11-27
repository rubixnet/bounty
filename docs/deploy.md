# Deploying `traction-server`

One Linux binary, migrations embedded, a `systemd` unit and Caddy for TLS. **No Docker is required.** Provisioning (buying the server, DNS, the Postgres password, the R2 bucket, the GitHub App and the CI key) is a human step; this page lists what to provision and what to put where. Prices: `docs/cost-model.md`.

## What you need

- A Linux host (the cost model assumes a Hetzner AX42) with Postgres 17 and Caddy 2 installed from the distro or vendor packages.
- A Cloudflare R2 bucket and an S3 API token for it (Audit Bundles and `pg_dump` backups).
- A domain pointing at the host.
- A Hack Club Auth OIDC client (`client_id` and secret).
- Optional: a GitHub App (below) and a CI Ed25519 key (below).

## 1. The binary

Build on a Linux machine (or take the release workflow's artifact, `.github/workflows/release.yml`):

```sh
cargo build --release -p traction-server
sudo install -m 0755 target/release/traction-server /usr/local/bin/traction-server
```

The migrations in `crates/traction-server/migrations/` are compiled into the binary and applied at start-up (`sqlx::migrate!`), and the monthly partitions are created at start-up and daily. Nothing else has to be run against the database.

## 2. Postgres

```sh
sudo -u postgres createuser traction
sudo -u postgres createdb -O traction traction
sudo -u postgres psql -c "ALTER USER traction PASSWORD '<generated>'"
```

The connection string goes in a secret file, not in the config (the server refuses a config with an inline secret):

```sh
sudo install -d -m 0750 -o root -g traction /etc/traction
echo 'postgres://traction:<generated>@127.0.0.1/traction' | sudo tee /etc/traction/database-url >/dev/null
sudo chown root:traction /etc/traction/database-url && sudo chmod 0640 /etc/traction/database-url
```

## 3. Configuration: `/etc/traction/traction-server.toml`

Secrets are only ever `*_env` or `*_file` references. Public keys may be inline.

```toml
[server]
bind = "127.0.0.1:8080"
public_url = "https://traction.example.com"
database_url_file = "/etc/traction/database-url"

[auth]
provider = "hackclub"
client_id = "<hack club client id>"
client_secret_file = "/etc/traction/oidc-client-secret"
# Defaults assume Doorkeeper paths under the issuer. They are UNVERIFIED
# against the live Hack Club IdP: check them once and override if needed.
# authorization_endpoint = "https://auth.hackclub.com/oauth/authorize"
# token_endpoint         = "https://auth.hackclub.com/oauth/token"
# jwks_uri               = "https://auth.hackclub.com/oauth/discovery/keys"

[roles]
reviewers = []        # OIDC `sub`s
admins    = []

[blob]
provider = "r2"
r2_endpoint = "https://<account id>.r2.cloudflarestorage.com"
r2_bucket = "traction-bundles"
r2_access_key_id_env = "TRACTION_R2_ACCESS_KEY_ID"
r2_secret_access_key_file = "/etc/traction/r2-secret"

[provenance]
# Public keys of the CI signing key(s); see "Build provenance" below.
ci_public_keys = ["<64 hex digits>"]

[github]
app_id = "<numeric app id>"
private_key_file = "/etc/traction/github-app.pem"

[jev]      # filled in by the JevEngine ticket (spend limit, rubrics)
[heuristic]
[anchor_policy]
```

Keep the secret files `0640 root:traction`.

## 4. `systemd`: `/etc/systemd/system/traction-server.service`

```ini
[Unit]
Description=Traction server
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
User=traction
Group=traction
Environment=TRACTION_R2_ACCESS_KEY_ID=<r2 access key id>
ExecStart=/usr/local/bin/traction-server --config /etc/traction/traction-server.toml
Restart=on-failure
RestartSec=3
# The server needs no privileges.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin traction
sudo systemctl daemon-reload && sudo systemctl enable --now traction-server
journalctl -u traction-server -f
```

`LimitNOFILE` matters: peak concurrency is about 1,500 simultaneous device connections.

## 5. Caddy (TLS): `/etc/caddy/Caddyfile`

```caddyfile
traction.example.com {
	encode zstd gzip
	# Anchors and heartbeats are small; the local blob route (not used with R2)
	# carries whole bundles, so keep the cap above [blob] max_bytes (64 MB).
	request_body {
		max_size 70MB
	}
	reverse_proxy 127.0.0.1:8080
}
```

Caddy obtains and renews the certificate itself. The server is bound to loopback and is reached only through Caddy. Open ports 80 and 443 only.

The server's local-ingestion hardening (Origin and Host checks) applies to the daemon's loopback bus, not this server, so nothing extra is configured on the proxy.

## 6. Backups: `pg_dump` to R2

A daily timer. `rclone` (or the `aws` CLI against the R2 endpoint) uploads the dump; keep 7 daily, 4 weekly and 3 monthly copies with an R2 lifecycle rule. The cost model prices exactly this.

```sh
#!/bin/sh
# /usr/local/sbin/traction-backup
set -eu
f="traction-$(date -u +%Y%m%dT%H%M%SZ).dump"
pg_dump --format=custom --compress=9 --dbname=traction --file="/var/backups/$f"
rclone copyto "/var/backups/$f" "r2:traction-backups/daily/$f"
rm -f "/var/backups/$f"
```

Restore with `pg_restore --clean --dbname=traction <file>`. Test a restore before relying on it.

## 7. Build provenance (soft)

Release CI signs `{version, git_commit, target_triple}` with a CI Ed25519 key. The signed statement is compiled into the daemon (`TRACTION_BUILD_STATEMENT`), sent at pairing and on the first heartbeat after each start, and the server tags the device `ci_signed` or `self_built`.

1. Generate a 32-byte seed: `openssl rand -hex 32`. Store it only as the repository secret `TRACTION_CI_SIGNING_SEED`.
2. Print the public key with the same helper the workflow uses: `TRACTION_CI_SIGNING_SEED=<seed> cargo run -p traction-core --bin traction-sign-build -- --public-key`. Put it in `[provenance] ci_public_keys`. A rotation lists both keys for a while.

**The tag is soft.** The statement can be copied verbatim into a fork's binary, so `ci_signed` is a hint, not proof of what runs. A `self_built` device is sent to a standard spot-check (Yellow) and never to Red.

## 8. GitHub App (optional linking)

Linking is optional for tracking (ADR 0002). To enable it, create a GitHub App with **read-only `metadata` and `contents`** permissions, installable on selected repos, and generate its private key. Put the App ID and the PEM in `[github]` as above. The key comes only from the file or from the environment variable named by `private_key_env`. Endpoints: `POST /v1/github/link`, `POST /v1/github/projects/{id}/sync`, `DELETE /v1/github/projects/{id}`. Only commit `oid`, `authored_at`, `committed_at` and an author-match flag are stored (no messages, no diffs).

## 9. Checks after deploy

```sh
curl -sS https://traction.example.com/v1/me          # 401 {"error":"unauthorized"}
systemctl status traction-server
sudo -u postgres psql traction -c '\dt'              # migrated tables, monthly partitions
```

## 10. Proving capacity (Gate 1)

The `load` binary drives the real server and Postgres 17. A reduced version runs on every PR (`load-smoke` in `.github/workflows/ci.yml`). The full-scale run is `.github/workflows/load.yml` (nightly and `workflow_dispatch`), which publishes the report as the `gate1-load-report` artifact. To run it by hand against a scratch Postgres (it creates and drops its own database):

```sh
cargo build --release -p traction-server --bins
target/release/load --database-url postgres://postgres@127.0.0.1/postgres \
    --devices 15488 --seed-buckets 6600000 --report gate1-load-report.json
```

A full-scale run needs about 4 GB of free disk and some tens of minutes. It exits non-zero when ingest p95 is not below 50 ms or server RSS is not below 512 MB.
