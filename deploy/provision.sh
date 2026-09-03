#!/usr/bin/env bash
# Provision a droplet to host the coord relay behind TLS.
#
#   scp -r deploy root@<ip>:/root/ && ssh root@<ip> 'bash /root/deploy/provision.sh'
#
# Idempotent: re-run it to deploy a new revision. Env overrides:
#   DOMAIN=relay.knoot.dev     hostname agents enrol against
#   APEX=knoot.dev             hostname serving the site and console
#   SOURCE=release|build       download the CI binary (default) or compile here
#   REF=main                   revision, when SOURCE=build
#   REPO=https://github.com/Ash20pk/coord.git
set -euo pipefail

DOMAIN="${DOMAIN:-relay.knoot.dev}"
APEX="${APEX:-knoot.dev}"
SOURCE="${SOURCE:-release}"
REF="${REF:-main}"
REPO="${REPO:-https://github.com/Ash20pk/coord.git}"
RELEASE_URL="${RELEASE_URL:-https://github.com/Ash20pk/coord/releases/download/nightly}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
say() { printf '\n== %s\n' "$*"; }

say "packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq git curl ca-certificates sqlite3 \
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

# ---------------------------------------------------------------------------
# The binary. Downloading beats building: this box is 1 vCPU / 961 MB, where a
# release build with LTO needs a swapfile to link at all and competes for
# memory with the relay it is replacing. `SOURCE=build` is kept for hacking on
# the box, or if the release is ever unavailable.
# ---------------------------------------------------------------------------
if [[ "$SOURCE" == "release" ]]; then
	say "binary (prebuilt, from CI)"
	tmp="$(mktemp -d)"
	curl -fsSL -o "$tmp/coord" "$RELEASE_URL/coord-x86_64-linux"
	curl -fsSL -o "$tmp/coord.sha256" "$RELEASE_URL/coord-x86_64-linux.sha256"
	# The checksum is published by the same run that built the binary, so this
	# catches a truncated download rather than a malicious one — worth having
	# for the former, and not claimed to protect against the latter.
	want="$(awk '{print $1}' "$tmp/coord.sha256")"
	got="$(sha256sum "$tmp/coord" | awk '{print $1}')"
	[[ "$want" == "$got" ]] || { echo "checksum mismatch: $got != $want" >&2; exit 1; }
	chmod 0755 "$tmp/coord"
	"$tmp/coord" --version
	install -m 0755 "$tmp/coord" /usr/local/bin/coord.new
	rm -rf "$tmp"
else
	say "swap (a 1 GB box cannot link this without it)"
	if ! swapon --show --noheadings | grep -q .; then
		fallocate -l 2G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=2048 status=none
		chmod 600 /swapfile
		mkswap -q /swapfile
		swapon /swapfile
		grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
		echo "   2G swapfile on, and in fstab"
	else
		echo "   already has swap"
	fi

	say "rust"
	apt-get install -y -qq build-essential pkg-config
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
	# One job at a time: parallel rustc on 1 vCPU only competes for the memory
	# the linker is about to need.
	CARGO_BUILD_JOBS=1 cargo build --release --manifest-path /opt/coord/Cargo.toml
	install -m 0755 /opt/coord/target/release/coord /usr/local/bin/coord.new
fi

say "user + state"
id -u coord >/dev/null 2>&1 || useradd --system --home /var/lib/coord --shell /usr/sbin/nologin coord
install -d -o coord -g coord -m 0750 /var/lib/coord
install -d -o coord -g coord -m 0750 /var/lib/coord/snapshots

say "token"
install -d -m 0755 /etc/coord
if ! [[ -s /etc/coord/relay.env ]]; then
	printf 'COORD_RELAY_TOKEN=%s\n' "$(openssl rand -hex 24)" > /etc/coord/relay.env
	chmod 0600 /etc/coord/relay.env
	echo "   generated a new token"
else
	echo "   keeping the existing token"
fi

