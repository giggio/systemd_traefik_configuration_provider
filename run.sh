#!/usr/bin/env bash

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
set -x
TRAEFIK_OUT_DIR="$DIR"/test/units/ RUST_LOG=systemd_traefik_configuration_provider=trace cargo run
