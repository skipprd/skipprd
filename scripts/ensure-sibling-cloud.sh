#!/usr/bin/env bash
# skipprd MAY path-depend on skippr-cloud source (../cloud/crates/cloud-client)
# until crates.io publishes it. MUST NOT clone skipprd/cloud for guest-broker.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLIENT="$ROOT/../cloud/crates/cloud-client/Cargo.toml"
if [[ -f "$CLIENT" ]]; then
  exit 0
fi

echo "skippr-cloud source not found at $CLIENT" >&2
echo "use a sibling cloud checkout, or depend on crates.io skippr-cloud" >&2
exit 1
