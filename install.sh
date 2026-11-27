#!/usr/bin/env bash
# ark-tbw installer for Ubuntu 22.04 / 24.04 / 26.04 (x86_64, arm64).
#
#   curl -fsSL https://raw.githubusercontent.com/geopsllc/ark-tbw/main/install.sh | bash
#
# Run it as the account that should own ark-tbw (not root; it asks for sudo when it needs it).
# It installs the binary, writes ~/.ark-tbw/config.toml and secret.toml from your answers,
# initialises the database, checks everything and starts the systemd service in SHADOW mode
# (payouts are computed and recorded, nothing is sent). Run it again to upgrade: your config,
# secret and database are kept and the service is restarted.
#
# Options:
#   --rootless          user service and ~/.local/bin, no sudo at all
#   --from-source       build with Rust instead of downloading a release binary
#   --version TAG       a specific release (default: the latest for this network)
#   --from-height N     start the database right after block N (default: the current tip)
#   --dir DIR           config folder (default: ~/.ark-tbw)
#   --binary FILE       install this binary instead of downloading or building one
#   --src DIR           use this source tree (for --from-source and the example config)
#   --no-service        install and initialise, but do not install or start the service
#   -y, --yes           no questions: defaults, or TBW_VALIDATOR, TBW_API, TBW_SHARE,
#                       TBW_KEEP, TBW_MEMO
set -euo pipefail

# ---- network of this branch -------------------------------------------------------------------
REPO=geopsllc/ark-tbw
BRANCH=main
TAG_PREFIX=v                      # releases of this branch: v0.1.0, v0.2.0, ...
NETWORK="ARK Core v3 mainnet"
API_PUBLIC=https://api.ark.io/api
API_PUBLIC_RATE=170               # requests per minute allowed on the public API
API_LOCAL=http://127.0.0.1:4003/api
# ------------------------------------------------------------------------------------------------

usage() {
  cat <<'USAGE'
ark-tbw installer. Run as the account that should own ark-tbw (not root):

  curl -fsSL https://raw.githubusercontent.com/geopsllc/ark-tbw/main/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/geopsllc/ark-tbw/main/install.sh | bash -s -- OPTIONS
  bash install.sh OPTIONS

Options:
  --rootless          user service and ~/.local/bin, no sudo at all
  --from-source       build with Rust instead of downloading a release binary
  --version TAG       a specific release (default: the latest for this network)
  --from-height N     start the database right after block N (default: the current tip)
  --dir DIR           config folder (default: ~/.ark-tbw)
  --binary FILE       install this binary instead of downloading or building one
  --src DIR           use this source tree (for --from-source and the example config)
  --no-service        install and initialise, but do not install or start the service
  -y, --yes           no questions: defaults, or TBW_VALIDATOR, TBW_API, TBW_SHARE,
                      TBW_KEEP, TBW_MEMO

Re-run to upgrade (or: ark-tbw upgrade): config, secret and database are kept, the service is restarted.
USAGE
}

ROOTLESS=0 FROM_SOURCE=0 TAG="" FROM_HEIGHT="" DIR="$HOME/.ark-tbw" BINARY="" SRC="" SERVICE=1 YES=0
while [ $# -gt 0 ]; do
  case "$1" in
    --rootless) ROOTLESS=1 ;;
    --from-source) FROM_SOURCE=1 ;;
    --version) TAG="$2"; shift ;;
    --from-height) FROM_HEIGHT="$2"; shift ;;
    --dir) DIR="$2"; shift ;;
    --binary) BINARY="$2"; shift ;;
    --src) SRC="$2"; shift ;;
    --no-service) SERVICE=0 ;;
    -y|--yes) YES=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option $1 (see --help)" >&2; exit 1 ;;
  esac
  shift
done

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# questions come from the terminal, also when the script itself arrives through a pipe
TTY=0
if [ "$YES" = 0 ] && { : </dev/tty; } 2>/dev/null; then TTY=1; fi
ask() { # ask VAR "question" default
  local v=""
  if [ "$TTY" = 1 ]; then
    read -r -p "$2 [$3]: " v </dev/tty || true
  fi
  printf -v "$1" '%s' "${v:-$3}"
}
ask_secret() { # ask_secret VAR "question"
  local v=""
  if [ "$TTY" = 1 ]; then
    read -r -s -p "$2: " v </dev/tty || true
    echo >/dev/tty
  fi
  printf -v "$1" '%s' "$v"
}

