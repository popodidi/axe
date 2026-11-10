#!/usr/bin/env bash
# Smoke-test a built axe image: version, revision, health and the embedded UI.
# Usage: scripts/smoke_image.sh <image-ref> <git-commit> <version>
# Uses disposable state inside the container; never mount production data.
set -euo pipefail
image=$1 revision=$2 version=$3
name="axe-smoke-${RANDOM}-$$"
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT

test "$(docker run --rm "$image" --version)" = "axe $version"
docker run -d --name "$name" --entrypoint sh "$image" -c \
  'mkdir -p /tmp/state; echo fixture > /tmp/token; AXE_DB=/tmp/state/axe.db AXE_CONFIG=/tmp/missing AXE_TOKEN_FILE=/tmp/token exec axe server --listen 0.0.0.0:8790' >/dev/null
get() {
  docker exec "$name" python3 -c 'import sys, urllib.request; r = urllib.request.urlopen("http://127.0.0.1:8790" + sys.argv[1]); assert r.status == 200; sys.stdout.write(r.read().decode())' "$1"
}
for _ in $(seq 30); do get /healthz >/dev/null 2>&1 && break; sleep 1; done
get /healthz | grep -q '"ok":true'
get / | grep -qi '<html'
test "$(docker exec "$name" cat /etc/axe-revision)" = "$revision"
echo "smoke ok: $image"
