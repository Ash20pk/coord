#!/usr/bin/env bash
# Provision a droplet to host the coord relay behind TLS.
#
#   scp -r deploy root@<ip>:/root/ && ssh root@<ip> 'bash /root/deploy/provision.sh'
#
# Idempotent: re-run it to deploy a new revision. Env overrides:
#   DOMAIN=relay.knoot.dev  REF=main  REPO=https://github.com/Ash20pk/coord.git
set -euo pipefail

DOMAIN="${DOMAIN:-relay.knoot.dev}"
REF="${REF:-main}"
REPO="${REPO:-https://github.com/Ash20pk/coord.git}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
say() { printf '\n== %s\n' "$*"; }

say "packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential pkg-config git curl ca-certificates \
	debian-keyring debian-archive-keyring apt-transport-https

if ! command -v caddy >/dev/null; then
	say "caddy"
	curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key \
		| gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
	curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt \
		> /etc/apt/sources.list.d/caddy-stable.list
	apt-get update -qq
	apt-get install -y -qq caddy
fi

say "rust"
if ! [[ -x /root/.cargo/bin/cargo ]]; then
	curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
fi
export PATH="/root/.cargo/bin:$PATH"

say "build $REF"
if [[ -d /opt/coord/.git ]]; then
	git -C /opt/coord fetch --quiet origin "$REF"
	git -C /opt/coord checkout --quiet -B deploy "origin/$REF"
else
	git clone --quiet "$REPO" /opt/coord
	git -C /opt/coord checkout --quiet -B deploy "origin/$REF"
fi
echo "   $(git -C /opt/coord rev-parse --short HEAD)  $(git -C /opt/coord log -1 --format=%s)"
cargo build --release --manifest-path /opt/coord/Cargo.toml
install -m 0755 /opt/coord/target/release/coord /usr/local/bin/coord

say "user + state"
id -u coord >/dev/null 2>&1 || useradd --system --home /var/lib/coord --shell /usr/sbin/nologin coord
install -d -o coord -g coord -m 0750 /var/lib/coord

say "token"
install -d -m 0755 /etc/coord
if ! [[ -s /etc/coord/relay.env ]]; then
	printf 'COORD_RELAY_TOKEN=%s\n' "$(openssl rand -hex 24)" > /etc/coord/relay.env
	chmod 0600 /etc/coord/relay.env
	echo "   generated a new token"
else
	echo "   keeping the existing token"
fi

say "systemd"
install -m 0644 "$HERE/coord-relay.service" /etc/systemd/system/coord-relay.service
systemctl daemon-reload
systemctl enable --quiet coord-relay
systemctl restart coord-relay

say "caddy"
sed "s/relay\.knoot\.dev/$DOMAIN/" "$HERE/Caddyfile" > /etc/caddy/Caddyfile
systemctl reload caddy || systemctl restart caddy

say "firewall"
if command -v ufw >/dev/null; then
	ufw allow OpenSSH >/dev/null
	ufw allow 80,443/tcp >/dev/null
	ufw --force enable >/dev/null
	echo "   ssh + 80/443 only; the relay is loopback-bound and unreachable directly"
fi

say "checks"
TOKEN="$(sed -n 's/^COORD_RELAY_TOKEN=//p' /etc/coord/relay.env)"
sleep 1
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:7420/api/repos")
[[ $code == 401 ]] && echo "[ok  ] relay refuses an untokened request (401)" \
	|| { echo "[FAIL] /api/repos returned $code, expected 401"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:7420/api/repos")
[[ $code == 200 ]] && echo "[ok  ] relay accepts the token (200)" \
	|| { echo "[FAIL] tokened /api/repos returned $code"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 "https://$DOMAIN/" || true)
[[ $code == 200 ]] && echo "[ok  ] https://$DOMAIN serves the dashboard shell" \
	|| echo "[warn] https://$DOMAIN returned '$code' — DNS may not have propagated yet; Caddy retries on its own"

cat <<OUT

relay is up.

  enroll a repo:  coord init --relay wss://$DOMAIN/ws
  each teammate:  coord login --relay wss://$DOMAIN/ws --token $TOKEN
  dashboard:      https://$DOMAIN/?token=$TOKEN

  logs:           journalctl -u coord-relay -f
  redeploy:       bash $HERE/provision.sh
OUT
