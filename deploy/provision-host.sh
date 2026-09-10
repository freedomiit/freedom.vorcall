#!/usr/bin/env bash
# Provision the Oracle host for Vorcall: certbot, nginx site, TLS renewal hook,
# the voice media UDP port, the production .env and (optionally) the deploy
# SSH key.
#
# Runs ON THE HOST as the `ubuntu` user, from /opt/vorcall/deploy/:
#   scp -r deploy .env.production.example user@<host>:/opt/vorcall/
#   ssh user@<host> 'cd /opt/vorcall/deploy && ./provision-host.sh'
#
# Idempotent: safe to re-run. It never touches other sites' nginx files and
# never prints secret values.
#
# Optional env overrides: EXPECTED_IP, DEPLOY_PUBKEY (public key appended to
# ~/.ssh/authorized_keys so GitHub Actions can deploy).
set -euo pipefail

DOMAIN=vorcall.example.com
EXPECTED_IP="${EXPECTED_IP:-203.0.113.10}"
APP_DIR=/opt/vorcall
SITE=/etc/nginx/sites-available/$DOMAIN
CERT_EMAIL=admin@example.com
WEBROOT=/var/www/certbot
VOICE_PORT=5005

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIVE_CERT="/etc/letsencrypt/live/$DOMAIN/fullchain.pem"

step() { printf '==> %s\n' "$1"; }

# --- 1. preflight -----------------------------------------------------------
step "preflight: DNS and required tooling"

addrs="$(getent ahostsv4 "$DOMAIN" | awk '{print $1}' | sort -u || true)"
if [ -z "$addrs" ]; then
    echo "ERROR: $DOMAIN does not resolve to any IPv4 address." >&2
    echo "Point the A record at $EXPECTED_IP with the Cloudflare proxy OFF (DNS only)." >&2
    exit 2
fi
for a in $addrs; do
    if [ "$a" != "$EXPECTED_IP" ]; then
        echo "ERROR: $DOMAIN resolves to:" >&2
        printf '  %s\n' $addrs >&2
        echo "Expected only $EXPECTED_IP. The A record must point at this host with" >&2
        echo "the Cloudflare proxy OFF (DNS only) — certbot's HTTP-01 challenge and" >&2
        echo "the WebSocket endpoint both require a direct connection." >&2
        exit 2
    fi
done
echo "DNS ok: $DOMAIN -> $EXPECTED_IP"

if ! nginx -v 2>/dev/null && ! sudo nginx -v 2>/dev/null; then
    echo "ERROR: nginx is not available on this host." >&2
    exit 2
fi
if ! docker compose version >/dev/null 2>&1; then
    echo "ERROR: 'docker compose' is not available on this host." >&2
    exit 2
fi
echo "nginx and docker compose ok"

# --- 2. certbot -------------------------------------------------------------
step "certbot"
if command -v certbot >/dev/null 2>&1; then
    echo "already installed: $(certbot --version 2>&1)"
else
    sudo apt-get update -qq
    sudo apt-get install -y -qq certbot
    echo "installed: $(certbot --version 2>&1)"
fi

# --- 3. ACME webroot --------------------------------------------------------
step "ACME webroot $WEBROOT"
sudo mkdir -p "$WEBROOT"

# install_site <source conf> — writes $SITE, enables it, validates, reloads.
# On a failed `nginx -t` the previous state is restored and nginx is NOT reloaded.
install_site() {
    local src="$1" backup="" had_site=no had_link=no
    if [ -f "$SITE" ]; then
        had_site=yes
        backup="$(mktemp)"
        sudo cp -f "$SITE" "$backup"
    fi
    # a site deliberately left disabled must stay disabled if we roll back
    if [ -L "/etc/nginx/sites-enabled/$DOMAIN" ]; then
        had_link=yes
    fi
    sudo install -m 644 -o root -g root "$src" "$SITE"
    sudo ln -sfn "$SITE" "/etc/nginx/sites-enabled/$DOMAIN"
    if ! sudo nginx -t; then
        echo "ERROR: nginx -t failed with $(basename "$src"); rolling back." >&2
        if [ "$had_site" = yes ]; then
            sudo install -m 644 -o root -g root "$backup" "$SITE"
        else
            sudo rm -f "$SITE"
        fi
        if [ "$had_link" = no ]; then
            sudo rm -f "/etc/nginx/sites-enabled/$DOMAIN"
        fi
        if [ -n "$backup" ]; then rm -f "$backup"; fi
        exit 1
    fi
    if [ -n "$backup" ]; then rm -f "$backup"; fi
    sudo systemctl reload nginx
    echo "installed $(basename "$src") as $SITE and reloaded nginx"
}

# --- 4. bootstrap site + certificate ---------------------------------------
step "TLS certificate for $DOMAIN"
if sudo test -f "$LIVE_CERT"; then
    echo "certificate already present, skipping issuance"
else
    echo "no certificate yet: installing the HTTP-only bootstrap site first"
    install_site "$SCRIPT_DIR/nginx/vorcall.http.conf"
    sudo certbot certonly --webroot -w "$WEBROOT" -d "$DOMAIN" \
        --non-interactive --agree-tos -m "$CERT_EMAIL" --keep-until-expiring
