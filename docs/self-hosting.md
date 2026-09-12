# Self-hosting Vorcall

**English** · [Português (BR)](self-hosting.pt-BR.md)

Everything you need to run a Vorcall server for your group: a first install, putting it on
the internet, every setting, backups and upgrades.

- [Quick start](#quick-start)
- [Connecting clients](#connecting-clients)
- [Putting it on the internet](#putting-it-on-the-internet)
- [Configuration](#configuration)
- [Day-to-day administration](#day-to-day-administration)
- [Backups](#backups)
- [Upgrading](#upgrading)
- [Client updates](#client-updates)
- [Monitoring and logs](#monitoring-and-logs)
- [Troubleshooting](#troubleshooting)

---

## Quick start

**Requirements:** a Linux machine with Docker and the Compose plugin, roughly 1 GB of free
memory and 2 GB of disk to start. The build needs a little more; a 1 vCPU / 1 GB VPS is
enough to run it but will be slow to build.

```bash
git clone https://github.com/freedomiit/freedom.vorcall.git
cd freedom.vorcall
docker compose up -d
```

The first run builds the backend image from source, which takes a few minutes. When it
finishes, the stack applies its own database migrations, seeds a server with an `@everyone`
role and a `General` category holding a `general` text channel and a `General` voice
channel, and generates two secrets it had no safe default for.

Watch it come up:

```bash
docker compose logs -f backend
```

### Read the door key

Vorcall's API is gated by a pre-shared **door key** that every client sends with every
request. It is not an account password — everyone who connects to your server uses the same
one — but without it nobody reaches the server at all.

```bash
docker compose logs backend | grep "Server key"
```

```
[22:17:07 WRN]  No Vorcall:ServerKey configured, so one was generated and saved in
/data/server-key. Give it to everyone who connects — it goes in the client's Server
section. Server key: fb8398eff26d83640069ddd02cb6e3c3
```

It lives in the `data` volume and is stable across restarts. If you would rather choose it
yourself, set `VORCALL_SERVER_KEY` before the first `up -d` (see
[Configuration](#configuration)).

### Make the first invite

There is no open registration. Accounts exist only against a one-time invite code, and
**the first account to register becomes the server owner** — the one identity that bypasses
every permission check.

```bash
docker compose exec backend dotnet Vorcall.Server.dll invites new
```

```
Invite code: F6M5F-J5GK4-Y4YP4-XG5PW
Expires: 2026-09-19T22:17:31Z
```

Register with it from the client, and that account owns the server. Mint one invite per
person after that.

---

## Connecting clients

Install the client from [Releases](https://github.com/freedomiit/freedom.vorcall/releases/latest),
then in the sign-in screen:

1. Click **Server** to unfold the section.
2. Put your server's address in the first field — `http://192.168.1.10:5000` on a LAN,
   `https://chat.example.org` once you have a domain.
3. Put the door key in the second.
4. **Use this server**, then **Create account** with the invite code.

Both values are saved in `config.toml` next to the client's other preferences, so this is a
one-time step per machine. **Built-in server** puts the client back on whatever address the
build carries.

Two environment variables, `VORCALL_SERVER_URL` and `VORCALL_SERVER_KEY`, override the saved
values and are useful for scripted installs. When either is set the in-app fields are shown
read-only, because saving them would change nothing.

> **A plain-HTTP server is unencrypted.** Fine on a LAN or over a VPN like Tailscale or
> WireGuard. Anywhere else, put TLS in front of it.

---

## Putting it on the internet

By default the API is published on `127.0.0.1:5000`, reachable only from the machine
itself. That is the right shape: terminate TLS in a reverse proxy on the host and let it
talk to the backend over loopback.

The **voice relay is different**. It is UDP on port 5005, published on all interfaces, and
it cannot go through a reverse proxy — nginx does not carry UDP media, and clients talk to
that port directly. It has to be open in your firewall and in your cloud provider's
security group. Voice and screen sharing simply do not work without it.

### What to open

| | Port | Protocol | Exposure |
| --- | --- | --- | --- |
| API + WebSocket | 443 | TCP | Public, through the reverse proxy |
| ACME challenge | 80 | TCP | Public, for certificate issuance |
| Voice relay | 5005 | UDP | Public, directly to the container |
| Backend | 5000 | TCP | Loopback only |
| PostgreSQL | 5433 | TCP | Loopback only |

### nginx

A complete site config, replacing `chat.example.org` with your domain. Get the certificate
with `certbot certonly --webroot -w /var/www/certbot -d chat.example.org` first.

The three unbuffered `location` blocks are not optional: uploads and update downloads are
raw streamed bodies, and a plain `/api/` proxy would apply nginx's 1 MiB default and buffer
whole files into memory.

```nginx
server {
    listen 80;
    listen [::]:80;
    server_name chat.example.org;

    location /.well-known/acme-challenge/ { root /var/www/certbot; }
    location / { return 301 https://$host$request_uri; }
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name chat.example.org;

    ssl_certificate     /etc/letsencrypt/live/chat.example.org/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/chat.example.org/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;

    client_max_body_size 64k;

    # Live chat: one long-lived WebSocket per client.
    location = /ws {
        proxy_pass http://127.0.0.1:5000;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
        proxy_buffering off;
    }

    # Image uploads and downloads: streamed raw bodies, never buffered.
    location /api/attachments { include /etc/nginx/snippets/vorcall-upload.conf; }
    location /api/images      { include /etc/nginx/snippets/vorcall-upload.conf; }

    # Problem reports uploaded from the client.
    location /api/diagnostics {
        client_max_body_size 5m;
        proxy_request_buffering off;
        proxy_buffering off;
        proxy_read_timeout 120s;
        proxy_pass http://127.0.0.1:5000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
    }

    # Client releases: a whole binary, streamed.
    location /api/updates/ {
        proxy_pass http://127.0.0.1:5000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_buffering off;
        proxy_read_timeout 300s;
    }

    # Admin endpoints are for the CLI on the compose network only.
    location /api/admin/ { return 404; }

    location /api/ {
        proxy_pass http://127.0.0.1:5000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
    }

    location = /health { proxy_pass http://127.0.0.1:5000/health; }
    location / { return 404; }
}
```

`/etc/nginx/snippets/vorcall-upload.conf`:

```nginx
client_max_body_size 9m;
proxy_request_buffering off;
proxy_buffering off;
proxy_read_timeout 300s;
proxy_send_timeout 300s;
proxy_pass http://127.0.0.1:5000;
proxy_http_version 1.1;
proxy_set_header Host $host;
proxy_set_header X-Real-IP $remote_addr;
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
proxy_set_header X-Forwarded-Proto https;
```

### Caddy

Caddy handles certificates on its own, but the upload and update paths still need their
limits raised:

```caddyfile
chat.example.org {
	request_body {
		max_size 64KB
	}

	@big path /api/attachments* /api/images* /api/diagnostics* /api/updates*
	handle @big {
		request_body {
			max_size 9MB
		}
		reverse_proxy 127.0.0.1:5000 {
			flush_interval -1
		}
	}

	handle /api/admin/* {
		respond 404
	}

	handle {
		reverse_proxy 127.0.0.1:5000
	}
}
```

### The firewall

The UDP relay needs a hole in both the host firewall and, on a cloud provider, the security
group or network ACL. This is the single most common reason voice fails on an otherwise
working install.

```bash
# ufw (Debian/Ubuntu)
sudo ufw allow 5005/udp

# firewalld (Fedora/RHEL)
sudo firewall-cmd --permanent --add-port=5005/udp && sudo firewall-cmd --reload

# iptables
sudo iptables -I INPUT -p udp --dport 5005 -j ACCEPT
```

On AWS, GCP, Azure, Oracle Cloud or Hetzner, add an inbound rule for UDP 5005 in the
console as well — the host firewall alone is not enough.

### Kernel buffers

The relay asks for 8 MiB socket buffers. Without raising the kernel ceiling the request is
silently clamped, which shows up as choppy audio once several people are talking:

```bash
printf 'net.core.rmem_max = 16777216\nnet.core.wmem_max = 16777216\n' \
  | sudo tee /etc/sysctl.d/90-vorcall.conf
sudo sysctl --system
docker compose restart backend
```

---

## Configuration

Every setting is optional. Put overrides in a `.env` file next to `docker-compose.yml`;
[`.env.example`](../.env.example) is a commented copy to start from.

```bash
cp .env.example .env
$EDITOR .env
docker compose up -d
```

### Compose-level

| Variable | Default | What it does |
| --- | --- | --- |
| `VORCALL_BIND` | `127.0.0.1` | Interface the API is published on. `0.0.0.0` exposes it directly — only do that if nothing is proxying. |
| `VORCALL_PORT` | `5000` | Host port for the API. |
| `VORCALL_VOICE_PORT` | `5005` | Host and container port for the UDP relay. |
| `VORCALL_SERVER_KEY` | *generated* | The door key. Set it to choose your own instead of reading the generated one. |
| `VORCALL_JWT_SIGNING_KEY` | *generated* | Base64 of 32 random bytes. Changing it signs everyone out within 15 minutes. |
| `VORCALL_ADMIN_KEY` | *unset* | Enables the admin endpoints the CLI's `users kick` / `disable` need. `openssl rand -hex 32`. |
| `VORCALL_VOICE_HOST` | *empty* | Hostname clients send media to. Empty means "the host the WebSocket came from", which is right unless the relay lives elsewhere. |
| `VORCALL_SHARE_ENABLED` | `true` | `false` turns screen sharing off server-wide; voice keeps working. |
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `vorcall` | Database credentials. The database is published on loopback only. |
| `POSTGRES_PORT` | `5433` | Loopback port for PostgreSQL, for the test suite and local tools. |

### Backend settings

Pass any of these to the `backend` service as an environment variable. Note the **double**
underscore — that is how .NET maps `Vorcall__ShareMaxKbps` onto the `Vorcall:ShareMaxKbps`
setting.

| Variable | Default | What it does |
| --- | --- | --- |
| `Vorcall__VoiceEnabled` | `true` | `false` disables voice entirely; `JoinVoice` answers `VOICE_UNAVAILABLE`. |
| `Vorcall__ShareMaxKbps` | `30000` | Per-session ceiling for screen-share media. Range 1000–200000. |
| `Vorcall__MaxSharersPerRoom` | `3` | How many people may share in one channel at once. Range 1–16. |
| `Vorcall__AttachmentsMaxBytes` | `2147483648` | Total on-disk quota for uploads. Beyond it an upload is refused with 507. |
| `Vorcall__AuthRequestsPerWindow` | `10` | Sign-in and registration requests per minute per IP. |
| `Vorcall__UploadRequestsPerWindow` | `20` | Uploads per minute per account. |
| `Vorcall__MessageBurst` | `20` | Write frames an account may send back to back. |
| `Vorcall__MessagesPerSecond` | `2` | Sustained write-frame rate per account. |
| `Vorcall__DiagnosticsReportsPerHour` | `10` | Problem-report uploads per account per hour. |
| `Vorcall__DataDir` | `/data` | Where generated secrets live. |
| `Vorcall__AttachmentsDir` | `/attachments` | Uploaded images. |
| `Vorcall__LogsDir` | `/logs` | Daily JSON logs, 31 kept. |
| `Vorcall__DiagnosticsDir` | `/diagnostics` | Uploaded problem reports, swept after 30 days. |
| `Vorcall__ReleasesDir` | `/releases` | Signed client releases served under `/api/updates/*`. |

Single-file uploads are capped at 8 MiB and four per message; those are not configurable.

### Where the data lives

Five Docker volumes, all created on the first `up -d`:

| Volume | Holds | Back up? |
| --- | --- | --- |
| `pgdata` | Accounts, messages, channels, roles — everything | **Yes** |
| `data` | The generated door key and signing key | **Yes** |
| `attachments` | Uploaded images, avatars, banners, icons | **Yes** |
| `logs` | Daily JSON logs | No |
| `diagnostics` | Client problem reports | No |
| `releases` | Signed client releases, if you publish any | If you use it |

---

## Day-to-day administration

The server binary is also the admin tool. Every subcommand runs the migrations and exits
without ever starting the web server.

```bash
docker compose exec backend dotnet Vorcall.Server.dll <subcommand>
```

```
invites new [--days N]        one-time invite code, 7-day expiry by default
invites list                  used / revoked / expired
invites revoke <id>           kills an unused code for good

users list                    with client version, platform and last-seen
users disable <username>      locks the account out and revokes its sessions
users enable <username>       lets it back in
users kick <username>         closes the live connection
users revoke-sessions <username>
users set-password <username> prompts for a new one

server show                   name, owner and general channel
server set-owner <username>   hands the server to that account
```

`users kick`, `disable` and `enable` need `VORCALL_ADMIN_KEY` set for their live-kick step;
without it the CLI says so and the lock still takes effect within 30 seconds.

A **disabled** account is an operator's lock — it can be undone with `users enable`. A
**ban**, issued from inside the app by someone with the permission, is a different thing:
it writes a ban row, tombstones every message the account sent, drops its overrides and
removes it from the server.

Full reference: [docs/administration.md](administration.md).

---

## Backups

`pgdata` is what matters; `data` and `attachments` are worth having too. This script dumps
the database, keeps 14 days and is safe to run against a live server:

```bash
#!/usr/bin/env bash
# /usr/local/bin/vorcall-backup
set -euo pipefail
APP_DIR=/opt/vorcall            # wherever you cloned it
cd "$APP_DIR"
set -a; . .env 2>/dev/null || true; set +a
mkdir -p backups
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
docker compose exec -T db pg_dump -Fc \
  -U "${POSTGRES_USER:-vorcall}" -d "${POSTGRES_DB:-vorcall}" \
  > "backups/vorcall-$STAMP.dump"
find backups -name 'vorcall-*.dump' -mtime +14 -delete
```

Nightly, via cron:

```
15 3 * * * root /usr/local/bin/vorcall-backup >> /var/log/vorcall-backup.log 2>&1
```

Back up the secrets and uploads too — losing `data` rotates the door key and signs everyone
out:

```bash
docker run --rm -v freedomvorcall_data:/src:ro -v "$PWD/backups:/out" \
  alpine tar czf /out/data.tar.gz -C /src .
docker run --rm -v freedomvorcall_attachments:/src:ro -v "$PWD/backups:/out" \
  alpine tar czf /out/attachments.tar.gz -C /src .
```

### Restoring

**Destructive** — it drops and recreates everything in the dump. Stop the backend first:

```bash
docker compose stop backend
set -a; . .env 2>/dev/null || true; set +a
docker compose exec -T db pg_restore \
  -U "${POSTGRES_USER:-vorcall}" -d "${POSTGRES_DB:-vorcall}" \
  --clean --if-exists --no-owner < backups/vorcall-<stamp>.dump
docker compose start backend
curl -fsS localhost:5000/health
```

---

## Upgrading

```bash
git pull
docker compose up -d --build
```

The backend applies its own migrations on boot, and `/health` answers 200 only once that
has finished. **Take a database dump before upgrading** — a migration is one-way.

Check the release notes for the version you are moving to before a major jump; a client
older than the server's `min_version` is refused.

---

## Client updates

Vorcall's in-app updater fetches from **whichever server the client is connected to**, at
`/api/updates/manifest`. A self-hosted server serves nothing there unless you put a signed
release manifest in the `releases` volume, so by default your users' clients will report
that no update is available.

Two ways to handle that:

- **Point people at GitHub Releases.** Simplest. Tell them when a new version is out and
  let them re-run the installer; it keeps their configuration and their saved server.
- **Publish your own signed releases.** Generate a signing key with `vorcall-release
  gen-key`, bake the public half into `client/update-keys.pub`, build your own clients and
  serve the signed manifest from the `releases` volume. See
  [docs/releasing.md](releasing.md). This means building and distributing your own client,
  which is only worth it for a large group.

Clients built from this repository with a development key never update at all.

---

## Monitoring and logs

**Logs.** The backend writes to the container's stdout and to daily compact-JSON files in
the `logs` volume, 31 kept. Every WebSocket line carries the session id, user id and
username; every HTTP request produces one line. Tokens, passwords, invite codes and the
signing key are never logged.

```bash
docker compose logs -f backend
docker compose exec backend sh -c 'ls -la /logs'
```

**Metrics.** `GET /metrics` renders a Prometheus text exposition. It is served only to
loopback and private-range sources and is never proxied, so it needs no key:

```bash
curl -s localhost:5000/metrics
```

Gauges cover connections, online users, channels, voice sessions, sharers and watchers;
counters cover messages, uploads, HTTP responses by class, rate-limit refusals and the
relay's packets, bytes and drop reasons.

**Health.** `GET /health` answers 200 once migrations are done and the database responds,
503 otherwise. It is what the compose healthcheck polls.

---

## Troubleshooting

**`docker compose up -d` finishes but the backend restarts.**
`docker compose logs backend`. Usually the database was not ready (the healthcheck should
prevent it) or a migration failed. A migration failure leaves the port closed on purpose.

**Clients say the server refused them before the sign-in screen.**
Wrong door key. Re-read it with `docker compose logs backend | grep "Server key"` and check
the client's Server section.

**Sign-in works, voice does not connect.**
UDP 5005 is not reachable. Check the host firewall *and* your cloud security group.
`nc -u -z -v <your-server> 5005` from another machine, or watch
`docker compose logs -f backend` for relay lines while someone joins.

**Voice connects but sounds choppy with several people.**
The kernel clamped the relay's socket buffers. Apply the
[sysctl settings](#kernel-buffers) and restart the backend.

**Uploads fail at around 1 MB.**
The reverse proxy is applying its default body limit. The `/api/attachments` and
`/api/images` locations need `client_max_body_size 9m` and unbuffered proxying of their own.

**Screen share starts and immediately stops on Linux.**
The client needs PipeWire (`libpipewire-0.3.so.0`) and a desktop portal. Every share raises
the portal picker by design — no source is ever remembered.

**Nobody can do anything, not even the first account.**
The server has no owner. `server show` confirms it; `server set-owner <username>` fixes it,
and takes effect at the next backend restart.

**I lost the `data` volume.**
The door key and signing key are gone. New ones are generated on the next boot: everyone is
signed out and everyone needs the new door key. The accounts and messages in `pgdata` are
untouched.
