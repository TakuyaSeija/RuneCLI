#!/bin/sh
# RuneAgent installer for Linux and macOS - Atoryn Studio
#
#   curl -fsSL {{.Origin}}{{.Base}}/install.sh | sh
#   . <(curl -fsSL {{.Origin}}{{.Base}}/install.sh)     # bash and zsh: ready here
#
# Installs the newest signed release of `rune` for this machine into
# ~/.local/bin (no root, no system change). Every download is verified
# against the release signing key below BEFORE anything is installed; any
# mismatch stops the installer with nothing changed.
#
# It never edits a shell profile or PATH. To let the window you already
# have run `rune` at once it links the binary into a folder that is
# already on PATH, or, when it cannot, names the single line that
# finishes the job. Run it as a source (the second form above) and it
# adds the folder to that window itself.
#
# Windows has its own installer: {{.Origin}}{{.Base}}/install.ps1
#
# Options (environment): RUNE_VERSION=X.Y.Z  RUNE_INSTALL_DIR=/abs/dir
#                        RUNE_LIBC=gnu|musl (Linux only)
#
# Everything runs inside main(), called on the last line: a download cut
# short can never execute half a script.

ORIGIN={{shq .Origin}}
BASE={{shq .Base}}
LATEST={{shq .Version}}
KEY_FINGERPRINT={{shq .Fingerprint}}
{{- if .Ready}}
SIGNING_KEY={{shq .KeyPEM}}
{{- else}}
SIGNING_KEY=''
{{- end}}

# Colour is for a person reading a terminal; a pipe, a log or a test gets plain
# lines. Written as escapes, so this file stays ASCII and both installers read
# the same everywhere. NO_COLOR and a dumb terminal switch it off.
esc=$(printf '\033')
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != dumb ]; then
	bold="${esc}[1m" dim="${esc}[2m" red="${esc}[31m" green="${esc}[32m" yellow="${esc}[33m" cyan="${esc}[36m" off="${esc}[0m"
else
	bold='' dim='' red='' green='' yellow='' cyan='' off=''
fi

say() { printf '%s\n' "$*"; }
step() { printf '  %sok%s  %s\n' "$green" "$off" "$*"; }
note() { printf '  %s--%s  %s\n' "$dim" "$off" "$*"; }
warn() { printf '  %s!%s   %s\n' "$yellow" "$off" "$*"; }
fail() { printf '%srune installer:%s %s\n' "$red" "$off" "$*" >&2; exit 1; }

fetch() { # url dest
	case "$ORIGIN" in
	https://*) curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 1 -o "$2" "$1" ;;
	*) curl -fsSL --retry 3 --retry-delay 1 -o "$2" "$1" ;; # development server only
	esac || fail "download failed: $1"
}

fetch_archive() { # url dest
	if [ -t 2 ] || [ -t 1 ]; then
		case "$ORIGIN" in
		https://*) curl --proto '=https' --tlsv1.2 -fL --progress-bar --retry 3 --retry-delay 1 -o "$2" "$1" ;;
		*) curl -fL --progress-bar --retry 3 --retry-delay 1 -o "$2" "$1" ;; # development server only
		esac
	else
		fetch "$1" "$2"
	fi || fail "download failed: $1"
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		openssl dgst -sha256 -r "$1" | cut -d' ' -f1
	fi
}

install_dir() { printf '%s' "${RUNE_INSTALL_DIR:-$HOME/.local/bin}"; }

on_path() { # dir
	case ":$PATH:" in
	*":$1:"*) return 0 ;;
	*) return 1 ;;
	esac
}