# ---------------------------------------------------------------------------
# Backups. Two layers, because they fail differently:
#
#   snapshots — a nightly `.backup` on the box, 7 kept. Covers the thing that
#               actually happens: a bad DELETE, a corrupted page, a mistake.
#   litestream — continuous replication off the box. Covers losing the droplet.
#               Needs object-storage credentials, so it configures itself only
#               once /etc/coord/litestream.env exists.
# ---------------------------------------------------------------------------
say "snapshots (nightly, on-box, 7 kept)"
cat > /usr/local/bin/coord-snapshot <<'SNAP'
#!/usr/bin/env bash
# A consistent copy of a live SQLite database: `.backup` takes a read lock and
# copies pages, which `cp` does not, and a half-copied WAL database restores as
# a corrupt one.
set -euo pipefail
DB=/var/lib/coord/relay.db
DIR=/var/lib/coord/snapshots
[[ -f $DB ]] || exit 0
out="$DIR/relay-$(date -u +%Y%m%dT%H%M%SZ).db"
sqlite3 "$DB" ".backup '$out'"
gzip -f "$out"
ls -1t "$DIR"/relay-*.db.gz 2>/dev/null | tail -n +8 | xargs -r rm --
SNAP
chmod 0755 /usr/local/bin/coord-snapshot

cat > /etc/systemd/system/coord-snapshot.service <<'UNIT'
[Unit]
Description=Snapshot the coord event log
[Service]
Type=oneshot
User=coord
Group=coord
ExecStart=/usr/local/bin/coord-snapshot
UNIT

cat > /etc/systemd/system/coord-snapshot.timer <<'UNIT'
[Unit]
Description=Nightly coord event log snapshot
[Timer]
OnCalendar=daily
RandomizedDelaySec=30m
Persistent=true
[Install]
WantedBy=timers.target
UNIT

say "litestream (continuous off-box replication)"
if ! command -v litestream >/dev/null; then
	arch=$(dpkg --print-architecture)
	ver=0.3.13
	curl -fsSL -o /tmp/litestream.deb \
		"https://github.com/benbjohnson/litestream/releases/download/v${ver}/litestream-v${ver}-linux-${arch}.deb"
	dpkg -i /tmp/litestream.deb >/dev/null
	rm -f /tmp/litestream.deb
fi

if [[ -s /etc/coord/litestream.env ]]; then
	# shellcheck disable=SC1091
	set -a; . /etc/coord/litestream.env; set +a
	: "${LITESTREAM_BUCKET:?set LITESTREAM_BUCKET in /etc/coord/litestream.env}"
	cat > /etc/litestream.yml <<YML
# Generated by deploy/provision.sh — edit /etc/coord/litestream.env instead.
dbs:
  - path: /var/lib/coord/relay.db
    replicas:
      - type: s3
        bucket: ${LITESTREAM_BUCKET}
        path: ${LITESTREAM_PATH:-coord/relay.db}
        endpoint: ${LITESTREAM_ENDPOINT:-}
        region: ${LITESTREAM_REGION:-us-east-1}
        # Ten seconds of loss on a total-loss event, and a full snapshot a day
        # so a restore never has to replay a month of WAL.
        sync-interval: 10s
        snapshot-interval: 24h
        retention: 168h
YML
	chmod 0644 /etc/litestream.yml
	mkdir -p /etc/systemd/system/litestream.service.d
	cat > /etc/systemd/system/litestream.service.d/override.conf <<'UNIT'
[Service]
EnvironmentFile=/etc/coord/litestream.env
UNIT
	# Restore before the relay starts, but only into an empty state directory:
	# an existing database is the authority, and clobbering it with a replica
	# would be the backup destroying the thing it protects.
	if ! [[ -f /var/lib/coord/relay.db ]]; then
		echo "   no local database — restoring from the replica"
		litestream restore -if-replica-exists -o /var/lib/coord/relay.db /var/lib/coord/relay.db || true
		chown coord:coord /var/lib/coord/relay.db 2>/dev/null || true
	fi
	systemctl daemon-reload
	systemctl enable --quiet litestream
	systemctl restart litestream
	echo "   replicating to ${LITESTREAM_BUCKET}/${LITESTREAM_PATH:-coord/relay.db}"
