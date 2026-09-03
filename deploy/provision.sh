#!/usr/bin/env bash
# Provision a droplet to host the knoot relay behind TLS.
#
#   scp -r deploy root@<ip>:/root/ && ssh root@<ip> 'bash /root/deploy/provision.sh'
#
# Idempotent: re-run it to deploy a new revision. Env overrides:
#   DOMAIN=relay.knoot.dev     hostname agents enrol against
#   APEX=knoot.dev             hostname serving the site and console
#   SOURCE=release|build       download the CI binary (default) or compile here
#   REF=main                   revision, when SOURCE=build
#   REPO=https://github.com/Ash20pk/knoot.git
set -euo pipefail

DOMAIN="${DOMAIN:-relay.knoot.dev}"
APEX="${APEX:-knoot.dev}"
SOURCE="${SOURCE:-release}"
REF="${REF:-main}"
REPO="${REPO:-https://github.com/Ash20pk/knoot.git}"
RELEASE_URL="${RELEASE_URL:-https://github.com/Ash20pk/knoot/releases/download/nightly}"
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
	curl -fsSL -o "$tmp/knoot" "$RELEASE_URL/knoot-x86_64-linux"
	curl -fsSL -o "$tmp/knoot.sha256" "$RELEASE_URL/knoot-x86_64-linux.sha256"
	# The checksum is published by the same run that built the binary, so this
	# catches a truncated download rather than a malicious one — worth having
	# for the former, and not claimed to protect against the latter.
	want="$(awk '{print $1}' "$tmp/knoot.sha256")"
	got="$(sha256sum "$tmp/knoot" | awk '{print $1}')"
	[[ "$want" == "$got" ]] || { echo "checksum mismatch: $got != $want" >&2; exit 1; }
	chmod 0755 "$tmp/knoot"
	"$tmp/knoot" --version
	install -m 0755 "$tmp/knoot" /usr/local/bin/knoot.new
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
	if [[ -d /opt/knoot/.git ]]; then
		git -C /opt/knoot fetch --quiet origin "$REF"
		git -C /opt/knoot checkout --quiet -B deploy "origin/$REF"
	else
		git clone --quiet "$REPO" /opt/knoot
		git -C /opt/knoot checkout --quiet -B deploy "origin/$REF"
	fi
	echo "   $(git -C /opt/knoot rev-parse --short HEAD)  $(git -C /opt/knoot log -1 --format=%s)"
	# One job at a time: parallel rustc on 1 vCPU only competes for the memory
	# the linker is about to need.
	CARGO_BUILD_JOBS=1 cargo build --release --manifest-path /opt/knoot/Cargo.toml
	install -m 0755 /opt/knoot/target/release/knoot /usr/local/bin/knoot.new
fi

# ---------------------------------------------------------------------------
# Migration from the old name. Runs once and is a no-op thereafter.
#
# The operator token is the thing that must survive: it is not stored anywhere
# else, and losing it does not stop the relay — it starts an *open* one. The
# database moves with it, because it holds every team's hashed tokens and the
# whole event log.
# ---------------------------------------------------------------------------
if [[ -d /var/lib/coord || -f /etc/coord/relay.env || -f /etc/systemd/system/coord-relay.service ]]; then
	say "migrating from coord"
	systemctl stop coord-relay.service coord-snapshot.timer 2>/dev/null || true
	systemctl disable coord-relay.service coord-snapshot.timer 2>/dev/null || true
	rm -f /etc/systemd/system/coord-relay.service \
		/etc/systemd/system/coord-snapshot.service \
		/etc/systemd/system/coord-snapshot.timer \
		/usr/local/bin/coord-snapshot
	systemctl daemon-reload

	id -u knoot >/dev/null 2>&1 || useradd --system --home /var/lib/knoot --shell /usr/sbin/nologin knoot
	if [[ -d /var/lib/coord && ! -d /var/lib/knoot ]]; then
		mv /var/lib/coord /var/lib/knoot
		chown -R knoot:knoot /var/lib/knoot
		echo "   moved the event log to /var/lib/knoot ($(du -sh /var/lib/knoot | cut -f1))"
	fi

	install -d -m 0755 /etc/knoot
	for f in relay.env litestream.env; do
		if [[ -f /etc/coord/$f && ! -f /etc/knoot/$f ]]; then
			# Rename the variables inside as well as the file around them.
			sed 's/^COORD_/KNOOT_/' "/etc/coord/$f" > "/etc/knoot/$f"
			chmod 0600 "/etc/knoot/$f"
			echo "   carried over /etc/coord/$f (token preserved)"
		fi
	done
	# Left in place rather than deleted: if anything here was wrong, the old
	# files are the only copy of a token nobody wrote down.
	[[ -d /etc/coord ]] && echo "   /etc/coord left in place — remove it once you are satisfied"
	rm -f /usr/local/bin/coord
fi

say "user + state"
id -u knoot >/dev/null 2>&1 || useradd --system --home /var/lib/knoot --shell /usr/sbin/nologin knoot
install -d -o knoot -g knoot -m 0750 /var/lib/knoot
install -d -o knoot -g knoot -m 0750 /var/lib/knoot/snapshots

say "token"
install -d -m 0755 /etc/knoot
# Console sign-in is optional. Say so plainly when it is off, the same way
# Litestream does, so "sign-in is not configured" is never a mystery.
if [[ -s /etc/knoot/supabase.env ]]; then
	chmod 0600 /etc/knoot/supabase.env
	say "console sign-in configured (/etc/knoot/supabase.env)"
