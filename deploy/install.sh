#!/usr/bin/env bash
# Install or upgrade a prebuilt Ponytail Risk Linux bundle.
set -euo pipefail

info() { printf '==> %s\n' "$1"; }
fail() { printf 'ERROR: %s\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"; }

bundle_url="${PGR_BUNDLE_URL:-}"
bundle_file=""
bundle_sha256="${PGR_BUNDLE_SHA256:-}"
bundle_signature_url="${PGR_BUNDLE_SIGNATURE_URL:-}"
bundle_signature_file=""
release_public_key_file=""
release_public_key_b64="${PGR_RELEASE_PUBLIC_KEY_B64:-}"
allow_unsigned_lab="${PGR_ALLOW_UNSIGNED_LAB:-0}"
check_only=0

usage() {
  cat <<'EOF'
Usage:
  sudo bash install.sh
  sudo bash install.sh --bundle-url HTTPS_URL --sha256 SHA256
  bash install.sh --bundle-file FILE --sha256 SHA256 --signature-file FILE.sig --public-key-file release-public-key.pem --check-only

Optional environment:
  PGR_PORT=4173                 Portal port
  PGR_BIND=127.0.0.1            Portal bind address; expose through an HTTPS reverse proxy
  PGR_INSTANCE=ponytail-risk    Instance/service name
  PGR_PORTAL_KEY=...            First-install portal key; generated when omitted
  PGR_TENANT_ID=...             Agent tenant id
  PGR_SERVER_ID=...             Agent server id
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --bundle-url) [ "$#" -ge 2 ] || fail "--bundle-url needs a value"; bundle_url="$2"; bundle_file=""; shift 2 ;;
    --bundle-file) [ "$#" -ge 2 ] || fail "--bundle-file needs a value"; bundle_file="$2"; bundle_url=""; shift 2 ;;
    --sha256) [ "$#" -ge 2 ] || fail "--sha256 needs a value"; bundle_sha256="$2"; shift 2 ;;
    --signature-url) [ "$#" -ge 2 ] || fail "--signature-url needs a value"; bundle_signature_url="$2"; shift 2 ;;
    --signature-file) [ "$#" -ge 2 ] || fail "--signature-file needs a value"; bundle_signature_file="$2"; shift 2 ;;
    --public-key-file) [ "$#" -ge 2 ] || fail "--public-key-file needs a value"; release_public_key_file="$2"; shift 2 ;;
    --allow-unsigned-lab) allow_unsigned_lab=1; shift ;;
    --check-only) check_only=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) fail "Unknown option: $1" ;;
  esac
done

[ -z "$bundle_url" ] || [ -z "$bundle_file" ] || fail "Use either --bundle-url or --bundle-file"
[ -n "$bundle_url" ] || [ -n "$bundle_file" ] || fail "No release bundle configured"
[[ "$bundle_sha256" =~ ^[0-9A-Fa-f]{64}$ ]] || fail "A valid SHA-256 is required"

need tar
need sha256sum
need mktemp
need wc

tmp_dir="$(mktemp -d /tmp/ponytail-risk-install.XXXXXX)"
cleanup() { rm -rf -- "$tmp_dir"; }
trap cleanup EXIT
archive="$tmp_dir/release.tar.gz"

if [ -n "$bundle_file" ]; then
  [ -f "$bundle_file" ] || fail "Bundle file not found: $bundle_file"
  cp -- "$bundle_file" "$archive"
else
  case "$bundle_url" in https://*) ;; *) fail "Bundle URL must use HTTPS" ;; esac
  need curl
  info "Downloading release bundle"
  curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error "$bundle_url" --output "$archive"
fi

actual_sha256="$(sha256sum "$archive" | awk '{print $1}')"
[ "$actual_sha256" = "${bundle_sha256,,}" ] || fail "Release bundle SHA-256 mismatch"