fi

# --- 5. final site ----------------------------------------------------------
step "final nginx site"
install_site "$SCRIPT_DIR/nginx/vorcall.conf"

# --- 6. renewal hook --------------------------------------------------------
step "certbot renewal hook"
sudo mkdir -p /etc/letsencrypt/renewal-hooks/deploy
printf '#!/bin/sh\nsystemctl reload nginx\n' \
    | sudo tee /etc/letsencrypt/renewal-hooks/deploy/vorcall-reload-nginx.sh >/dev/null
sudo chmod 755 /etc/letsencrypt/renewal-hooks/deploy/vorcall-reload-nginx.sh
echo "hook in place: /etc/letsencrypt/renewal-hooks/deploy/vorcall-reload-nginx.sh"

# --- 7. voice media port -----------------------------------------------------
step "voice media port udp/$VOICE_PORT"
# Docker publishes the backend's UDP port through its own DNAT/FORWARD chains, so
# this INPUT rule is belt-and-braces: it only matters if the backend ever runs
# with host networking. Inserted before the trailing REJECT so it is reachable.
if sudo iptables -C INPUT -p udp -m udp --dport "$VOICE_PORT" -j ACCEPT 2>/dev/null; then
    echo "iptables: udp/$VOICE_PORT already accepted"
else
    reject_line="$(sudo iptables -L INPUT --line-numbers -n | awk '$2 == "REJECT" {print $1; exit}')"
    if [ -n "$reject_line" ]; then
        sudo iptables -I INPUT "$reject_line" -p udp -m udp --dport "$VOICE_PORT" -j ACCEPT
    else
        sudo iptables -A INPUT -p udp -m udp --dport "$VOICE_PORT" -j ACCEPT
    fi
    if command -v netfilter-persistent >/dev/null 2>&1; then
        sudo netfilter-persistent save >/dev/null
        echo "iptables: accepted udp/$VOICE_PORT and saved the rules"
    else
        echo "iptables: accepted udp/$VOICE_PORT (netfilter-persistent not found; rule is not persisted)"
    fi
fi

# --- 8. production .env -----------------------------------------------------
step "production .env"
mkdir -p "$APP_DIR"
# Bind-mounted read-only into the backend; the release workflow scps manifests and binaries here.
mkdir -p "$APP_DIR/releases"
if [ -f "$APP_DIR/.env" ]; then
    echo "$APP_DIR/.env already exists, left untouched"
elif [ ! -f "$APP_DIR/.env.production.example" ]; then
    echo "ERROR: $APP_DIR/.env.production.example is missing; copy it from the repo" >&2
    echo "and re-run, or write $APP_DIR/.env by hand." >&2
    exit 1
else
    (
        umask 077
        pg_pass="$(openssl rand -hex 24)"
        server_key="$(openssl rand -hex 32)"
        jwt_key="$(openssl rand -base64 32)"
        sed -e "s|__POSTGRES_PASSWORD__|$pg_pass|g" \
            -e "s|__SERVER_KEY__|$server_key|g" \
            -e "s|__JWT_SIGNING_KEY__|$jwt_key|g" \
            "$APP_DIR/.env.production.example" > "$APP_DIR/.env"
    )
    echo "generated $APP_DIR/.env"
    echo "bake the same Vorcall__ServerKey into client builds via VORCALL_SERVER_KEY"
fi

# --- 9. deploy public key ---------------------------------------------------
step "deploy SSH key"
if [ -n "${DEPLOY_PUBKEY:-}" ]; then
    mkdir -p ~/.ssh
    chmod 700 ~/.ssh
    if [ ! -f ~/.ssh/authorized_keys ]; then
        touch ~/.ssh/authorized_keys
        chmod 600 ~/.ssh/authorized_keys
    fi
    if grep -qxF "$DEPLOY_PUBKEY" ~/.ssh/authorized_keys; then
        echo "DEPLOY_PUBKEY already authorized"
    else
        printf '%s\n' "$DEPLOY_PUBKEY" >> ~/.ssh/authorized_keys
        echo "DEPLOY_PUBKEY appended to ~/.ssh/authorized_keys"
    fi
else
    echo "DEPLOY_PUBKEY not set, skipping"
fi

# --- 10. summary ------------------------------------------------------------
step "summary"
sudo certbot certificates -d "$DOMAIN" 2>/dev/null | grep -i 'expiry date' || \
    echo "certificate expiry: unknown"
if sudo nginx -t >/dev/null 2>&1; then
    echo "nginx -t: ok"
else
    echo "nginx -t: FAILED (run 'sudo nginx -t' for details)"
fi
if [ -f "$APP_DIR/.env" ]; then
    echo "$APP_DIR/.env: present"
else
    echo "$APP_DIR/.env: MISSING — the backend will not start without it"
fi
echo "next: push to main (or run the deploy workflow) to pull the image and start the stack"
echo "reminder: the OCI VCN security list must allow ingress UDP $VOICE_PORT from 0.0.0.0/0 (OCI console), or voice will not connect"