else
	systemctl disable --quiet litestream 2>/dev/null || true
	systemctl stop litestream 2>/dev/null || true
	cat <<'OFF'
   installed but OFF — no /etc/coord/litestream.env.
   Nightly on-box snapshots still run; losing the droplet would still lose the log.
   To enable, create a DigitalOcean Space and write:

     cat > /etc/coord/litestream.env <<'EOF'
     LITESTREAM_BUCKET=your-space-name
     LITESTREAM_ENDPOINT=https://fra1.digitaloceanspaces.com
     LITESTREAM_REGION=fra1
     LITESTREAM_ACCESS_KEY_ID=...
     LITESTREAM_SECRET_ACCESS_KEY=...
     EOF
     chmod 600 /etc/coord/litestream.env

   then re-run this script.
OFF
fi

say "systemd"
install -m 0644 "$HERE/coord-relay.service" /etc/systemd/system/coord-relay.service
systemctl daemon-reload
systemctl enable --quiet coord-relay coord-snapshot.timer
# Swap the binary in only now that everything around it is in place, so a
# failed download or a bad checksum leaves the running version untouched.
mv /usr/local/bin/coord.new /usr/local/bin/coord
systemctl restart coord-relay
systemctl start coord-snapshot.timer

say "caddy"
# Order matters: the longer name is substituted first, or `relay.knoot.dev`
# would be rewritten to `relay.<apex>` by the second rule.
sed -e "s/relay\.knoot\.dev/$DOMAIN/g" -e "s/knoot\.dev/$APEX/g" \
	"$HERE/Caddyfile" > /etc/caddy/Caddyfile
caddy fmt --overwrite /etc/caddy/Caddyfile 2>/dev/null || true
caddy validate --config /etc/caddy/Caddyfile 2>&1 | tail -3
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
fail() { echo "[FAIL] $1"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:7420/api/repos")
[[ $code == 401 ]] && echo "[ok  ] relay refuses an untokened request (401)" \
	|| fail "/api/repos returned $code, expected 401"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:7420/api/repos")
[[ $code == 200 ]] && echo "[ok  ] relay accepts the token (200)" \
	|| fail "tokened /api/repos returned $code"
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:7420/api/register" -X POST \
	-H 'Content-Type: application/json' -d '{"team":""}')
[[ $code == 400 ]] && echo "[ok  ] registration validates its input (400 on empty)" \
	|| fail "/api/register on an empty name returned $code, expected 400"

# WAL is what makes the log replicable; without it litestream copies nothing
# and reports success.
mode=$(sqlite3 /var/lib/coord/relay.db "PRAGMA journal_mode;" 2>/dev/null || echo none)
[[ $mode == wal ]] && echo "[ok  ] event log is in WAL mode (replicable)" \
	|| echo "[warn] journal_mode is '$mode' — continuous replication would do nothing"

sudo -u coord /usr/local/bin/coord-snapshot \
	&& echo "[ok  ] snapshot taken ($(ls -1 /var/lib/coord/snapshots | wc -l | tr -d ' ') kept)" \
	|| echo "[warn] snapshot failed"

if systemctl is-active --quiet litestream; then
	echo "[ok  ] litestream replicating off-box"
else
	echo "[warn] litestream is off — on-box snapshots only (see above)"
fi

for host in "$DOMAIN" "$APEX"; do
	code=$(curl -s -o /dev/null -m 20 -w '%{http_code}' "https://$host/" || true)
	[[ $code == 200 ]] && echo "[ok  ] https://$host serves the site" \
		|| echo "[warn] https://$host returned '$code' — DNS may not point here yet; Caddy retries on its own"
done

cat <<OUT

relay is up.

  site:           https://$APEX
  console:        https://$APEX/app
  enroll a repo:  coord init --relay wss://$DOMAIN/ws
  each teammate:  coord login --relay wss://$DOMAIN/ws --token <team token from the console>

  operator token: $TOKEN
  logs:           journalctl -u coord-relay -f
  snapshots:      /var/lib/coord/snapshots
  redeploy:       bash $HERE/provision.sh
OUT