# try_link dir name install-dir: point dir/name at the installed binary. Only a
# folder that is already on PATH helps this window, the link never replaces a
# regular file, and an older link of ours is refreshed.
try_link() {
	[ -n "$1" ] || return 1
	case "$1" in /*) ;; *) return 1 ;; esac
	[ "$1" != "$3" ] || return 1
	[ -d "$1" ] && [ -w "$1" ] || return 1
	on_path "$1" || return 1
	[ ! -e "$1/$2" ] || [ -L "$1/$2" ] || return 1
	ln -sf "$3/$2" "$1/$2" 2>/dev/null || return 1
	return 0
}

# link_dir name install-dir: a folder to link into, or nothing. The system bin
# (where an administrator account may write, as on macOS), the Homebrew bin and
# the person's own ~/bin come first; only then any other folder on PATH, so Rune
# never lands in another tool's folder while a fitting one is free.
link_dir() {
	for candidate in /usr/local/bin /opt/homebrew/bin "$HOME/bin"; do
		try_link "$candidate" "$1" "$2" && { printf '%s' "$candidate"; return 0; }
	done
	rest=$PATH
	while [ -n "$rest" ]; do
		case "$rest" in
		*:*)
			candidate=${rest%%:*}
			rest=${rest#*:}
			;;
		*)
			candidate=$rest
			rest=
			;;
		esac
		while [ "${candidate%/}" != "$candidate" ]; do candidate=${candidate%/}; done
		try_link "$candidate" "$1" "$2" && { printf '%s' "$candidate"; return 0; }
	done
	printf '%s' ''
}

ready() {
	printf '\n%sReady.%s Start it in a project folder with: %s%srune%s\n' "$bold" "$off" "$bold" "$cyan" "$off"
}

# finish dir: say how this window starts rune, and never edit a profile.
finish() {
	if on_path "$1"; then
		ready
		return 0
	fi
	linked=$(link_dir rune "$1")
	if [ -n "$linked" ]; then
		step "linked as $linked/rune, so this window can run it"
		ready
		return 0
	fi
	# Sourced: main adds the folder to the calling shell right after this.
	[ "$sourced" -eq 0 ] || return 0
	warn "$1 is not on your PATH, so this window cannot run rune on its own yet."
	say ""
	printf '%sFinish in this window, no new terminal needed:%s\n' "$bold" "$off"
	say "  export PATH=\"$1:\$PATH\""
	say "  rune --version"
	say ""
	say "A terminal you open later finds rune on its own once that folder is on your PATH:"
	say "add the same line to your shell profile yourself, this installer never edits it."
	say ""
	say "Next time, install as a source and this step disappears:"
	say "  bash, zsh:  . <(curl -fsSL $ORIGIN$BASE/install.sh)"
	say "  any shell:  curl -fsSL $ORIGIN$BASE/install.sh -o rune-install.sh && . ./rune-install.sh"
	return 0
}

install_rune() {
	set -eu
	umask 022

	[ -n "$SIGNING_KEY" ] && [ -n "$LATEST" ] || fail "no release has been published yet - see $ORIGIN$BASE/"
	[ "$(id -u)" -ne 0 ] || fail "run this as your normal user, not root: RuneAgent installs into your home folder"
	case "$(uname -s)" in
	Linux) system=linux name_system=Linux ;;
	Darwin) system=macos name_system=macOS ;;
	*) fail "this installer is for Linux and macOS; on Windows use $ORIGIN$BASE/install.ps1" ;;
	esac
	command -v curl >/dev/null 2>&1 || fail "curl is required"
	command -v tar >/dev/null 2>&1 || fail "tar is required"
	command -v gzip >/dev/null 2>&1 || fail "gzip is required"
	command -v openssl >/dev/null 2>&1 ||
		fail "openssl is required to verify the release signature (install your system's openssl package)"

	case "$(uname -m)" in
	x86_64 | amd64) arch=x86_64 ;;
	aarch64 | arm64) arch=aarch64 ;;
	*) fail "unsupported processor: $(uname -m)" ;;
	esac

	if [ "$system" = macos ]; then
		major=$(sw_vers -productVersion 2>/dev/null | cut -d. -f1)
		[ -n "$major" ] && [ "$major" -ge 11 ] 2>/dev/null || fail "RuneAgent needs macOS 11 or later"
		# A shell running under Rosetta reports x86_64 on Apple silicon; the native build is the right one.
		if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
			arch=aarch64
		fi
		[ -z "${RUNE_LIBC:-}" ] || fail "RUNE_LIBC applies to Linux only"
		if [ "$arch" = aarch64 ]; then name_arch="Apple silicon"; else name_arch=Intel; fi
	else
		libc=${RUNE_LIBC:-}
		if [ -z "$libc" ]; then
			if ls /lib/ld-musl-*.so.1 >/dev/null 2>&1 && ! ls /lib*/ld-linux-*.so.* >/dev/null 2>&1; then
				libc=musl
			else
				libc=gnu
			fi
		fi
		case "$libc" in gnu | musl) ;; *) fail "RUNE_LIBC must be gnu or musl" ;; esac
		name_arch="$arch ($libc)"
	fi

	version=${RUNE_VERSION:-$LATEST}
	printf '%s' "$version" | grep -Eq '^(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})$' ||
		fail "RUNE_VERSION must look like 1.2.3"

	dir=$(install_dir)
	case "$dir" in /*) ;; *) fail "RUNE_INSTALL_DIR must be an absolute path" ;; esac

	if [ "$system" = macos ]; then
		name="rune-$version-macos-$arch.tar.gz"
	else
		name="rune-$version-linux-$arch-$libc.tar.gz"
	fi
	url="$ORIGIN$BASE/download/$version"

	tmp=$(mktemp -d 2>/dev/null || mktemp -d -t rune-install)
	trap 'rm -rf "$tmp"' EXIT
	trap 'exit 130' INT TERM

	printf '%sRuneAgent %s for %s %s%s\n' "$bold" "$version" "$name_system" "$name_arch" "$off"
	note "checking the signed release list"
	fetch "$url/SHA256SUMS" "$tmp/SHA256SUMS"
	fetch "$url/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig"
	printf '%s\n' "$SIGNING_KEY" >"$tmp/signing-key.pem"

	# 1. The checksum list must carry the release key's signature (ECDSA P-256
	#    over SHA-256, which OpenSSL and the LibreSSL of macOS both check).
	openssl dgst -sha256 -verify "$tmp/signing-key.pem" -signature "$tmp/SHA256SUMS.sig" \
		"$tmp/SHA256SUMS" >/dev/null 2>&1 ||
		fail "SIGNATURE CHECK FAILED for $version - nothing was installed. Do not use these files."
	step "signature verified (key $KEY_FINGERPRINT)"

	# 2. The archive must be in that signed list with exactly that hash.
	expected=$(tr -d '\r' <"$tmp/SHA256SUMS" | awk -v n="$name" '$2 == n { print $1 }')
	[ -n "$expected" ] || fail "no build for $name_system $name_arch in release $version"
	note "downloading $name"
	fetch_archive "$url/$name" "$tmp/$name"
	actual=$(sha256_of "$tmp/$name")
	[ "$actual" = "$expected" ] || fail "CHECKSUM MISMATCH for $name - nothing was installed. Do not use this file."
	step "checksum verified"

	# 3. The archive holds only plain files with plain names.
	tar -tzf "$tmp/$name" >"$tmp/list" || fail "the archive is damaged"
	while IFS= read -r entry; do
		case "$entry" in
		rune | LICENSE.txt | THIRD-PARTY-NOTICES.txt) ;;
		*) fail "unexpected file in the archive: $entry" ;;
		esac
	done <"$tmp/list"
	mkdir "$tmp/x"
	tar -xzf "$tmp/$name" -C "$tmp/x" --no-same-owner
	[ -f "$tmp/x/rune" ] && [ ! -L "$tmp/x/rune" ] || fail "the archive holds no rune binary"
	chmod 0755 "$tmp/x/rune"
	"$tmp/x/rune" --version 2>/dev/null | grep -q "$version" || fail "the downloaded binary does not run on this system"

	# 4. Install in one rename: a running RuneAgent takes the new build over when idle.
	mkdir -p "$dir"
	cp "$tmp/x/rune" "$dir/rune.new.$$"
	mv -f "$dir/rune.new.$$" "$dir/rune"
	docs="${XDG_DATA_HOME:-$HOME/.local/share}/runeagent"
	mkdir -p "$docs"
	for f in LICENSE.txt THIRD-PARTY-NOTICES.txt; do
		[ ! -f "$tmp/x/$f" ] || cp "$tmp/x/$f" "$docs/$f"
	done
	step "installed $dir/rune ($version)"
	note "license and notices: $docs"

	finish "$dir"
}

# Sourced or executed? bash and zsh report it directly. A POSIX shell (dash,
# ash, ksh, mksh) has no such marker: it is sourced when an interactive shell
# reads this text from its terminal, which `$-` (the `i` flag) and a terminal on
# stdin both tell us. A pipe (curl | sh) has no terminal on stdin, `sh
# install.sh` has no `i` and finds this very file in $0, and a non-interactive
# script that sources us cannot pass anything on anyway: none of those three is
# mistaken for a source.
sourced=0
if [ -n "${BASH_SOURCE:-}" ] && [ "${BASH_SOURCE[0]:-$0}" != "$0" ]; then
	sourced=1
elif [ -n "${ZSH_VERSION:-}" ]; then
	case "${ZSH_EVAL_CONTEXT:-}" in
	*:file*) sourced=1 ;;
	esac
else
	own=0
	if [ -f "$0" ] && grep -q "installer for Linux and macOS" "$0" 2>/dev/null; then
		own=1
	fi
	case "$-" in
	*i*) [ -t 0 ] && [ "$own" -eq 0 ] && sourced=1 ;;
	esac
fi

main() {
	# Sourced (". install.sh")? The work runs in a subshell, so no option, umask,
	# trap or failed step can reach the calling shell, and the folder is added to
	# that shell's PATH afterwards: the window that ran the installer is ready.
	if [ "$sourced" = 1 ]; then
		dir=$(install_dir)
		( install_rune ) || return 1
		if ! on_path "$dir"; then
			PATH="$dir:$PATH"
			export PATH
		fi
		case "$-" in
		*i*) # an interactive shell read this file: that window is ready
			step "added $dir to PATH in this window"
			ready
			;;
		*) # a script sourced it: the script has the folder, the person's own
			# shell does not, and saying otherwise would be a lie
			step "added $dir to PATH for the script that sourced this installer"
			say "Your own shell still needs the one line:"
			say "  export PATH=\"$dir:\$PATH\""
			;;
		esac
		return 0
	fi
	# Executed: still a subshell, so no shell that sourced this file can be
	# closed by a failure, while the exit status reaches the caller unchanged.
	( install_rune )
	return $?
}
main "$@"