if [ "$allow_unsigned_lab" -ne 1 ]; then
  need openssl
  signature="$tmp_dir/release.sig"
  public_key="$tmp_dir/release-public-key.pem"
  if [ -n "$bundle_file" ]; then
    if [ -z "$bundle_signature_file" ] && [ -f "$bundle_file.sig" ]; then bundle_signature_file="$bundle_file.sig"; fi
    [ -f "$bundle_signature_file" ] || fail "A detached signature file is required"
    cp -- "$bundle_signature_file" "$signature"
    if [ -n "$release_public_key_file" ]; then
      [ -f "$release_public_key_file" ] || fail "Release public key file not found"
      cp -- "$release_public_key_file" "$public_key"
    elif [ -n "$release_public_key_b64" ]; then
      need base64
      printf '%s' "$release_public_key_b64" | base64 -d >"$public_key" || fail "Pinned release public key is invalid"
    else
      fail "A pinned release public key is required"
    fi
  else
    case "$bundle_signature_url" in https://*) ;; *) fail "Bundle signature URL must use HTTPS" ;; esac
    [ -n "$release_public_key_b64" ] || fail "The installer has no pinned release public key"
    curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error "$bundle_signature_url" --output "$signature"
    need base64
    printf '%s' "$release_public_key_b64" | base64 -d >"$public_key" || fail "Pinned release public key is invalid"
  fi
  [ "$(wc -c <"$signature")" -le 16384 ] || fail "Release signature is too large"
  openssl pkey -pubin -in "$public_key" -noout >/dev/null 2>&1 || fail "Release public key is invalid"
  openssl dgst -sha256 -verify "$public_key" -signature "$signature" "$archive" >/dev/null 2>&1 || fail "Release bundle signature verification failed"
else
  info "Unsigned lab bundle explicitly allowed; do not use this mode for customers"
fi

# Reject path traversal and special archive entries before extracting as root.
if tar -tzf "$archive" | LC_ALL=C grep -Eq '(^/|(^|/)\.\.(/|$))'; then
  fail "Release bundle contains an unsafe path"
fi
if tar -tvzf "$archive" | awk '$1 ~ /^[lhbcps]/ { bad=1 } END { exit bad ? 0 : 1 }'; then
  fail "Release bundle contains links or special files"
fi

unpack_dir="$tmp_dir/unpack"
mkdir -p "$unpack_dir"
tar -xzf "$archive" -C "$unpack_dir" --no-same-owner --no-same-permissions
bundle_root="$unpack_dir/ponytail-risk"
[ -d "$bundle_root" ] || fail "Release bundle root is missing"

for path in VERSION TARGET_ARCH SHA256SUMS server.mjs public/app.html runtime/bin/node bin/risk-live-data bin/risk-probe bin/risk-agent lib/librisk_sdk.so; do
  [ -e "$bundle_root/$path" ] || fail "Release bundle is missing: $path"
done

manifest_result="$(cd "$bundle_root" && sha256sum -c SHA256SUMS 2>&1)" || fail "Bundle manifest failed: $manifest_result"
[ "$(cat "$bundle_root/TARGET_ARCH")" = "x86_64" ] || fail "Unsupported release architecture"
[ "$(uname -m)" = "x86_64" ] || fail "This release requires Linux x86_64"
chmod 0755 "$bundle_root/runtime/bin/node" "$bundle_root/bin/risk-live-data" "$bundle_root/bin/risk-probe" "$bundle_root/bin/risk-agent"

node_major="$($bundle_root/runtime/bin/node -p 'process.versions.node.split(".")[0]')"
[ "$node_major" -ge 18 ] || fail "Bundled Node.js is older than 18"
"$bundle_root/bin/risk-live-data" self-check >/dev/null
"$bundle_root/bin/risk-agent" self-check >/dev/null
info "Release bundle verified: $(cat "$bundle_root/VERSION")"

if [ "$check_only" -eq 1 ]; then
  info "Check-only mode completed; no system files changed"
  exit 0
fi

[ "$(id -u)" -eq 0 ] || fail "Run the installer as root (sudo bash ...)"
need systemctl
need getent
need curl

instance="${PGR_INSTANCE:-ponytail-risk}"
[[ "$instance" =~ ^[a-z0-9][a-z0-9-]{0,31}$ ]] || fail "PGR_INSTANCE is invalid"
portal_port="${PGR_PORT:-4173}"
agent_port="${PGR_AGENT_PORT:-17870}"
[[ "$portal_port" =~ ^[0-9]+$ ]] && [ "$portal_port" -ge 1 ] && [ "$portal_port" -le 65535 ] || fail "PGR_PORT is invalid"
[[ "$agent_port" =~ ^[0-9]+$ ]] && [ "$agent_port" -ge 1 ] && [ "$agent_port" -le 65535 ] || fail "PGR_AGENT_PORT is invalid"
[ "$portal_port" != "$agent_port" ] || fail "Portal and Agent ports must differ"
portal_bind="${PGR_BIND:-127.0.0.1}"
case "$portal_bind" in 0.0.0.0|127.0.0.1|::|::1) ;; *) fail "PGR_BIND is invalid" ;; esac

