#!/usr/bin/env bash
# Publishes a GitHub release of the checked-out branch, with the Linux binaries that install.sh
# downloads: ark-tbw-x86_64-linux, ark-tbw-aarch64-linux and SHA256SUMS.
#   tools/release.sh <tag> <notes-file> [--prerelease] [--latest]
# Tags: main v0.1.0, ...; mainsail mainsail-v0.1.0, ... (install.sh looks for its branch's prefix).
# Run on the maintainer's Mac: the GitHub token comes from the macOS keychain
# (git credential-osxkeychain, host github.com) and is never printed.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG="$1"; NOTES="$2"; shift 2
PRE=false LATEST=false
for a in "$@"; do case "$a" in --prerelease) PRE=true ;; --latest) LATEST=true ;; *) echo "unknown option $a"; exit 1 ;; esac; done
REPO=geopsllc/ark-tbw
[ -f "$NOTES" ] || { echo "missing $NOTES"; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "commit or stash your changes first"; exit 1; }
BRANCH=$(git branch --show-current)
prefix=$(sed -n 's/^TAG_PREFIX=\([^ ]*\).*/\1/p' install.sh)
case "$TAG" in "$prefix"[0-9]*) ;; *) echo "tag $TAG does not match this branch's prefix '$prefix' (install.sh)"; exit 1 ;; esac
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ "${TAG#"$prefix"}" = "$ver" ] || { echo "tag $TAG does not match Cargo.toml version $ver"; exit 1; }

tools/build-linux.sh
( cd dist && shasum -a 256 ark-tbw-x86_64-linux ark-tbw-aarch64-linux > SHA256SUMS && cat SHA256SUMS )

git tag -a "$TAG" -m "ark-tbw $TAG"
git push -q origin "$TAG"

TOKEN=$(printf "protocol=https\nhost=github.com\n\n" | git credential-osxkeychain get | sed -n 's/^password=//p')
[ -n "$TOKEN" ] || { echo "no token in keychain"; exit 1; }
AUTH="Authorization: Bearer $TOKEN"
J=$(mktemp); R=$(mktemp)
trap 'rm -f "$J" "$R"' EXIT
python3 -I -c 'import json,sys; print(json.dumps({"tag_name":sys.argv[1],"target_commitish":sys.argv[2],"name":"ark-tbw "+sys.argv[1],"body":open(sys.argv[3]).read(),"draft":False,"prerelease":sys.argv[4]=="true","make_latest":sys.argv[5]}))' "$TAG" "$BRANCH" "$NOTES" "$PRE" "$LATEST" > "$J"
code=$(curl -sS -o "$R" -w '%{http_code}' -X POST -H "$AUTH" -H "Accept: application/vnd.github+json" "https://api.github.com/repos/$REPO/releases" --data @"$J")
echo "create release: HTTP $code"
[ "$code" = 201 ] || { python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get("message"), d.get("errors"))' "$R"; exit 1; }
ID=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$R")
URL=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["html_url"])' "$R")
for f in ark-tbw-x86_64-linux ark-tbw-aarch64-linux SHA256SUMS; do
  ct=application/octet-stream; [ "$f" = SHA256SUMS ] && ct=text/plain
  code=$(curl -sS -o "$R" -w '%{http_code}' -X POST -H "$AUTH" -H "Content-Type: $ct" --data-binary @"dist/$f" "https://uploads.github.com/repos/$REPO/releases/$ID/assets?name=$f")
  echo "upload $f: HTTP $code"
  [ "$code" = 201 ] || { python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1])))' "$R"; exit 1; }
done
unset TOKEN AUTH
echo "$URL"
