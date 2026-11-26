#!/usr/bin/env bash
# Runs the static binary on Ubuntu 22.04, 24.04 and 26.04 in Docker (with systemd unit verification).
# Usage: tools/test-ubuntu.sh [x86_64|aarch64]   (default: the host's arch). Build first: tools/build-linux.sh
set -euo pipefail
cd "$(dirname "$0")/.."
n=${1:-$(uname -m | sed 's/arm64/aarch64/')}
case "$n" in x86_64) a=amd64 ;; aarch64) a=arm64 ;; *) echo "unknown arch $n" >&2; exit 1 ;; esac
bin="dist/ark-tbw-$n-linux"; [ -x "$bin" ] || { echo "missing $bin: run tools/build-linux.sh" >&2; exit 1; }
for v in 22.04 24.04 26.04; do
  echo "== ubuntu:$v ($a)"
  docker run --rm --platform "linux/$a" -v "$PWD/$bin":/usr/local/bin/ark-tbw:ro \
    -v "$PWD/config.example.toml":/cfg/config.example.toml:ro "ubuntu:$v" bash -euo pipefail -c '
      grep PRETTY /etc/os-release
      apt-get update -qq >/dev/null && apt-get install -y -qq systemd ca-certificates >/dev/null 2>&1
      useradd -m tbw
      ark-tbw --version
      su tbw -c "mkdir -p ~/.ark-tbw && cd ~/.ark-tbw && sed \"s|http://127.0.0.1:4003/api|https://api.ark.io/api|;s|requests_per_minute = 0|requests_per_minute = 60|;s|name = \\\"myvalidator\\\"|name = \\\"geops\\\"|;s|enabled = true|enabled = false|\" /cfg/config.example.toml > config.toml && printf \"passphrase = \\\"abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about\\\"\n\" > secret.toml && chmod 600 secret.toml"
      su tbw -c "ark-tbw --config ~/.ark-tbw/config.toml init" | tail -2
      su tbw -c "ark-tbw --config ~/.ark-tbw/config.toml status"
      su tbw -c "ark-tbw --config ~/.ark-tbw/config.toml check" 2>&1 | tail -3 || true
      ark-tbw --config /home/tbw/.ark-tbw/config.toml service show --user tbw > /etc/systemd/system/ark-tbw.service
      systemd-analyze verify /etc/systemd/system/ark-tbw.service && echo "unit OK"
      ark-tbw logs -n 1 >/dev/null 2>&1; echo "logs command runs (exit $?; no journald in a container)"
    '
done
