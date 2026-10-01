#!/usr/bin/env bash
# Local remote-execution harness: NativeLink (CAS, action cache, scheduler and
# a worker) in a container that only has what the image provides (by default
# python:3.12-slim-bookworm: /bin/sh, coreutils and python3, which the
# prelude's Go bootstrap runs; no Go or Node.js).
# Actions that depend on undeclared host tools or paths fail on it instead of
# silently working.
#
#   run.sh start    start NativeLink (the cache is kept across restarts)
#   run.sh stop     stop it
#   run.sh wipe     stop it and delete the cache (CAS + action cache)
#   run.sh status
#
# Environment:
#   NATIVELINK       statically linked nativelink binary (default: from PATH)
#   NL_IMAGE         worker image (default: mirror.gcr.io/library/python:3.12-slim-bookworm);
#                    must match container-image in re.buckconfig
#   NL_STATE         host directory for the cache (default: ~/.cache/buck2-re-harness)
#   NL_CPUS          CPUs given to the container (default: all)
#   NL_MAX_INFLIGHT  max concurrent actions on the worker (default: nproc)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
NATIVELINK=${NATIVELINK:-$(command -v nativelink || true)}
NL_IMAGE=${NL_IMAGE:-mirror.gcr.io/library/python:3.12-slim-bookworm}
NL_STATE=${NL_STATE:-$HOME/.cache/buck2-re-harness}
NAME=buck2-re-harness

wait_for_port() {
    local i=0
    until (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; do
        i=$((i + 1))
        if [ $i -gt 100 ]; then
            echo "NativeLink did not open port $1; see \`docker logs $NAME\`" >&2
            exit 1
        fi
        sleep 0.2
    done
}

start() {
    if [ -z "$NATIVELINK" ]; then
        echo "set NATIVELINK to a statically linked nativelink binary" >&2
        exit 1
    fi
    if [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" != "true" ]; then
        docker rm -f "$NAME" >/dev/null 2>&1 || true
        mkdir -p "$NL_STATE"
        docker run -d --name "$NAME" --network host \
            ${NL_CPUS:+--cpus "$NL_CPUS"} \
            -v "$(readlink -f "$NATIVELINK"):/usr/local/bin/nativelink:ro" \
            -v "$here/nativelink.json5:/etc/nativelink.json5:ro" \
            -v "$NL_STATE:/nl" \
            -e NL_IMAGE="$NL_IMAGE" -e NL_MAX_INFLIGHT="${NL_MAX_INFLIGHT:-$(nproc)}" \
            "$NL_IMAGE" /usr/local/bin/nativelink /etc/nativelink.json5 >/dev/null
    fi
    wait_for_port 50051
    status
}

stop() {
    docker rm -f "$NAME" >/dev/null 2>&1 || true
}

status() {
    echo "nativelink: $(docker inspect -f '{{.State.Status}}' "$NAME" 2>/dev/null || echo stopped)" \
        "(image $NL_IMAGE, cache in $NL_STATE)"
}

case "${1:-}" in
start) start ;;
stop) stop ;;
wipe)
    stop
    # Written by root inside the container.
    docker run --rm -v "$(dirname "$NL_STATE"):/p" "$NL_IMAGE" rm -rf "/p/$(basename "$NL_STATE")"
    ;;
status) status ;;
*)
    sed -n '2,20p' "$0"
    exit 1
    ;;
esac
