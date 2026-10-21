#!/bin/sh
# Run a Linux release of rune on the systems it is built for, not only on
# the one that built it:
#
#   scripts/smoke_linux_release.sh <path/to/rune> gnu|musl
#
# In each image the build must start, read and write a home of its own
# (SQLite, the upgrade steps, the help bundle) and answer a one-shot prompt
# from the script provider — no network, no credential. `check_linux_binary.py`
# says what the file asks for; this says the systems give it. Needs Docker.
#
#   gnu   CentOS 7 (glibc 2.17, the floor), Debian 11 (2.31), Debian 12 (2.36,
#         the SeaDrive console that could not start 0.7.7), Ubuntu 24.04,
#         and Alpine with its glibc compatibility layer left out: it must
#         refuse there, not crash half way.
#   musl  the same systems, every one of which must run it, Alpine and
#         BusyBox included.
set -eu
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
libc=$2
root=$(cd "$(dirname "$0")/.." && pwd)
work="$root/target/test-tmp/smoke-$libc-$$"
mkdir -p "$work/home" "$work/project"
trap 'rm -rf "$work"' EXIT

cat > "$work/home/rune.toml" <<'EOF'
[providers]
default = "fixture"
[[providers.list]]
name = "fixture"
base_url = "http://127.0.0.1:1"
api = "openai"
auth = { header = "Authorization", prefix = "Bearer " }
[[providers.list.models]]
id = "test"
thinking = false
context = 32768
EOF
cat > "$work/home/fixture.json" <<'EOF'
{"responses": [{"chunks": [{"text": "Smoke answered."}]}]}
EOF
cp "$bin" "$work/rune"

images="centos:7 debian:bullseye debian:bookworm ubuntu:24.04"
if [ "$libc" = musl ]; then
  images="$images alpine:3.22 busybox:1.37"
fi

failed=""
for image in $images; do
  # A fresh home per system: each run upgrades and writes it from nothing.
  rm -rf "$work/home/projects" "$work/home/rune.db"* "$work/home/run"
  if docker run --rm --network none \
      -v "$work:/smoke" -e RUNE_HOME=/smoke/home -e HOME=/smoke/home -e TERM=dumb \
      --user "$(id -u):$(id -g)" "$image" \
      sh -ec '
        /smoke/rune -version
        /smoke/rune -project /smoke/project -script /smoke/home/fixture.json -p "smoke" > /smoke/answer.txt
        grep -q "Smoke answered." /smoke/answer.txt
        /smoke/rune -config check
      ' > "$work/out.txt" 2>&1; then
    echo "PASS $image"
  else
    echo "FAIL $image"
    sed 's/^/     /' "$work/out.txt" | tail -20
    failed="$failed $image"
  fi
done

# A glibc build on a system without glibc: refused by the system at once,
# never a partial start.
if [ "$libc" = gnu ]; then
  if docker run --rm --network none -v "$work:/smoke" alpine:3.22 /smoke/rune -version >/dev/null 2>&1; then
    echo "FAIL alpine:3.22 started a glibc build"
    failed="$failed alpine-refusal"
  else
    echo "PASS alpine:3.22 refuses a glibc build (as expected)"
  fi
fi

if [ -n "$failed" ]; then
  echo "smoke failed on:$failed"
  exit 1
fi
echo "smoke passed: $libc"
