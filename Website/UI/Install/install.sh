#!/bin/sh
# RuneAgent installer for Linux and macOS — Atoryn Studio
#
#   curl -fsSL {{.Origin}}{{.Base}}/install.sh | sh
#
# Installs the newest signed release of `rune` for this machine into
# ~/.local/bin (no root, no system change). Every download is verified
# against the release signing key below BEFORE anything is installed; any
# mismatch stops the installer with nothing changed. Windows has its own
# installer: {{.Origin}}{{.Base}}/install.ps1
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

say() { printf '%s\n' "$*"; }
fail() { printf 'rune installer: %s\n' "$*" >&2; exit 1; }

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

main() {
	set -eu
	umask 022

	[ -n "$SIGNING_KEY" ] && [ -n "$LATEST" ] || fail "no release has been published yet — see $ORIGIN$BASE/"
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

	dir=${RUNE_INSTALL_DIR:-$HOME/.local/bin}
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

	say "RuneAgent $version for $name_system $name_arch"
	fetch "$url/SHA256SUMS" "$tmp/SHA256SUMS"
	fetch "$url/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig"
	printf '%s\n' "$SIGNING_KEY" >"$tmp/signing-key.pem"

	# 1. The checksum list must carry the release key's signature (ECDSA P-256
	#    over SHA-256, which OpenSSL and the LibreSSL of macOS both check).
	openssl dgst -sha256 -verify "$tmp/signing-key.pem" -signature "$tmp/SHA256SUMS.sig" \
		"$tmp/SHA256SUMS" >/dev/null 2>&1 ||
		fail "SIGNATURE CHECK FAILED for $version — nothing was installed. Do not use these files."
	say "  signature verified (key $KEY_FINGERPRINT)"

	# 2. The archive must be in that signed list with exactly that hash.
	expected=$(tr -d '\r' <"$tmp/SHA256SUMS" | awk -v n="$name" '$2 == n { print $1 }')
	[ -n "$expected" ] || fail "no build for $name_system $name_arch in release $version"
	say "  downloading $name..."
	fetch_archive "$url/$name" "$tmp/$name"
	actual=$(sha256_of "$tmp/$name")
	[ "$actual" = "$expected" ] || fail "CHECKSUM MISMATCH for $name — nothing was installed. Do not use this file."
	say "  checksum verified"

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

	say "Installed $dir/rune ($version). License and notices: $docs"
	case ":$PATH:" in
	*":$dir:"*)
		say "Start it in a project folder with: rune"
		;;
	*)
		configured=0
		case "$dir" in
		/tmp/* | */scratch/* | */tmp.* | */target/*) ;;
		"$HOME"/*)
			for rc in "$HOME/.zprofile" "$HOME/.zshrc" "$HOME/.bash_profile" "$HOME/.bashrc" "$HOME/.profile"; do
				if [ -f "$rc" ] && [ -w "$rc" ]; then
					if ! grep -q "\$HOME/\.local/bin" "$rc" 2>/dev/null && ! grep -q "$dir" "$rc" 2>/dev/null; then
						printf '\n# Added by RuneAgent installer\nexport PATH="%s:$PATH"\n' "$dir" >>"$rc"
						say "  configured PATH in $rc"
						configured=1
					fi
				fi
			done
			fish_cfg="$HOME/.config/fish/config.fish"
			if [ -d "$HOME/.config/fish" ] || command -v fish >/dev/null 2>&1; then
				mkdir -p "$HOME/.config/fish" 2>/dev/null || true
				if [ -f "$fish_cfg" ] || [ -d "$HOME/.config/fish" ]; then
					if [ ! -f "$fish_cfg" ] || ! grep -q "$dir" "$fish_cfg" 2>/dev/null; then
						printf '\n# Added by RuneAgent installer\nfish_add_path %s\n' "$dir" >>"$fish_cfg" 2>/dev/null && {
							say "  configured PATH in $fish_cfg"
							configured=1
						} || true
					fi
				fi
			fi
			;;
		esac
		if [ "$configured" -eq 1 ]; then
			say "Added $dir to your PATH in your shell configuration."
			say "To start in this shell session: export PATH=\"$dir:\$PATH\""
			say "Or open a new terminal and run: rune"
		else
			say "Add $dir to your PATH, then start it in a project folder with: rune"
		fi
		;;
	esac
}

main "$@"