# ---- checks -------------------------------------------------------------------------------------
[ "$(uname -s)" = Linux ] || die "ark-tbw runs on Linux (Ubuntu)"
[ "$(id -u)" != 0 ] || die "run this as the account that should own ark-tbw, not as root (it uses sudo when needed)"
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "unsupported CPU $(uname -m): x86_64 and arm64 only" ;;
esac
if [ -r /etc/os-release ]; then
  os=$(. /etc/os-release; echo "${ID:-}:${VERSION_ID:-}")
  case "$os" in
    ubuntu:22.04|ubuntu:24.04|ubuntu:26.04) ;;
    *) warn "tested on Ubuntu 22.04, 24.04 and 26.04; this is $(. /etc/os-release; echo "${PRETTY_NAME:-unknown}")" ;;
  esac
fi
SUDO=""
if [ "$ROOTLESS" = 0 ]; then
  command -v sudo >/dev/null || die "sudo is not installed: run as a user with sudo, or use --rootless"
  SUDO=sudo
fi

apt_need() { # apt_need pkg... : install what is missing
  [ $# -gt 0 ] || return 0
  [ -n "$SUDO" ] || die "missing $*: install them (sudo apt install $*) and run again"
  say "installing $*"
  $SUDO apt-get update -qq && $SUDO DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null
}
pkgs=()
command -v curl >/dev/null || pkgs+=(curl)
[ -e /etc/ssl/certs/ca-certificates.crt ] || pkgs+=(ca-certificates)
apt_need ${pkgs[@]+"${pkgs[@]}"}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# ---- the binary ---------------------------------------------------------------------------------
latest_release() {
  curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=50" 2>/dev/null |
    grep -o '"tag_name": *"[^"]*"' | sed 's/.*"\([^"]*\)"$/\1/' |
    grep -E "^${TAG_PREFIX}[0-9]" | head -1 || true
}

REF=$BRANCH
if [ -n "$BINARY" ]; then
  [ -f "$BINARY" ] || die "no file $BINARY"
  cp "$BINARY" "$WORK/ark-tbw"
elif [ "$FROM_SOURCE" = 0 ]; then
  [ -n "$TAG" ] || TAG=$(latest_release)
  if [ -z "$TAG" ]; then
    warn "no release published for $NETWORK yet: building from source instead"
    FROM_SOURCE=1
  else
    say "downloading ark-tbw $TAG ($ARCH)"
    base="https://github.com/$REPO/releases/download/$TAG"
    curl -fsSL -o "$WORK/ark-tbw" "$base/ark-tbw-$ARCH-linux" || die "download failed: $base/ark-tbw-$ARCH-linux"
    if curl -fsSL -o "$WORK/SHA256SUMS" "$base/SHA256SUMS"; then
      want=$(grep " ark-tbw-$ARCH-linux\$" "$WORK/SHA256SUMS" | cut -d' ' -f1)
      got=$(sha256sum "$WORK/ark-tbw" | cut -d' ' -f1)
      [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for ark-tbw-$ARCH-linux"
      say "checksum OK"
    else
      warn "release has no SHA256SUMS; checksum not verified"
    fi
    REF=$TAG
  fi
fi
if [ -z "$BINARY" ] && [ "$FROM_SOURCE" = 1 ]; then
  pkgs=()
  command -v cc >/dev/null || pkgs+=(build-essential)
  [ -n "$SRC" ] || command -v git >/dev/null || pkgs+=(git)
  apt_need ${pkgs[@]+"${pkgs[@]}"}
  if [ -z "$SRC" ]; then
    command -v git >/dev/null || die "git is needed to build from source"
    say "fetching the source ($BRANCH)"
    git clone -q --depth 1 -b "${TAG:-$BRANCH}" "https://github.com/$REPO.git" "$WORK/src" || die "could not clone $REPO"
    SRC="$WORK/src"
  fi
  if ! command -v cargo >/dev/null; then
    [ -x "$HOME/.cargo/bin/cargo" ] || {
      say "installing Rust (rustup, into ~/.cargo)"
      curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null
    }
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
  say "building (a few minutes on a small server)"
  CARGO_TARGET_DIR="$WORK/target" cargo build -q --release --locked --manifest-path "$SRC/Cargo.toml"
  cp "$WORK/target/release/ark-tbw" "$WORK/ark-tbw"
fi
chmod 755 "$WORK/ark-tbw"
"$WORK/ark-tbw" --version >/dev/null 2>&1 || die "the binary does not run on this machine"

if [ "$ROOTLESS" = 1 ]; then
  BIN_DIR="$HOME/.local/bin"
  mkdir -p "$BIN_DIR"
  install -m 755 "$WORK/ark-tbw" "$BIN_DIR/ark-tbw"
  case ":$PATH:" in *":$BIN_DIR:"*) ;; *) warn "$BIN_DIR is not in your PATH: log in again, or add it to ~/.profile" ;; esac
  export PATH="$BIN_DIR:$PATH"