else
	say "console sign-in OFF — no /etc/knoot/supabase.env; agent tokens still work"
fi

if ! [[ -s /etc/knoot/relay.env ]]; then
	printf 'KNOOT_RELAY_TOKEN=%s\n' "$(openssl rand -hex 24)" > /etc/knoot/relay.env
	chmod 0600 /etc/knoot/relay.env
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
#               once /etc/knoot/litestream.env exists.
# ---------------------------------------------------------------------------
say "snapshots (nightly, on-box, 7 kept)"
cat > /usr/local/bin/knoot-snapshot <<'SNAP'
#!/usr/bin/env bash
# A consistent copy of a live SQLite database: `.backup` takes a read lock and
# copies pages, which `cp` does not, and a half-copied WAL database restores as
# a corrupt one.
set -euo pipefail
DB=/var/lib/knoot/relay.db
DIR=/var/lib/knoot/snapshots
[[ -f $DB ]] || exit 0
out="$DIR/relay-$(date -u +%Y%m%dT%H%M%SZ).db"
sqlite3 "$DB" ".backup '$out'"
gzip -f "$out"
ls -1t "$DIR"/relay-*.db.gz 2>/dev/null | tail -n +8 | xargs -r rm --
SNAP
chmod 0755 /usr/local/bin/knoot-snapshot

cat > /etc/systemd/system/knoot-snapshot.service <<'UNIT'
[Unit]
Description=Snapshot the knoot event log
[Service]
Type=oneshot
User=knoot
Group=knoot
ExecStart=/usr/local/bin/knoot-snapshot
UNIT

cat > /etc/systemd/system/knoot-snapshot.timer <<'UNIT'
[Unit]
Description=Nightly knoot event log snapshot
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

if [[ -s /etc/knoot/litestream.env ]]; then
	# shellcheck disable=SC1091
	set -a; . /etc/knoot/litestream.env; set +a
	: "${LITESTREAM_BUCKET:?set LITESTREAM_BUCKET in /etc/knoot/litestream.env}"
	cat > /etc/litestream.yml <<YML
# Generated by deploy/provision.sh — edit /etc/knoot/litestream.env instead.
dbs:
  - path: /var/lib/knoot/relay.db
    replicas:
      - type: s3
        bucket: ${LITESTREAM_BUCKET}
        path: ${LITESTREAM_PATH:-knoot/relay.db}
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
EnvironmentFile=/etc/knoot/litestream.env
UNIT
	# Restore before the relay starts, but only into an empty state directory:
	# an existing database is the authority, and clobbering it with a replica
	# would be the backup destroying the thing it protects.
	if ! [[ -f /var/lib/knoot/relay.db ]]; then
		echo "   no local database — restoring from the replica"
		litestream restore -if-replica-exists -o /var/lib/knoot/relay.db /var/lib/knoot/relay.db || true
		chown knoot:knoot /var/lib/knoot/relay.db 2>/dev/null || true
	fi
	systemctl daemon-reload
	systemctl enable --quiet litestream
	systemctl restart litestream
	echo "   replicating to ${LITESTREAM_BUCKET}/${LITESTREAM_PATH:-knoot/relay.db}"
else
	systemctl disable --quiet litestream 2>/dev/null || true
	systemctl stop litestream 2>/dev/null || true
	cat <<'OFF'
   installed but OFF — no /etc/knoot/litestream.env.
   Nightly on-box snapshots still run; losing the droplet would still lose the log.
   To enable, create a DigitalOcean Space and write:

     cat > /etc/knoot/litestream.env <<'EOF'
     LITESTREAM_BUCKET=your-space-name
     LITESTREAM_ENDPOINT=https://fra1.digitaloceanspaces.com
     LITESTREAM_REGION=fra1
     LITESTREAM_ACCESS_KEY_ID=...
     LITESTREAM_SECRET_ACCESS_KEY=...
     EOF
     chmod 600 /etc/knoot/litestream.env

   then re-run this script.
OFF
fi

say "systemd"
install -m 0644 "$HERE/knoot-relay.service" /etc/systemd/system/knoot-relay.service
systemctl daemon-reload
systemctl enable --quiet knoot-relay knoot-snapshot.timer
# Swap the binary in only now that everything around it is in place, so a
# failed download or a bad checksum leaves the running version untouched.
mv /usr/local/bin/knoot.new /usr/local/bin/knoot
systemctl restart knoot-relay
systemctl start knoot-snapshot.timer

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
TOKEN="$(sed -n 's/^KNOOT_RELAY_TOKEN=//p' /etc/knoot/relay.env)"
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
mode=$(sqlite3 /var/lib/knoot/relay.db "PRAGMA journal_mode;" 2>/dev/null || echo none)
[[ $mode == wal ]] && echo "[ok  ] event log is in WAL mode (replicable)" \
	|| echo "[warn] journal_mode is '$mode' — continuous replication would do nothing"

sudo -u knoot /usr/local/bin/knoot-snapshot \
	&& echo "[ok  ] snapshot taken ($(ls -1 /var/lib/knoot/snapshots | wc -l | tr -d ' ') kept)" \
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
  enroll a repo:  knoot init --relay wss://$DOMAIN/ws
  each teammate:  knoot login --relay wss://$DOMAIN/ws --token <team token from the console>

  operator token: $TOKEN
  logs:           journalctl -u knoot-relay -f
  snapshots:      /var/lib/knoot/snapshots
  redeploy:       bash $HERE/provision.sh
OUT
