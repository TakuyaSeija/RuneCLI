#!/bin/sh
# Type-check Rune for another OS from Linux, without that OS's SDK:
#
#   scripts/xcheck.sh <target-triple> <cargo subcommand> [arguments]
#   scripts/xcheck.sh x86_64-pc-windows-msvc clippy --workspace --all-targets -- -D warnings
#   scripts/xcheck.sh aarch64-apple-darwin clippy --workspace --all-targets -- -D warnings
#
# The C build scripts (aws-lc-sys, ring, SQLite) get a check-only compiler
# that creates the files it is asked for and nothing else: `cargo check` and
# `cargo clippy` never link, so nothing built here ever runs. Building and
# testing on macOS and Windows themselves is CI's (.github/workflows/studio-ci.yml).
# Needs the target installed: rustup target add <target-triple>.
set -e
root=$(cd "$(dirname "$0")/.." && pwd)
tools="$root/target/xcheck-tools"
mkdir -p "$tools/bin"

cat > "$tools/fakecc" <<'EOF'
#!/bin/sh
# Check-only stand-in for a C compiler or assembler: creates its output.
out=""
prev=""
for a in "$@"; do
  case "$prev" in -o) out="$a" ;; esac
  case "$a" in
    -o*) [ "$a" != "-o" ] && out="${a#-o}" ;;
    /Fo*|-Fo*) out="${a#?Fo}" ;;
    -E) exit 0 ;;
    --version|-v) echo "fakecc 1.0"; exit 0 ;;
  esac
  prev="$a"
done
[ -n "$out" ] && : > "$out"
exit 0
EOF

cat > "$tools/fakear" <<'EOF'
#!/bin/sh
# Check-only stand-in for ar or lib.exe: creates the archive it is asked for.
for a in "$@"; do
  case "$a" in
    /OUT:*|-out:*|-OUT:*) : > "${a#*:}"; exit 0 ;;
    *.a|*.lib) [ -e "$a" ] || : > "$a"; exit 0 ;;
  esac
done
exit 0
EOF

chmod +x "$tools/fakecc" "$tools/fakear"
# Assemblers and compilers some build scripts call by name for these targets.
for name in nasm clang; do
  ln -sf ../fakecc "$tools/bin/$name"
done

target="$1"
shift
t=$(echo "$target" | tr '-' '_')
export "CC_$t=$tools/fakecc" "CXX_$t=$tools/fakecc" "AR_$t=$tools/fakear"
export AWS_LC_SYS_NO_ASM=1 AWS_LC_SYS_CMAKE_BUILDER=0 AWS_LC_SYS_PREBUILT_NASM=0
export PATH="$tools/bin:$PATH"
export CARGO_TARGET_DIR="$root/target/xcheck"
cd "$root"
# --target goes with the subcommand, before any `--` that ends cargo's own options.
sub="$1"
shift
exec cargo "$sub" --target "$target" "$@"
