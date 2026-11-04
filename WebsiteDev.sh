#!/usr/bin/env bash
# WebsiteDev.sh — development website of Atoryn Studio (DEV only).
#
#   WebsiteDev.sh Start       rebuild from Website/, then (re)start in the background
#   WebsiteDev.sh Shutdown    stop
#   WebsiteDev.sh Status
#
# The pages are embedded in the binary (UI/ui.go), so every Start rebuilds: a
# change in Website/ shows after the next Start. Runs as the owner, needs no
# root, writes only inside Website/build/Dev/ (ignored by Git) and never
# touches production (~/Deploy, /etc, systemd, nginx).
# Dev port = prod + 100 inside Studio's 3300-3499; listens on the LAN address
# only. The health listener is loopback by the server's own rule.
set -euo pipefail

STUDIO="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
WEB="$STUDIO/Website"
RUN="$WEB/build/Dev"
BIN="$RUN/atoryn-studio-web-dev"
HOST="${ATORYN_DEV_HOST:-192.168.18.10}"
PORT=3401
HEALTH_PORT=$((PORT + 2))
DOWNLOADS="${STUDIO_DEV_DOWNLOADS:-$RUN/Downloads}"

die() { echo "Studio   $*" >&2; exit 1; }

usage() { sed -n '2,13p' "$0"; exit 2; }

# One instance per project: a second Start/Shutdown waits nothing and says so.
lock() { exec 9>"$RUN/.lock"; flock -n 9 || die "another WebsiteDev.sh is running for this project"; }

pid_of() { cat "$RUN/server.pid" 2>/dev/null; }

# setsid makes the server lead its own process group; killing the group reaches
# it even if the recorded pid is stale.
group_alive() {
    [[ "${1:-}" =~ ^[0-9]+$ ]] || return 1
    kill -0 -"$1" 2>/dev/null
}

# The recorded pid is alive and really our binary. A rebuild replaces the file
# under a live server, so its exe then reads "<path> (deleted)".
ours_alive() {
    local pid
    pid="$(pid_of)" || return 1
    [[ "$pid" =~ ^[0-9]+$ ]] || return 1
    [ "$(readlink "/proc/$pid/exe" 2>/dev/null | sed 's/ (deleted)$//' || true)" = "$BIN" ]
}

# A pipe into `grep -q` makes the writer die of SIGPIPE, and `pipefail` then
# calls the whole pipeline failed even though the match was found. Never pipe
# into `grep -q` here; capture first, then match.
have_addr() {
    local addrs
    addrs="$(ip -4 -o addr show 2>/dev/null || true)"
    case "$addrs" in *"inet $HOST/"*) return 0 ;; *) return 1 ;; esac
}

find_go() {
    local go
    go="$(command -v go || true)"
    [ -n "$go" ] || go=/usr/local/go/bin/go
    [ -x "$go" ] || die "Go was not found"
    printf '%s\n' "$go"
}

port_busy() { ss -ltn | awk -v p=":$1" '$4 ~ p "$" { f = 1 } END { exit !f }'; }

write_config() {
    mkdir -p "$DOWNLOADS"
    cat >"$RUN/Config.json" <<EOF
{
  "env": "Dev",
  "basePath": "/Studio",
  "origin": "http://$HOST:$PORT",
  "hosts": ["$HOST:$PORT"],
  "listen": ["$HOST:$PORT"],
  "health": "127.0.0.1:$HEALTH_PORT",
  "downloads": "$DOWNLOADS",
  "indexing": false,
  "parentUrl": "${STUDIO_DEV_PARENT:-http://$HOST:3601}"
}
EOF
}

build() {
    local go
    go="$(find_go)"
    mkdir -p "$RUN"
    (cd "$WEB" && CGO_ENABLED=0 GOTOOLCHAIN=local GOFLAGS=-mod=readonly \
        "$go" build -trimpath -buildvcs=false -o "$BIN.new" ./Server) || die "build failed; a running server is left as it is"
    mv -f "$BIN.new" "$BIN"
    write_config
    "$BIN" --config "$RUN/Config.json" --check >"$RUN/check.log" 2>&1 ||
        { cat "$RUN/check.log" >&2; die "config check failed"; }
}

stop_server() {
    local pid
    pid="$(pid_of)"
    if group_alive "$pid"; then
        kill -TERM -"$pid" 2>/dev/null || true
        for _ in $(seq 1 100); do # the server drains for up to 20 s
            group_alive "$pid" || break
            sleep 0.25
        done
        if group_alive "$pid"; then
            kill -KILL -"$pid" 2>/dev/null || true
            sleep 0.5
        fi
    fi
    rm -f "$RUN/server.pid"
    group_alive "$pid" && die "server group $pid did not stop"
    return 0
}

cmd_start() {
    have_addr || die "$HOST is not an address of this machine (set ATORYN_DEV_HOST)"
    lock
    # Build first: a failed build must never take down a running server.
    build
    if port_busy "$PORT" || port_busy "$HEALTH_PORT"; then
        if ours_alive || group_alive "$(pid_of)"; then
            echo "Studio   restarting with a fresh build"
            stop_server
        else
            die "port $PORT or $HEALTH_PORT is in use by another process"
        fi
    fi
    port_busy "$PORT" && die "port $PORT is already in use"
    port_busy "$HEALTH_PORT" && die "port $HEALTH_PORT is already in use"
    # Closing fd 9 and detaching: the server must hold no lock and no terminal.
    setsid nohup "$BIN" --config "$RUN/Config.json" >"$RUN/server.log" 2>&1 </dev/null 9>&- &
    echo $! >"$RUN/server.pid"
    for _ in $(seq 1 40); do
        if curl -fsS -o /dev/null --max-time 2 "http://$HOST:$PORT/Studio/" 2>/dev/null; then
            echo "Studio   started           http://$HOST:$PORT/Studio/"
            return 0
        fi
        group_alive "$(pid_of)" || { cat "$RUN/server.log" >&2; die "the server exited; see $RUN/server.log"; }
        sleep 0.25
    done
    die "started but did not answer; see $RUN/server.log"
}

cmd_shutdown() {
    lock
    if ! ours_alive && ! group_alive "$(pid_of)"; then
        rm -f "$RUN/server.pid"
        echo "Studio   not running"
        return 0
    fi
    stop_server
    echo "Studio   stopped"
}

cmd_status() {
    if ours_alive; then
        echo "Studio   running           http://$HOST:$PORT/Studio/   pid $(pid_of)"
    elif group_alive "$(pid_of)"; then
        echo "Studio   degraded          a process outlives a dead leader; run Shutdown"
    else
        echo "Studio   stopped"
    fi
}

mkdir -p "$RUN"
[ $# -eq 1 ] || usage
case "${1,,}" in
start) cmd_start ;;
shutdown) cmd_shutdown ;;
status) cmd_status ;;
*) usage ;;
esac
