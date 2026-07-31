#!/usr/bin/env bash
# Build a self-contained Linux x86_64 release and an online installer.
set -euo pipefail

cd "$(dirname "$0")/.."
root="$PWD"
output_dir="$root/dist/linux-release"
base_url=""
unsigned_lab=0

usage() {
  cat <<'EOF'
Usage:
  PGR_NODE_BIN=/path/to/node PGR_NODE_SHA256=approved_sha256 PGR_RELEASE_SIGNING_KEY=/secure/release-rsa.pem \
    bash deploy/build-linux-bundle.sh [--base-url HTTPS_DIRECTORY] [--output-dir DIR]

The generated install script embeds the bundle URL and SHA-256. Upload both files
to HTTPS_DIRECTORY, then customers run the printed curl | sudo bash command.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --base-url) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; base_url="${2%/}"; shift 2 ;;
    --output-dir) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; output_dir="$2"; shift 2 ;;
    --unsigned-lab) unsigned_lab=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown option: %s\n' "$1" >&2; exit 2 ;;
  esac
done

[ "$(uname -s)" = "Linux" ] || { printf 'Run this builder on Linux.\n' >&2; exit 1; }
[ "$(uname -m)" = "x86_64" ] || { printf 'Only Linux x86_64 is currently published.\n' >&2; exit 1; }
for cmd in cargo tar sha256sum zip awk find sort xargs openssl base64 stat; do command -v "$cmd" >/dev/null 2>&1 || { printf 'Missing build command: %s\n' "$cmd" >&2; exit 1; }; done

