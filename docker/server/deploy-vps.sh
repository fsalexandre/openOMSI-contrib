#!/usr/bin/env bash
# Put a new build of the server on a VPS without sending the whole image: only the Linux
# program is built (the Dockerfile's `build` stage, with cargo's cache: a small change
# builds in a minute or two), sent compressed and swapped in; the server restarts.
#
# The VPS keeps the runtime image (Debian and the libraries: rebuilt only when those
# change, `./deploy-vps.sh --image`) and runs the program from /srv/openomsi/bin, mounted
# over the image's (see docker-compose.vps.yml).
#
# Usage, from this folder (Git Bash on Windows, or any shell with docker and ssh):
#   ./deploy-vps.sh            the program only (~55 MB, sent compressed)
#   ./deploy-vps.sh --image    the whole image (the first time, or new libraries)
# Settings (environment): VPS (root@your-vps), VPS_KEY (~/.ssh/id_ed25519),
# VPS_DIR (/srv/openomsi).
set -euo pipefail
cd "$(dirname "$0")"
VPS="${VPS:-root@your-vps}"
VPS_KEY="${VPS_KEY:-$HOME/.ssh/id_ed25519}"
VPS_DIR="${VPS_DIR:-/srv/openomsi}"
ssh_vps() { ssh -i "$VPS_KEY" -o BatchMode=yes -o IdentitiesOnly=yes "$VPS" "$@"; }
repo="$(cd ../.. && pwd)"
version="${OPENOMSI_VERSION:-$(tr -d ' \r\n' < "$repo/VERSION").custom}"
start=$(date +%s)

if [ "${1:-}" = "--image" ]; then
    echo "Building the whole image..."
    docker build -f Dockerfile --build-arg OPENOMSI_VERSION="$version" -t openomsi-server:custom "$repo"
    echo "Sending it..."
    docker save openomsi-server:custom | gzip -1 | ssh_vps 'gunzip | docker load'
else
    echo "Building the program (Linux)..."
    docker build -f Dockerfile --target build --build-arg OPENOMSI_VERSION="$version" -t openomsi-server-build "$repo"
    out="$repo/target/linux"
    mkdir -p "$out"
    id=$(docker create openomsi-server-build)
    docker cp "$id:/out/openomsi" "$out/openomsi" >/dev/null
    docker rm "$id" >/dev/null
    echo "Sending $(du -h "$out/openomsi" | cut -f1) (compressed on the way)..."
    # (in beside the running one, then swapped: the server never runs half a file)
    gzip -1 -c "$out/openomsi" | ssh_vps "mkdir -p '$VPS_DIR/bin' && gunzip > '$VPS_DIR/bin/openomsi.new' && chmod 755 '$VPS_DIR/bin/openomsi.new' && mv '$VPS_DIR/bin/openomsi.new' '$VPS_DIR/bin/openomsi'"
fi

echo "Restarting the server..."
ssh_vps "cd '$VPS_DIR' && docker compose up -d >/dev/null 2>&1 && docker compose restart >/dev/null 2>&1; for i in \$(seq 1 100); do curl -fsS -m 3 http://127.0.0.1:27025/status >/dev/null 2>&1 && break; sleep 3; done; curl -s -m 5 http://127.0.0.1:27025/status | head -c 200; echo"
echo "Done in $(( $(date +%s) - start )) s."