tenant_id="${PGR_TENANT_ID:-tenant-$(hostname | tr -cd 'A-Za-z0-9._-' | cut -c1-40)}"
server_id="${PGR_SERVER_ID:-server-1}"
[[ "$tenant_id" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || fail "PGR_TENANT_ID is invalid"
[[ "$server_id" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || fail "PGR_SERVER_ID is invalid"

install_root="/opt/$instance"
config_root="/etc/$instance"
data_root="/var/lib/$instance"
portal_env="$config_root/portal.env"
agent_env="$config_root/agent.env"
portal_unit="$instance.service"
agent_unit="$instance-agent.service"

service_user="${PGR_SERVICE_USER:-}"
if [ -z "$service_user" ] && [ -f "/etc/systemd/system/$portal_unit" ]; then
  service_user="$(sed -n 's/^User=//p' "/etc/systemd/system/$portal_unit" | head -n 1)"
fi
[ -n "$service_user" ] || service_user="$instance"
[[ "$service_user" =~ ^[a-z_][a-z0-9_-]{0,31}$ ]] || fail "PGR_SERVICE_USER is invalid"
if ! getent group "$service_user" >/dev/null 2>&1; then groupadd --system "$service_user"; fi
if ! id -u "$service_user" >/dev/null 2>&1; then
  nologin_shell="$(command -v nologin || printf '/bin/false')"
  useradd --system --gid "$service_user" --home-dir "$data_root" --shell "$nologin_shell" "$service_user"
fi

install -d -o root -g root -m 0755 "$install_root" "$install_root/releases" "$config_root"
install -d -o "$service_user" -g "$service_user" -m 0750 "$data_root"
chown -R "$service_user:$service_user" "$data_root"

random_hex() { od -An -N "$1" -tx1 /dev/urandom | tr -d ' \n'; }
new_credentials=0
if [ ! -f "$portal_env" ]; then
  portal_key="${PGR_PORTAL_KEY:-PONYTAIL-$(random_hex 2)-$(random_hex 2)-$(random_hex 2)-$(random_hex 2)}"
  agent_token="$(random_hex 32)"
  config_master_key="$(random_hex 32)"
  umask 077
  cat >"$portal_env" <<EOF
RISK_PORTAL_KEY=$portal_key
RISK_CONFIG_MASTER_KEY=$config_master_key
RISK_PORT=$portal_port
RISK_HOST=$portal_bind
RISK_ENGINE=$install_root/current/bin/risk-live-data
PGR_AGENT_PORT=$agent_port
PGR_AGENT_LOCAL_TOKEN=$agent_token
RISK_DB_CONFIG_PATH=$data_root/database-connection.enc.json
RISK_AI_CONFIG_PATH=$data_root/ai-provider.enc.json
RISK_AI_REVIEWS_PATH=$data_root/ai-reviews.json
RISK_SDK_KEYS_PATH=$data_root/sdk-credentials.json
RISK_CASE_ACTIONS_PATH=$data_root/case-actions.json
RISK_GAMEPLAY_CAPS_PATH=$data_root/gameplay-caps.json
EOF
  chmod 0600 "$portal_env"
  new_credentials=1
else
  agent_token="$(sed -n 's/^PGR_AGENT_LOCAL_TOKEN=//p' "$portal_env" | head -n 1)"
  [[ "$agent_token" =~ ^[0-9a-f]{64}$ ]] || fail "Existing portal.env has no valid Agent token"
  config_master_key="$(sed -n 's/^RISK_CONFIG_MASTER_KEY=//p' "$portal_env" | head -n 1)"
  if [ -z "$config_master_key" ]; then
    config_master_key="$(random_hex 32)"
    printf 'RISK_CONFIG_MASTER_KEY=%s\n' "$config_master_key" >>"$portal_env"
  fi
  [[ "$config_master_key" =~ ^[0-9a-f]{64}$ ]] || fail "Existing portal.env has no valid config master key"
  info "Existing portal configuration and key preserved"
fi

if [ ! -f "$agent_env" ]; then
  umask 077
  cat >"$agent_env" <<EOF
PGR_TENANT_ID=$tenant_id
PGR_SERVER_ID=$server_id
PGR_LOCAL_TOKEN=$agent_token
PGR_AGENT_PORT=$agent_port
PGR_QUEUE_DB=$data_root/plugin-events.db
PGR_MODE=shadow
PGR_GOLD_GAIN_10M=1000000
PGR_ASSET_MOVES_10M=5
PGR_HIGH_VALUE_GOLD=1000000
PGR_HIGH_VALUE_ASSET_QUANTITY=20
EOF
  chmod 0600 "$agent_env"
else
  info "Existing Agent configuration preserved"
fi

version="$(tr -cd 'A-Za-z0-9._+-' <"$bundle_root/VERSION")"
[ -n "$version" ] || fail "Release version is invalid"
release_id="$version-$(date -u +%Y%m%d%H%M%S)"
release_dir="$install_root/releases/$release_id"
[ ! -e "$release_dir" ] || fail "Release directory already exists: $release_dir"
mkdir -p "$release_dir"
cp -a "$bundle_root/." "$release_dir/"
ln -s "$data_root" "$release_dir/data"
chown -R root:root "$release_dir"
chmod 0755 "$release_dir/runtime/bin/node" "$release_dir/bin/risk-live-data" "$release_dir/bin/risk-probe" "$release_dir/bin/risk-agent"

previous_release=""
if [ -L "$install_root/current" ]; then previous_release="$(readlink -f "$install_root/current")"; fi
next_link="$install_root/.current.$$"
ln -s "$release_dir" "$next_link"
mv -Tf "$next_link" "$install_root/current"

cat >"/etc/systemd/system/$agent_unit" <<EOF
[Unit]
Description=Ponytail Risk Plugin Agent ($instance)
After=network.target

[Service]
Type=simple
User=$service_user
Group=$service_user
WorkingDirectory=$install_root/current
EnvironmentFile=$agent_env
ExecStart=$install_root/current/bin/risk-agent serve
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
PrivateDevices=true
ProtectHome=true
ProtectSystem=full
CapabilityBoundingSet=
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
UMask=0027
LimitNOFILE=4096

[Install]
WantedBy=multi-user.target
EOF

cat >"/etc/systemd/system/$portal_unit" <<EOF
[Unit]
Description=Ponytail Risk Portal ($instance)
After=network-online.target $agent_unit
Wants=network-online.target

[Service]
Type=simple
User=$service_user
Group=$service_user
WorkingDirectory=$install_root/current
EnvironmentFile=$portal_env
Environment=NODE_ENV=production
ExecStart=$install_root/current/runtime/bin/node $install_root/current/server.mjs
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
PrivateDevices=true
ProtectHome=true
ProtectSystem=full
CapabilityBoundingSet=
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
UMask=0027
LimitNOFILE=8192

[Install]
WantedBy=multi-user.target
EOF

rollback() {
  if [ -n "$previous_release" ] && [ -d "$previous_release" ]; then
    rollback_link="$install_root/.rollback.$$"
    ln -s "$previous_release" "$rollback_link"
    mv -Tf "$rollback_link" "$install_root/current"
    systemctl restart "$agent_unit" "$portal_unit" >/dev/null 2>&1 || true
    if wait_service_http "$agent_unit" "http://127.0.0.1:$agent_port/agent/v1/health" && wait_service_http "$portal_unit" "http://127.0.0.1:$portal_port/"; then
      printf 'Rolled back and healthy: %s\n' "$previous_release" >&2
    else
      printf 'Rollback restored %s, but its health check is still pending\n' "$previous_release" >&2
    fi
  else
    systemctl stop "$portal_unit" "$agent_unit" >/dev/null 2>&1 || true
  fi
}

systemctl daemon-reload
systemctl enable "$agent_unit" "$portal_unit" >/dev/null
systemctl restart "$agent_unit"
systemctl restart "$portal_unit"

wait_service_http() {
  unit="$1"
  url="$2"
  i=0
  while [ "$i" -lt 30 ]; do
    if systemctl is-active --quiet "$unit" && curl --fail --silent --output /dev/null "$url"; then return 0; fi
    i=$((i + 1))
    sleep 1
  done
  return 1
}

if ! wait_service_http "$agent_unit" "http://127.0.0.1:$agent_port/agent/v1/health"; then rollback; fail "Agent health check failed"; fi
if ! wait_service_http "$portal_unit" "http://127.0.0.1:$portal_port/"; then rollback; fail "Portal health check failed"; fi

external_host="127.0.0.1"
if [ "$portal_bind" = "0.0.0.0" ]; then external_host="$(hostname -I 2>/dev/null | awk '{print $1}')"; fi
[ -n "$external_host" ] || external_host="127.0.0.1"

info "Ponytail Risk installed successfully"
printf 'Portal URL: http://%s:%s/\n' "$external_host" "$portal_port"
printf 'Config:     %s\n' "$config_root"
printf 'Data:       %s\n' "$data_root"
if [ "$new_credentials" -eq 1 ]; then
  printf 'Portal key: %s\n' "$portal_key"
fi
printf 'For Internet access, place the portal behind HTTPS before enabling remote SDK access.\n'
