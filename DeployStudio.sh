#!/usr/bin/env bash
# DeployStudio.sh — deploy Atoryn Studio, dari root proyek (seperti WebsiteDev.sh).
#
#   DeployStudio.sh plan            apa yang akan dilakukan, urutannya (read-only)
#   DeployStudio.sh status          release situs, kesehatan, versi rune terbit
#   DeployStudio.sh rune            bangun rilis Linux + terbitkan rilis CLI rune
#   DeployStudio.sh website         pasang unit (sekali) → release → switch
#   DeployStudio.sh all             rune, lalu website
#   DeployStudio.sh build           hanya bangun rilis Linux (gnu + musl)
#   DeployStudio.sh release|switch|rollback|stop-standby   situs saja
#
#   --dry-run   (atau STUDIO_DEPLOY_DRY_RUN=1) berlaku untuk SEMUA sub-command:
#               setiap pemeriksaan read-only tetap dijalankan, setiap langkah
#               yang mengubah dicetak — tidak menulis, membangun, menandatangani,
#               me-restart, atau menerbitkan apa pun.
#
# Yang dipanggil (satu sumber, tidak ada logika deploy kedua di sini):
#   Ops/Release/PublishRune.sh   rilis CLI (interaktif: meminta passphrase kunci)
#   Ops/Web/Install.sh           unit systemd situs (root, sekali)
#   Ops/Web/StudioWeb.sh         rilis + blue/green situs
#   scripts/build_linux_release.sh   build rilis Linux (butuh Docker)
set -uo pipefail

ROOT="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
WEB="$ROOT/Ops/Web"
REL="$ROOT/Ops/Release"
DRY_RUN="${STUDIO_DEPLOY_DRY_RUN:-0}"
SUDO="${SUDO:-sudo}"

die() { echo "DeployStudio: $*" >&2; exit 1; }
dry_note() { printf '[dry-run] %s\n' "$*"; }
run() { if [ "$DRY_RUN" = 1 ]; then dry_note "$*"; return 0; fi; "$@"; }

help() { awk 'NR>1 && /^#/ {print; next} NR>1 {exit}' "$0"; exit 0; }

usage() { awk 'NR>1 && /^#/ {print; next} NR>1 {exit}' "$0"; exit 2; }

# Teruskan --dry-run ke skrip yang dipanggil, dalam bahasa flag masing-masing.
web() {  # web <perintah StudioWeb.sh>
    if [ "$DRY_RUN" = 1 ]; then STUDIO_WEB_DRY_RUN=1 "$WEB/StudioWeb.sh" "$@"
    else "$WEB/StudioWeb.sh" "$@"; fi
}

unit_installed() { systemctl list-unit-files 'atoryn-studio-web@.service' 2>/dev/null | grep -q 'atoryn-studio-web@'; }

version() { sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\([^"]*\)".*/\1/p}' "$ROOT/Cargo.toml" | head -1; }

published() {  # versi yang sudah terbit di Downloads
    local dl="${RUNE_PUBLISH_DOWNLOADS:-/home/noire/.data/Atoryn/Studio/Releases}"
    [ -d "$dl" ] || return 0
    (cd "$dl" 2>/dev/null && ls -d [0-9]*.[0-9]*.[0-9]* 2>/dev/null | sort -V)
}

cmd_plan() {
    echo "Rencana deploy Atoryn Studio (rune $(version)):"
    echo "  1. rune     bangun rilis Linux (gnu, musl) yang belum ada, lalu PublishRune.sh"
    echo "              menerbitkan ~/.data/Atoryn/Studio/Releases/<versi>/ (bertanda tangan)"
    echo "  2. website  Ops/Web/Install.sh (unit systemd, sekali) → StudioWeb.sh release"
    echo "              (membekukan Website/ yang sudah di-commit) → switch ke warna idle"
    echo "  3. verifikasi /Studio/ lewat nginx; sesudahnya: stop-standby"
    echo
    echo "Keadaan sekarang:"
    echo "  versi workspace   : $(version)"
    echo "  rilis terbit      : $(published | tr '\n' ' ' | sed 's/ $//' || echo '(belum ada)')"
    echo "  unit situs        : $(unit_installed && echo 'terpasang' || echo 'BELUM terpasang')"
    echo "  Website/ di git   : $(git -C "$ROOT" status --porcelain -- Website | wc -l) berkas belum di-commit"
    echo "  build gnu         : $([ -x "$ROOT/target/linux-release/gnu-x86_64/release/rune" ] && echo ada || echo 'belum ada')"
    echo "  build musl        : $([ -x "$ROOT/target/linux-release/musl-x86_64/release/rune" ] && echo ada || echo 'belum ada')"
    echo "  kunci tanda tangan: $([ -f /home/noire/.local/share/atoryn/studio-signing/SigningKey.enc ] && echo ada || echo 'BELUM ada (keygen sekali)')"
}

cmd_status() {
    echo "== situs /Studio/"
    web status
    echo "== rilis CLI rune"
    echo "  versi workspace: $(version)"
    local list; list="$(published)"
    if [ -n "$list" ]; then echo "  terbit: $(echo "$list" | tr '\n' ' ')"; else echo "  terbit: (belum ada rilis CLI)"; fi
}

cmd_build() {
    local libc bin
    for libc in gnu musl; do
        bin="$ROOT/target/linux-release/$libc-x86_64/release/rune"
        if [ -x "$bin" ] && [ "$("$bin" --version 2>/dev/null)" = "rune $(version)" ]; then
            echo "  build $libc sudah ada dan versinya benar"
            continue
        fi
        if [ "$DRY_RUN" = 1 ]; then
            dry_note "would build the Linux $libc release: scripts/build_linux_release.sh $libc (needs Docker)"
            continue
        fi
        echo "  membangun Linux $libc (Docker, lama)..."
        "$ROOT/scripts/build_linux_release.sh" "$libc" || die "build $libc gagal"
    done
}

cmd_rune() {
    cmd_build || return 1
    if [ "$DRY_RUN" = 1 ]; then
        RUNE_PUBLISH_DRY_RUN=1 "$REL/PublishRune.sh"
    else
        [ -t 0 ] || die "rune perlu terminal: PublishRune.sh meminta passphrase kunci"
        "$REL/PublishRune.sh" || die "publish rune gagal"
    fi
}

cmd_website() {
    if ! unit_installed; then
        if [ "$DRY_RUN" = 1 ]; then
            dry_note "would install the systemd units once: $SUDO $WEB/Install.sh"
        else
            echo "  memasang unit systemd (sekali)..."
            "$SUDO" "$WEB/Install.sh" || die "Install.sh gagal"
        fi
    fi
    web release || return 1
    web switch || return 1
    if [ "$DRY_RUN" = 1 ]; then
        dry_note "after a real switch: check /Studio/ through nginx, then: DeployStudio.sh stop-standby"
    fi
}

cmd_all() { cmd_rune || return 1; cmd_website; }

# --dry-run bisa di mana saja; perintah pertama yang bukan flag.
ARGS=()
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        *) ARGS+=("$arg") ;;
    esac
done
set -- ${ARGS[@]+"${ARGS[@]}"}
[ $# -gt 0 ] || usage

case "$1" in
    plan) cmd_plan ;;
    status) cmd_status ;;
    build) cmd_build ;;
    rune) cmd_rune ;;
    website) cmd_website ;;
    all) cmd_all ;;
    release|switch|rollback|stop-standby) web "$@" ;;
    help|-h|--help) help ;;
    *) die "unknown command: $1 (lihat: DeployStudio.sh help)" ;;
esac
