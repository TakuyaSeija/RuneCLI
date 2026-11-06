#!/bin/sh
# RuneAgent offline installer for Linux and macOS — Atoryn Studio
#
#   sh ./install.sh
#
# Installs the `rune` binary that sits beside this script into ~/.local/bin (no
# root, no system change). It downloads nothing: everything comes from this
# package. Before anything is installed every file is checked against the
# package's own SHA256SUMS, so a damaged or half-copied package stops here with
# nothing changed.
#
# What the checksum proves, honestly: the files belong together and match the
# package you hold. It does not prove who built the package — that comes from
# where you got it. The published package's own sha256 is listed in Atoryn's
# signed release, and that list can be verified against the release key.
#
# Options (environment): RUNE_INSTALL_DIR=/abs/dir
#
# Everything runs inside main(), called on the last line: a damaged package can
# never execute half a script.

say() { printf '%s\n' "$*"; }
fail() { printf 'rune offline installer: %s\n' "$*" >&2; exit 1; }

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

main() {
	set -eu
	umask 022

	dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || fail "cannot locate the package folder"
	[ "$(id -u)" -ne 0 ] || fail "run this as your normal user, not root: RuneAgent installs into your home folder"
	case "$(uname -s)" in
	Linux | Darwin) ;;
	*) fail "this package is for Linux and macOS; on Windows use the package's install.ps1" ;;
	esac
	[ -f "$dir/rune" ] || fail "this folder holds no rune binary — unpack the whole package first"
	[ -f "$dir/SHA256SUMS" ] || fail "this folder holds no SHA256SUMS — unpack the whole package first"

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
			[ ! -L "$dir/rune" ] || fail "the rune binary in this package is a link, not a file"
		fi
		[ -f "$dir/$name" ] || fail "missing from the package: $name"
		actual=$(sha256_of "$dir/$name")
		[ "$actual" = "$hash" ] ||
			fail "CHECKSUM MISMATCH for $name — nothing was installed. Do not use this package."
	done <"$dir/SHA256SUMS"
	[ "$checked" = 1 ] || fail "the checksum list does not cover the rune binary"
	say "  every file matches the package checksums"

	# 2. The binary must run here and say what it is. Its executable bit may not
	#    have survived the unpacking (a zip tool on Windows, a GUI unzipper), so
	#    it is set before anything tries to run the file.
	chmod 0755 "$dir/rune" 2>/dev/null || true
	version=$("$dir/rune" --version 2>/dev/null) || fail "the rune binary in this package does not run on this system"
	say "  $version (offline package)"
	install_dir=${RUNE_INSTALL_DIR:-$HOME/.local/bin}
	case "$install_dir" in /*) ;; *) fail "RUNE_INSTALL_DIR must be an absolute path" ;; esac

	# 3. Install in one rename: a running RuneAgent takes the new build over when idle.
	mkdir -p "$install_dir"
	cp "$dir/rune" "$install_dir/rune.new.$$"
	mv -f "$install_dir/rune.new.$$" "$install_dir/rune"
	docs="${XDG_DATA_HOME:-$HOME/.local/share}/runeagent"
	mkdir -p "$docs"
	for f in LICENSE.txt THIRD-PARTY-NOTICES.txt; do
		[ ! -f "$dir/$f" ] || cp "$dir/$f" "$docs/$f"
	done

	say "Installed $install_dir/rune ($version). License and notices: $docs"
	case ":$PATH:" in
	*":$install_dir:"*)
		say "Start it with: rune"
		;;
	*)
		# Same rule as the online installer: never edit a shell profile or
		# PATH (docs/website.md). When the install folder is not on PATH,
		# link the binary into the first writable folder that already is,
		# so this session can run it at once.
		link_dir=""
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
			[ -n "$candidate" ] || continue
			case "$candidate" in /*) ;; *) continue;; esac
			if [ -d "$candidate" ] && [ -w "$candidate" ] && [ "$candidate" != "$install_dir" ]; then
				if [ ! -e "$candidate/rune" ] || [ -L "$candidate/rune" ]; then
					if ln -sf "$install_dir/rune" "$candidate/rune" 2>/dev/null; then
						link_dir=$candidate
						break
					fi
				fi
			fi
		done
		if [ -n "$link_dir" ]; then
			say "Also linked as $link_dir/rune, so this session can run: rune --version"
		else
			say "$install_dir is not on your PATH, so rune cannot start yet. Run it by its full path:"
			say "  $install_dir/rune --version"
			say "or add the folder for this session only:"
			say "  export PATH=\"$install_dir:\$PATH\""
		fi
		;;
	esac
}

main "$@"
