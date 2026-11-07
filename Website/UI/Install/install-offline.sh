#!/bin/sh
# RuneAgent offline installer for Linux and macOS - Atoryn Studio
#
#   sh ./install.sh          run it from inside the unpacked package
#   . ./install.sh           bash and zsh: also ready in this window
#
# Installs the `rune` binary that sits beside this script into ~/.local/bin (no
# root, no system change). It downloads nothing: everything comes from this
# package. Before anything is installed every file is checked against the
# package's own SHA256SUMS, so a damaged or half-copied package stops here with
# nothing changed.
#
# What the checksum proves, honestly: the files belong together and match the
# package you hold. It does not prove who built the package - that comes from
# where you got it. The published package's own sha256 is listed in Atoryn's
# signed release, and that list can be verified against the release key.
#
# It never edits a shell profile or PATH. To let the window you already have run
# `rune` at once it links the binary into a folder that is already on PATH, or,
# when it cannot, names the single line that finishes the job. Run it as a source
# (the second form above) and it adds the folder to that window itself.
#
# Options (environment): RUNE_INSTALL_DIR=/abs/dir
#
# Everything runs inside main(), called on the last line: a damaged package can
# never execute half a script.

# Colour only for a person reading a terminal; a pipe, a log or a test gets
# plain lines. Escapes are written out, so this file stays ASCII.
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
fail() { printf '%srune offline installer:%s %s\n' "$red" "$off" "$*" >&2; exit 1; }

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	elif command -v openssl >/dev/null 2>&1; then
		openssl dgst -sha256 -r "$1" | cut -d' ' -f1
	else
		fail "no sha256 tool found (sha256sum, shasum or openssl)"
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
	[ "${sourced:-0}" -eq 0 ] || return 0
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
	say "  . ./install.sh"
	return 0
}

package_dir() {
	# Sourced, $0 is the calling shell: the package folder is where the person
	# stands, which is where the package was unpacked.
	if [ "${sourced:-0}" = 1 ]; then
		pwd
	else
		CDPATH= cd -- "$(dirname -- "$0")" && pwd
	fi
}

install_rune() {
	set -eu
	umask 022

	package=$(package_dir) || fail "cannot locate the package folder"
	[ "$(id -u)" -ne 0 ] || fail "run this as your normal user, not root: RuneAgent installs into your home folder"
	case "$(uname -s)" in
	Linux | Darwin) ;;
	*) fail "this package is for Linux and macOS; on Windows use the package's install.ps1" ;;
	esac
	[ -f "$package/rune" ] || fail "this folder holds no rune binary - unpack the whole package first, and run the installer from inside it"
	[ -f "$package/SHA256SUMS" ] || fail "this folder holds no SHA256SUMS - unpack the whole package first"

	printf '%sRuneAgent offline package%s\n' "$bold" "$off"
	note "checking every file against the package checksums"

	# 1. Every listed file must match its hash, and the list must cover rune.
	checked=0
	while IFS= read -r line; do
		line=$(printf '%s' "$line" | tr -d '\r')
		[ -n "$line" ] || continue
		# sha256sum writes two spaces between the hash and the name; awk
		# splits on a run of whitespace, cut would not.
		hash=$(printf '%s' "$line" | awk '{print $1}')
		name=$(printf '%s' "$line" | awk '{print $2}')
		case "$name" in
		rune | install.sh | INSTALL.txt | LICENSE.txt | THIRD-PARTY-NOTICES.txt) ;;
		*) fail "unexpected file in the checksum list: $name" ;;
		esac
		if [ "$name" = rune ]; then
			checked=1
			[ ! -L "$package/rune" ] || fail "the rune binary in this package is a link, not a file"
		fi
		[ -f "$package/$name" ] || fail "missing from the package: $name"
		actual=$(sha256_of "$package/$name")
		[ "$actual" = "$hash" ] ||
			fail "CHECKSUM MISMATCH for $name - nothing was installed. Do not use this package."
	done <"$package/SHA256SUMS"
	[ "$checked" = 1 ] || fail "the checksum list does not cover the rune binary"
	step "every file matches the package checksums"

	# 2. The binary must run here and say what it is. Its executable bit may not
	#    have survived the unpacking (a zip tool on Windows, a GUI unzipper), so
	#    it is set before anything tries to run the file.
	chmod 0755 "$package/rune" 2>/dev/null || true
	version=$("$package/rune" --version 2>/dev/null) || fail "the rune binary in this package does not run on this system"
	step "$version (offline package)"

	dir=$(install_dir)
	case "$dir" in /*) ;; *) fail "RUNE_INSTALL_DIR must be an absolute path" ;; esac

	# 3. Install in one rename: a running RuneAgent takes the new build over when idle.
	mkdir -p "$dir"
	cp "$package/rune" "$dir/rune.new.$$"
	mv -f "$dir/rune.new.$$" "$dir/rune"
	docs="${XDG_DATA_HOME:-$HOME/.local/share}/runeagent"
	mkdir -p "$docs"
	for f in LICENSE.txt THIRD-PARTY-NOTICES.txt; do
		[ ! -f "$package/$f" ] || cp "$package/$f" "$docs/$f"
	done
	step "installed $dir/rune ($version)"
	note "license and notices: $docs"

	finish "$dir"
}

# Sourced or executed? bash and zsh report it directly. A POSIX shell (dash,
# ash, ksh, mksh) has no such marker: it is sourced when an interactive shell
# reads this text from its terminal, which `$-` (the `i` flag) and a terminal on
# stdin both tell us. A pipe has no terminal on stdin, `sh install.sh` has no `i`
# and finds this very file in $0, and a non-interactive script that sources us
# cannot pass anything on anyway: none of those three is mistaken for a source.
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
	# Sourced (". ./install.sh")? The work runs in a subshell, so no option, umask,
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