signing_key="${PGR_RELEASE_SIGNING_KEY:-}"
if [ "$unsigned_lab" -ne 1 ]; then
  [ -f "$signing_key" ] || { printf 'PGR_RELEASE_SIGNING_KEY must point to the production RSA private key.\n' >&2; exit 1; }
  key_mode="$(stat -c '%a' "$signing_key")"
  [ $((8#$key_mode & 077)) -eq 0 ] || { printf 'Release signing key must not be group/world accessible.\n' >&2; exit 1; }
  openssl rsa -in "$signing_key" -check -noout >/dev/null 2>&1 || { printf 'Release signing key must be a readable RSA private key.\n' >&2; exit 1; }
fi

node_bin="${PGR_NODE_BIN:-$(command -v node || true)}"
[ -x "$node_bin" ] || { printf 'Set PGR_NODE_BIN to a Node.js 18+ executable.\n' >&2; exit 1; }
node_sha256="${PGR_NODE_SHA256:-}"
if [ "$unsigned_lab" -ne 1 ]; then
  [[ "$node_sha256" =~ ^[0-9A-Fa-f]{64}$ ]] || { printf 'PGR_NODE_SHA256 is required for production builds.\n' >&2; exit 1; }
  [ "$(sha256sum "$node_bin" | awk '{print $1}')" = "${node_sha256,,}" ] || { printf 'Bundled Node.js SHA-256 mismatch.\n' >&2; exit 1; }
elif [ -n "$node_sha256" ]; then
  [ "$(sha256sum "$node_bin" | awk '{print $1}')" = "${node_sha256,,}" ] || { printf 'Bundled Node.js SHA-256 mismatch.\n' >&2; exit 1; }
fi
node_major="$($node_bin -p 'process.versions.node.split(".")[0]')"
[ "$node_major" -ge 18 ] || { printf 'Node.js 18+ is required.\n' >&2; exit 1; }
[ "$($node_bin -p 'process.arch')" = "x64" ] || { printf 'The bundled Node.js must be x64.\n' >&2; exit 1; }

version="$(awk -F'"' '/^version = "/ { print $2; exit }' Cargo.toml)"
[ -n "$version" ] || { printf 'Cannot read workspace version.\n' >&2; exit 1; }
build_stamp="$(date -u +%Y%m%d%H%M%S)"
release_version="$version+$build_stamp"
bundle_name="ponytail-risk-$version-linux-x86_64.tar.gz"
installer_name="ponytail-risk-install.sh"

printf '==> Building Rust release binaries\n'
build_target_dir="$root/target/linux-x86_64"
cargo_release_dir="$build_target_dir/release"
CARGO_TARGET_DIR="$build_target_dir" cargo build --locked --release -p risk-engine -p risk-probe -p risk-agent -p risk-sdk
"$cargo_release_dir/risk-live-data" self-check >/dev/null
"$cargo_release_dir/risk-agent" self-check >/dev/null
RISK_ENGINE="$cargo_release_dir/risk-live-data" "$node_bin" self_check.mjs >/dev/null

work_dir="$(mktemp -d /tmp/ponytail-risk-build.XXXXXX)"
cleanup() { rm -rf -- "$work_dir"; }
trap cleanup EXIT
bundle_root="$work_dir/ponytail-risk"
mkdir -p "$bundle_root/bin" "$bundle_root/lib" "$bundle_root/include" "$bundle_root/runtime/bin" "$bundle_root/dist/risk-sdk" "$bundle_root/public/assets" "$bundle_root/public/vendor" "$bundle_root/docs"

cp server.mjs "$bundle_root/"
cp public/app.html public/app.js public/home.js public/index.html public/styles.css "$bundle_root/public/"
cp public/assets/dashboard-preview.png "$bundle_root/public/assets/"
cp public/vendor/lucide-0.468.0.min.js "$bundle_root/public/vendor/"
cp docs/GAME_PLUGIN_INTEGRATION_V1.md docs/plugin-event-batch.v1.schema.json docs/plugin-event-batch.v1.example.json docs/SECURITY_AND_LICENSING.md "$bundle_root/docs/"
cp "$cargo_release_dir/risk-live-data" "$cargo_release_dir/risk-probe" "$cargo_release_dir/risk-agent" "$bundle_root/bin/"
cp "$cargo_release_dir/librisk_sdk.so" "$bundle_root/lib/"
cp crates/risk-sdk/include/ponytail_risk_sdk.h "$bundle_root/include/"
cp "$node_bin" "$bundle_root/runtime/bin/node"
if [ -f "$(dirname "$(dirname "$node_bin")")/LICENSE" ]; then cp "$(dirname "$(dirname "$node_bin")")/LICENSE" "$bundle_root/runtime/NODE_LICENSE"; fi

sdk_stage="$work_dir/sdk-linux-x86_64"
mkdir -p "$sdk_stage/lib" "$sdk_stage/include" "$sdk_stage/docs"
cp "$cargo_release_dir/librisk_sdk.so" "$sdk_stage/lib/"
cp crates/risk-sdk/include/ponytail_risk_sdk.h "$sdk_stage/include/"
cp docs/GAME_PLUGIN_INTEGRATION_V1.md docs/plugin-event-batch.v1.schema.json docs/plugin-event-batch.v1.example.json "$sdk_stage/docs/"
(cd "$sdk_stage" && zip -qr "$bundle_root/dist/risk-sdk/ponytail-risk-sdk-linux-x86_64.zip" .)
if [ -f dist/risk-sdk/ponytail-risk-sdk-windows-x86_64.zip ]; then cp dist/risk-sdk/ponytail-risk-sdk-windows-x86_64.zip "$bundle_root/dist/risk-sdk/"; fi
(cd "$bundle_root/dist/risk-sdk" && sha256sum ./*.zip > SHA256SUMS.txt)

printf '%s\n' "$release_version" >"$bundle_root/VERSION"
printf 'x86_64\n' >"$bundle_root/TARGET_ARCH"
chmod 0755 "$bundle_root/runtime/bin/node" "$bundle_root/bin/"*

printf '==> Running bundled runtime checks\n'
(cd "$bundle_root" && "$bundle_root/runtime/bin/node" --check server.mjs)
if find "$bundle_root" -type f \( -iname '*backup*' -o -iname '*.bak*' -o -iname '*pre-production*' \) -print -quit | grep -q .; then
  printf 'Release bundle contains a backup or pre-production file.\n' >&2
  exit 1
fi
(cd "$bundle_root" && find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 sha256sum >SHA256SUMS)

mkdir -p "$output_dir"
archive="$output_dir/$bundle_name"
tar -czf "$archive" -C "$work_dir" ponytail-risk
archive_sha256="$(sha256sum "$archive" | awk '{print $1}')"
printf '%s  %s\n' "$archive_sha256" "$bundle_name" >"$archive.sha256"

signature="$archive.sig"
public_key="$output_dir/release-public-key.pem"
if [ "$unsigned_lab" -ne 1 ]; then
  openssl rsa -in "$signing_key" -pubout -out "$public_key" >/dev/null 2>&1
  openssl dgst -sha256 -sign "$signing_key" -out "$signature" "$archive"
  openssl dgst -sha256 -verify "$public_key" -signature "$signature" "$archive" >/dev/null 2>&1 || { printf 'Release signature self-check failed.\n' >&2; exit 1; }
else
  rm -f -- "$signature" "$public_key"
fi

bundle_url=""
if [ -n "$base_url" ]; then
  case "$base_url" in https://*) ;; *) printf 'The publish base URL must use HTTPS.\n' >&2; exit 1 ;; esac
  bundle_url="$base_url/$bundle_name"
fi

installer="$output_dir/$installer_name"
{
  printf '#!/usr/bin/env bash\n'
  printf 'PGR_BUNDLE_URL=%q\n' "$bundle_url"
  printf 'PGR_BUNDLE_SHA256=%q\n' "$archive_sha256"
  printf 'PGR_BUNDLE_SIGNATURE_URL=%q\n' "${bundle_url:+$bundle_url.sig}"
  if [ "$unsigned_lab" -eq 1 ]; then
    printf 'PGR_ALLOW_UNSIGNED_LAB=1\n'
    printf 'PGR_RELEASE_PUBLIC_KEY_B64=\n'
  else
    printf 'PGR_ALLOW_UNSIGNED_LAB=0\n'
    printf 'PGR_RELEASE_PUBLIC_KEY_B64=%q\n' "$(base64 -w 0 "$public_key")"
  fi
  tail -n +2 deploy/install.sh
} >"$installer"
chmod 0755 "$installer"
installer_sha256="$(sha256sum "$installer" | awk '{print $1}')"
printf '%s  %s\n' "$installer_sha256" "$installer_name" >"$installer.sha256"

bash -n deploy/install.sh
bash -n "$installer"
if [ "$unsigned_lab" -eq 1 ]; then
  bash deploy/install.sh --bundle-file "$archive" --sha256 "$archive_sha256" --allow-unsigned-lab --check-only
else
  bash deploy/install.sh --bundle-file "$archive" --sha256 "$archive_sha256" --signature-file "$signature" --public-key-file "$public_key" --check-only
fi

printf '==> Release ready\n'
printf 'Bundle:    %s\n' "$archive"
printf 'Installer: %s\n' "$installer"
printf 'SHA-256:   %s\n' "$archive_sha256"
printf 'Installer SHA-256: %s\n' "$installer_sha256"
if [ "$unsigned_lab" -ne 1 ]; then printf 'Release public key SHA-256: %s\n' "$(sha256sum "$public_key" | awk '{print $1}')"; fi
if [ -n "$base_url" ]; then
  printf 'Customer command (download, verify installer, then run):\n'
  printf '  tmp=$(mktemp) && curl --proto '\''=https'\'' --tlsv1.2 -fsSL %s/%s -o "$tmp" && echo "%s  $tmp" | sha256sum -c - && sudo bash "$tmp"; rc=$?; rm -f "$tmp"; exit $rc\n' "$base_url" "$installer_name" "$installer_sha256"
else
  printf 'Set --base-url when building to emit the final customer command.\n'
fi