else
  BIN_DIR=/usr/local/bin
  $SUDO install -m 755 "$WORK/ark-tbw" "$BIN_DIR/ark-tbw"
fi
TBW="$BIN_DIR/ark-tbw"
say "installed $("$TBW" --version) to $BIN_DIR"

# ---- config and secret --------------------------------------------------------------------------
CFG="$DIR/config.toml"
SECRET="$DIR/secret.toml"
mkdir -p "$DIR"
chmod 700 "$DIR"
toml_str() { local s=$1; s=${s//\\/\\\\}; s=${s//\"/\\\"}; printf '"%s"' "$s"; }

UPGRADE=0
if [ -f "$CFG" ]; then
  UPGRADE=1
  say "keeping your config $CFG"
else
  if [ -n "$SRC" ] && [ -f "$SRC/config.example.toml" ]; then
    cp "$SRC/config.example.toml" "$WORK/example.toml"
  else
    curl -fsSL -o "$WORK/example.toml" "https://raw.githubusercontent.com/$REPO/$REF/config.example.toml" || die "could not download the example config"
  fi
  echo
  say "configuration for $NETWORK (Enter keeps the default; change anything later in $CFG)"
  ask V_NAME "Validator username or address" "${TBW_VALIDATOR:-myvalidator}"
  if curl -fsS -m 3 -o /dev/null "$API_LOCAL/node/configuration" 2>/dev/null; then api_default=$API_LOCAL; else api_default=$API_PUBLIC; fi
  ask V_API "Node API" "${TBW_API:-$api_default}"
  while :; do
    ask V_SHARE "Percent of each block reward shared with voters (the keep is the rest)" "${TBW_SHARE:-50}"
    [[ "$V_SHARE" =~ ^[0-9]{1,3}(\.[0-9]{1,2})?$ ]] && awk -v p="$V_SHARE" 'BEGIN{exit !(p>=0 && p<=100)}' && break
    [ "$TTY" = 1 ] || die "share must be a percentage from 0 to 100"
    echo "  a percentage from 0 to 100, at most 2 decimals"
  done
  while :; do
    ask V_KEEP "Address the keep is paid to (Enter = it stays in the validator wallet)" "${TBW_KEEP:-}"
    [ -z "$V_KEEP" ] || [[ "$V_KEEP" =~ ^A[1-9A-HJ-NP-Za-km-z]{33}$ ]] && break
    [ "$TTY" = 1 ] || die "keep address must be an ARK address"
    echo "  an ARK address (A and 33 more characters), or Enter for none"
  done
  ask V_MEMO "Payout memo (public)" "${TBW_MEMO:-$V_NAME - true block weight}"
  rate=0
  [ "${V_API%/}" = "$API_PUBLIC" ] && rate=$API_PUBLIC_RATE
  # only the first occurrence of each key in the example is replaced
  # values go through the environment: awk -v would interpret backslashes
  T_API="$(toml_str "${V_API%/}")" T_NAME="$(toml_str "$V_NAME")" T_SHARE="$V_SHARE" T_MEMO="$(toml_str "$V_MEMO")" T_RATE="$rate" T_KEEP="$V_KEEP" awk '
    !a && /^api = /                 { print "api = " ENVIRON["T_API"]; a=1; next }
    !r && /^requests_per_minute = / { print "requests_per_minute = " ENVIRON["T_RATE"]; r=1; next }
    !n && /^name = /                { print "name = " ENVIRON["T_NAME"]; n=1; next }
    !p && /^percent = /             { print "percent = " ENVIRON["T_SHARE"]; p=1; next }
    !k && /^# keep_address = / && ENVIRON["T_KEEP"] != "" { print "keep_address = \"" ENVIRON["T_KEEP"] "\""; k=1; next }
    !m && /^memo = /                { print "memo = " ENVIRON["T_MEMO"]; m=1; next }
    !e && /^enabled = /             { print "enabled = false"; e=1; next }
    { print }' "$WORK/example.toml" > "$WORK/config.toml"
  install -m 600 "$WORK/config.toml" "$CFG"
  say "wrote $CFG (payouts in SHADOW mode: nothing is sent until you set payout.enabled = true)"
fi

if [ -f "$SECRET" ]; then
  say "keeping your secret file $SECRET"
  chmod 600 "$SECRET"
else
  echo
  say "payouts are signed with the validator WALLET passphrase: the 12 or 24 words of the"
  echo "    validator's address, the ones you import into ARK Wallet (and forge with)."
  echo "    It is stored only in $SECRET (chmod 600)."
  ask_secret V_PASS "Validator wallet passphrase (Enter to add it later)"
  if [ -n "$V_PASS" ]; then
    say "a second passphrase is the wallet's optional second signature; most wallets have none"
    ask_secret V_PASS2 "Second passphrase, only if the wallet has one registered (Enter for none)"
    ( umask 077
      { printf 'passphrase = %s\n' "$(toml_str "$V_PASS")"
        [ -z "$V_PASS2" ] || printf 'second_passphrase = %s\n' "$(toml_str "$V_PASS2")"
      } > "$SECRET" )
    unset V_PASS V_PASS2
    say "wrote $SECRET"
  else
    ( umask 077; printf '# validator WALLET passphrase\n# passphrase = "..."\n# second_passphrase = "..."   (only if the wallet has one)\n' > "$SECRET" )
    warn "no passphrase yet: add it to $SECRET before you enable payouts"
  fi
fi

# ---- database, check, service -------------------------------------------------------------------
echo
if "$TBW" -c "$CFG" status >/dev/null 2>&1; then
  say "keeping your database"
else
  say "initialising the database"
  if [ -n "$FROM_HEIGHT" ]; then "$TBW" -c "$CFG" init --from-height "$FROM_HEIGHT"; else "$TBW" -c "$CFG" init; fi
fi
echo
"$TBW" -c "$CFG" check || die "the check failed: fix $CFG and run this installer again"

if [ "$SERVICE" = 1 ]; then
  echo
  unit=/etc/systemd/system/ark-tbw.service
  [ "$ROOTLESS" = 1 ] && unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ark-tbw.service"
  kind=$([ "$ROOTLESS" = 1 ] && echo "user service" || echo "system service")
  if [ -f "$unit" ]; then
    # an upgrade: the unit is only rewritten when this version would write a different one
    # (new binary path, config path or service options); otherwise a restart is enough
    if [ "$ROOTLESS" = 1 ]; then want=$("$TBW" --config "$CFG" service show --rootless 2>/dev/null || true)
    else want=$("$TBW" --config "$CFG" service show 2>/dev/null || true); fi
    if [ -n "$want" ] && [ "$want" = "$(cat "$unit" 2>/dev/null)" ]; then
      say "restarting the $kind on the new binary"
    else
      say "updating the $kind file (it changed in this version)"
      if [ "$ROOTLESS" = 1 ]; then "$TBW" --config "$CFG" service install --rootless >/dev/null; else sudo "$TBW" --config "$CFG" service install >/dev/null; fi
    fi
    "$TBW" restart
  else
    say "installing the $kind"
    if [ "$ROOTLESS" = 1 ]; then "$TBW" --config "$CFG" service install --rootless; else sudo "$TBW" --config "$CFG" service install; fi
    "$TBW" start
  fi
  sleep 3
  "$TBW" -c "$CFG" status || true
fi

echo
if [ "$UPGRADE" = 1 ]; then
  say "upgraded to $("$TBW" --version)$([ "$SERVICE" = 1 ] && echo ", service restarted")"
  if awk '/^\[payout\]/{p=1;next} /^\[/{p=0} p && /^enabled *= *false/{f=1} END{exit !f}' "$CFG"; then
    echo "    payouts are still in shadow mode (payout.enabled = false in $CFG)"
  fi
  exit 0
fi
say "done. Next:"
cat <<EOF
    ark-tbw logs -f            follow the service log
    ark-tbw status / voters / blocks / payouts
    nano $CFG   then: ark-tbw reload
  Shadow mode records every payout without sending it (ark-tbw payouts). To go live, fund the
  validator wallet, put the passphrase in $SECRET, set  enabled = true  under [payout],
  run  ark-tbw check  and then  ark-tbw reload.
EOF
