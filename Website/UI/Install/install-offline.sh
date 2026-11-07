#!/bin/sh
# RuneAgent offline installer for Linux and macOS - Atoryn Studio
#
#   sh ./install.sh          run it from inside the unpacked package
#   . ./install.sh           bash and zsh: ready here when a folder can take it
#
# Installs the `rune` binary that sits beside this script into ~/.local/bin (no
# root, no system change). It downloads nothing: everything comes from this
# package, and every file is checked against the package's own SHA256SUMS before
# anything is installed, so a damaged or half-copied package stops here with
# nothing changed.
#
# What the checksum proves, honestly: the files belong together and match the
# package you hold. It does not prove who built it - that comes from where you got
# the package.
#
# It never edits a shell profile or PATH. To let the window you already have run
# `rune` at once it links the binary into a folder that is already on PATH. When
# it cannot, it looks at your shell setup and prints the one command that finishes
# the job: add the export, then refresh the terminal. Run it as a source (the
# second form above) and it adds the folder to that window itself.
#
# In a terminal you get colour and a spinner. In a pipe, a log or a test you get
# plain lines, no colour and no redraws. NO_COLOR drops the colour and keeps the
# rest.
#
# Options (environment): RUNE_INSTALL_DIR=/abs/dir
#
# Everything runs inside main(), called on the last line: a damaged package can
# never execute half a script.

# ---------------------------------------------------------------------------
# Terminal: colour, glyphs, animation
#
# One accent colour (terracotta) means "work in progress": the spinner, solid,
# never a gradient. Green is only ever a finished step, yellow only a warning,
# red only a failure. Anything the person has to TYPE is set in bold between two
# thin rules (see cmdline), with no background, so it cannot be mistaken for
# selected text. It is the only such line on its screen and always the LAST thing
# printed there, because that is where people look.
#
# Glyphs are written as octal escapes, so this file stays ASCII and both
# installers read the same everywhere. A terminal that is not UTF-8 gets plain
# ASCII glyphs; a pipe, a log or a dumb terminal gets no colour and no redraws.
# ---------------------------------------------------------------------------
tty=0
if [ -t 1 ] && [ "${TERM:-}" != dumb ]; then tty=1; fi

utf=0
if [ "$tty" -eq 1 ]; then
	case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
	*[Uu][Tt][Ff]-8* | *[Uu][Tt][Ff]8*) utf=1 ;;
	esac
fi

esc=$(printf '\033')
bold='' dim='' red='' green='' yellow='' acc='' off=''
clr='' curs_off='' curs_on=''
if [ "$tty" -eq 1 ]; then
	clr="${esc}[K" curs_off="${esc}[?25l" curs_on="${esc}[?25h"
	# Bold, dim and reverse are attributes, not colour: NO_COLOR keeps them.
	bold="${esc}[1m" dim="${esc}[2m" off="${esc}[0m"
	acc=$bold
	if [ -z "${NO_COLOR:-}" ]; then
		red="${esc}[31m" green="${esc}[32m" yellow="${esc}[33m"
		case "${COLORTERM:-}${TERM:-}" in
		*truecolor* | *24bit* | *256color*)
			# terracotta (#d7875f)
			acc="${esc}[38;5;173m"
			;;
		esac
	fi
fi

# Fractional sleeps drive the animation; without them it runs at one frame a second.
tick=1
if [ "$tty" -eq 1 ] && sleep 0.01 2>/dev/null; then tick=0.1; fi

f0='' f1='' f2='' f3='' f4='' f5='' f6='' f7='' f8='' f9='' nframes=4
if [ "$utf" -eq 1 ]; then
	g_ok=$(printf '\342\234\223  ')
	g_note=$(printf '\342\200\242  ')
	g_warn='!  '
	g_bad=$(printf '\342\234\227  ')
	g_sp='  ' ind='     '
	g_empty=$(printf '\342\224\200')
	g_rule=$g_empty
	g_dot=$(printf '\302\267')
	f0=$(printf '\342\240\213') f1=$(printf '\342\240\231') f2=$(printf '\342\240\271')
	f3=$(printf '\342\240\270') f4=$(printf '\342\240\274') f5=$(printf '\342\240\264')
	f6=$(printf '\342\240\246') f7=$(printf '\342\240\247') f8=$(printf '\342\240\207')
	f9=$(printf '\342\240\217')
	nframes=10
else
	g_ok='ok  ' g_note='--  ' g_warn='!   ' g_bad='x   '
	g_sp='   ' ind='      '
	g_empty='.' g_dot='-' g_rule='-'
	f0='|' f1='/' f2='-' f3='\'
fi

say() { printf '%s\n' "$*"; }
step() { printf '  %s%s%s%s\n' "$green" "$g_ok" "$off" "$*"; }
note() { printf '  %s%s%s%s\n' "$dim" "$g_note" "$off" "$*"; }
warn() { printf '  %s%s%s%s\n' "$yellow" "$g_warn" "$off" "$*"; }
fail() {
	if [ "$tty" -eq 1 ]; then
		printf '\r%s%s' "$clr" "$curs_on"
		printf '  %s%s%srune offline installer:%s %s\n' "$red" "$g_bad" "$off$bold$red" "$off" "$*" >&2
	else
		printf 'rune offline installer: %s\n' "$*" >&2
	fi
	exit 1
}

# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------
file_size() { # file: bytes so far, 0 while it does not exist yet
	if [ -f "$1" ]; then wc -c <"$1" | tr -d ' '; else printf '0'; fi
}

human() { # bytes
	if [ "$1" -ge 1048576 ]; then
		printf '%d.%d MB' $(($1 / 1048576)) $(($1 % 1048576 * 10 / 1048576))
	elif [ "$1" -ge 1024 ]; then
		printf '%d KB' $(($1 / 1024))
	else
		printf '%d B' "$1"
	fi
}

rep() { # string count: print string count times
	rn=$2
	while [ "$rn" -gt 0 ]; do
		printf '%s' "$1"
		rn=$((rn - 1))
	done
}

tilde() { # path: ~/x when it lives under the home folder
	case "$1" in
	"$HOME") printf '~' ;;
	"$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;;
	*) printf '%s' "$1" ;;
	esac
}

homevar() { # path: $HOME/x when it lives under the home folder (for a shell line)
	case "$1" in
	"$HOME"/*) printf '$HOME/%s' "${1#"$HOME"/}" ;;
	*) printf '%s' "$1" ;;
	esac
}

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

package_dir() {
	# Sourced, $0 is the calling shell: the package folder is where the person
	# stands, which is where the package was unpacked.
	if [ "${sourced:-0}" = 1 ]; then
		pwd
	else
		CDPATH= cd -- "$(dirname -- "$0")" && pwd
	fi
}

# ---------------------------------------------------------------------------
# Animation: spinner and background jobs
# ---------------------------------------------------------------------------
pick_frame() { # index: sets $fr
	case $(($1 % nframes)) in
	0) fr=$f0 ;; 1) fr=$f1 ;; 2) fr=$f2 ;; 3) fr=$f3 ;; 4) fr=$f4 ;;
	5) fr=$f5 ;; 6) fr=$f6 ;; 7) fr=$f7 ;; 8) fr=$f8 ;; *) fr=$f9 ;;
	esac
}

# A job in the background leaves its exit status in $tmp/rc when it is done. The
# marker file is what the animation watches: it needs no signal and no reaping,
# so it behaves the same in dash, bash and zsh.
bg() { # command args...
	rm -f "$tmp/rc"
	(
		if "$@"; then st=0; else st=$?; fi
		printf '%s' "$st" >"$tmp/rc.tmp"
		mv -f "$tmp/rc.tmp" "$tmp/rc"
	) &
	bgpid=$!
}

reap() { # wait for the background job; its status is this function's status
	wait "$bgpid" 2>/dev/null || :
	bgpid=''
	rs=$(cat "$tmp/rc")
	return "$rs"
}

spin() { # message: animate until the background job is done
	sn=0
	printf '%s' "$curs_off"
	while [ ! -f "$tmp/rc" ]; do
		pick_frame "$sn"
		printf '\r  %s%s%s%s%s%s' "$acc" "$fr" "$g_sp" "$off" "$1" "$clr"
		sn=$((sn + 1))
		sleep "$tick"
	done
	printf '\r%s%s' "$clr" "$curs_on"
	reap
}

task() { # message command args...: animated in a terminal, a plain note elsewhere
	tmsg=$1
	shift
	if [ "$tty" -eq 1 ]; then
		bg "$@"
		spin "$tmsg"
	else
		note "$tmsg"
		"$@"
	fi
}

# ---------------------------------------------------------------------------
# Screens: header, command block, PATH help
# ---------------------------------------------------------------------------
header() { # version platform: one quiet line, no logo
	if [ "$tty" -eq 0 ]; then
		printf 'RuneAgent %s for %s\n' "$1" "$2"
		return 0
	fi
	say ""
	printf '  %sRuneAgent %s%s  %s%s  %s  %s  Atoryn Studio%s\n\n' \
		"$bold" "$1" "$off" "$dim" "$g_dot" "$2" "$g_dot" "$off"
}

# cmdline text: the one thing to type. Bold in the normal text colour between two
# thin rules, with no background and no prompt character or bar, so it never looks
# selected and selecting the line and pasting it runs as it is. The rules are as
# wide as the command (never wider than the window); the line sits on the glyph
# column so it still fits in an 80-column window.
cmdline() {
	if [ "$tty" -eq 0 ]; then
		printf '\n%s%s\n' "$ind" "$1"
		return 0
	fi
	cl_cols=${COLUMNS:-}
	case "$cl_cols" in '' | *[!0-9]*) cl_cols=$(tput cols 2>/dev/null || echo 80) ;; esac
	case "$cl_cols" in '' | *[!0-9]*) cl_cols=80 ;; esac
	cl_w=${#1}
	[ "$cl_w" -le $((cl_cols - 4)) ] || cl_w=$((cl_cols - 4))
	printf '  %s' "$dim"
	rep "$g_rule" "$cl_w"
	printf '%s\n  %s%s%s\n  %s' "$off" "$bold" "$1" "$off" "$dim"
	rep "$g_rule" "$cl_w"
	printf '%s\n' "$off"
}

ready() { # version: ends with the command to type, as the last line
	if [ "$tty" -eq 0 ]; then
		printf '\nReady. Start it in a project folder with: rune\n'
		return 0
	fi
	say ""
	printf '  RuneAgent %s is ready. Start it in a project folder:\n' "$1"
	cmdline rune
	say ""
}

mentioned() { # dir file...: the first file that already names dir on an uncommented line
	case "$1" in
	"$HOME"/*) mkey=${1#"$HOME"/} ;;
	*) mkey=$1 ;;
	esac
	shift
	for mf in "$@"; do
		[ -f "$mf" ] || continue
		if grep -v '^[[:space:]]*#' "$mf" 2>/dev/null | grep -qF -- "$mkey"; then
			printf '%s' "$mf"
			return 0
		fi
	done
	return 1
}

# path_help dir mode: tell the person how to get the folder onto PATH, and never
# touch a profile. mode "none": this window cannot run rune yet. mode "window":
# this window already can, only terminals opened later may miss the folder.
#
# Every screen here follows one order: what is wrong, what to do, and then the
# single command, last. There is deliberately no second, shorter command (an
# "only for this window" export): the one command below already refreshes this
# terminal, and a lookalike line under it is the one people copy by mistake.
path_help() {
	ph_dir=$1
	ph_show=$(tilde "$ph_dir")
	ph_var=$(homevar "$ph_dir")
	ph_export="export PATH=\"$ph_var:\$PATH\""
	ph_rc='' ph_found='' ph_fish=0 ph_src='.'
	case "${SHELL:-}" in
	*/fish) ph_fish=1 ;;
	*/zsh)
		ph_zd=${ZDOTDIR:-$HOME}
		ph_rc=$ph_zd/.zshrc ph_src=source
		ph_found=$(mentioned "$ph_dir" "$ph_zd/.zshrc" "$ph_zd/.zprofile" "$ph_zd/.zshenv" "$HOME/.profile") || ph_found=''
		;;
	*/bash)
		if [ "$(uname -s)" = Darwin ]; then ph_rc=$HOME/.bash_profile; else ph_rc=$HOME/.bashrc; fi
		ph_src=source
		ph_found=$(mentioned "$ph_dir" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile") || ph_found=''
		;;
	*)
		ph_rc=$HOME/.profile
		ph_found=$(mentioned "$ph_dir" "$HOME/.profile") || ph_found=''
		;;
	esac
	if [ "$ph_fish" -eq 1 ]; then
		ph_add="fish_add_path $ph_var"
	else
		ph_where=$(tilde "$ph_rc")
		ph_add="echo '$ph_export' >> $ph_where"
	fi

	if [ "$2" = window ]; then
		[ -z "$ph_found" ] || return 0
		say ""
		note "New terminals will not find rune yet."
		printf '%sNo export for %s was found in %s.\n\n' "$ind" "$ph_show" "${ph_where:-your fish setup}"
		printf '%sRun the line below once to keep it:\n' "$ind"
		cmdline "$ph_add"
		return 0
	fi

	say ""
	warn "$ph_show is not on your PATH, so ${bold}rune${off} is not found yet."
	if [ -n "$ph_found" ]; then
		ph_file=$(tilde "$ph_found")
		printf '%s%s already adds it; this terminal only needs a refresh.\n\n' "$ind" "$ph_file"
		printf '%sRun the line below once, then start RuneAgent by\n' "$ind"
		printf '%styping %srune%s.\n' "$ind" "$bold" "$off"
		cmdline "$ph_src $ph_file"
	elif [ "$ph_fish" -eq 1 ]; then
		printf '%sNo export for it was found.\n\n' "$ind"
		printf '%sRun the line below once. It works at once and stays, so\n' "$ind"
		printf '%syou can then start RuneAgent by typing %srune%s.\n' "$ind" "$bold" "$off"
		cmdline "$ph_add"
	else
		printf '%sNo export for it was found in %s.\n\n' "$ind" "$ph_where"
		printf '%sRun the line below once. It adds the export and refreshes\n' "$ind"
		printf '%sthis terminal, then you can start RuneAgent by typing %srune%s.\n' "$ind" "$bold" "$off"
		cmdline "$ph_add && $ph_src $ph_where"
	fi
	say ""
}

# ---------------------------------------------------------------------------
# Linking into a folder that is already on PATH
# ---------------------------------------------------------------------------

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

# finish dir: 0 when this window can run rune now (the folder is on PATH, or a
# link in a folder that is), 3 when the folder still has to be put on PATH.
finish() {
	on_path "$1" && return 0
	linked=$(link_dir rune "$1")
	[ -z "$linked" ] && return 3
	step "Linked $(tilde "$linked")/rune  ${dim}(already on your PATH)${off}"
	return 0
}

# ---------------------------------------------------------------------------
# The install itself
# ---------------------------------------------------------------------------
# Never exit from here. zsh runs an EXIT trap after an explicit exit with $? set
# to 0, so "the folder is not on PATH yet" (3) became a reported success, and the
# window was told it was ready while rune could not be found at all. Letting the
# shell keep its own status is what dash, bash, ash and zsh all honour.
cleanup() {
	[ "$tty" -eq 0 ] || printf '%s' "$curs_on"
	[ -z "$bgpid" ] || kill "$bgpid" 2>/dev/null || :
	[ -z "$newbin" ] || rm -f "$newbin"
	rm -rf "$tmp"
}

abort() {
	[ "$tty" -eq 0 ] || printf '\r%s%s' "$clr" "$curs_on"
	warn "installation interrupted"
	exit 130
}

# check_package package: every file the list names, with the hash it names.
check_package() {
	checked=0
	while IFS= read -r line; do
		line=$(printf '%s' "$line" | tr -d '\r')
		[ -n "$line" ] || continue
		# sha256sum writes two spaces between the hash and the name; awk splits on
		# a run of whitespace, cut would not.
		hash=$(printf '%s' "$line" | awk '{print $1}')
		name=$(printf '%s' "$line" | awk '{print $2}')
		case "$name" in
		rune | install.sh | install.ps1 | INSTALL.txt | LICENSE.txt | THIRD-PARTY-NOTICES.txt) ;;
		*) fail "unexpected file in the checksum list: $name" ;;
		esac
		if [ "$name" = rune ]; then
			checked=1
			[ ! -L "$1/rune" ] || fail "the rune binary in this package is a link, not a file"
		fi
		[ -f "$1/$name" ] || fail "missing from the package: $name"
		actual=$(sha256_of "$1/$name")
		[ "$actual" = "$hash" ] ||
			fail "CHECKSUM MISMATCH for $name - nothing was installed. Do not use this package."
	done <"$1/SHA256SUMS"
	[ "$checked" = 1 ] || fail "the checksum list does not cover the rune binary"
}

install_rune() {
	set -eu
	umask 022
	# No "set +m" here: posh refuses it, and under set -e that killed the whole
	# run silently. Job control is already off in a non-interactive shell, which
	# is every way this installer is started.

	package=$(package_dir) || fail "cannot locate the package folder"
	[ "$(id -u)" -ne 0 ] || fail "run this as your normal user, not root: RuneAgent installs into your home folder"
	case "$(uname -s)" in
	Linux)
		system=linux name_system=Linux
		[ -z "${RUNE_LIBC:-}" ] || fail "RUNE_LIBC applies to the online installer only"
		;;
	Darwin) system=macos name_system=macOS ;;
	*) fail "this package is for Linux and macOS; on Windows use the package's install.ps1" ;;
	esac
	[ -f "$package/rune" ] || fail "this folder holds no rune binary - unpack the whole package first, and run the installer from inside it"
	[ -f "$package/SHA256SUMS" ] || fail "this folder holds no SHA256SUMS - unpack the whole package first"

	dir=$(install_dir)
	case "$dir" in /*) ;; *) fail "RUNE_INSTALL_DIR must be an absolute path" ;; esac

	tmp=$(mktemp -d 2>/dev/null || mktemp -d -t rune-install)
	bgpid='' newbin=''
	trap cleanup EXIT
	trap abort INT TERM

	# The binary must run here and say what it is. Its executable bit may not have
	# survived the unpacking (a zip tool on Windows, a GUI unzipper), so it is set
	# before anything tries to run the file.
	chmod 0755 "$package/rune" 2>/dev/null || true
	version=$("$package/rune" --version 2>/dev/null) || fail "the rune binary in this package does not run on this system"
	case "$version" in
	rune\ *) ;;
	*) fail "the rune binary in this package does not say what it is" ;;
	esac

	header "${version#rune }" "$name_system"

	task "Checking every file against the package checksums" check_package "$package" ||
		fail "this package does not match its own checksums - nothing was installed"
	step "Package checksums verified  ${dim}(SHA-256)${off}"

	mkdir -p "$dir"
	newbin="$dir/rune.new.$$"
	cp "$package/rune" "$newbin"
	mv -f "$newbin" "$dir/rune"
	newbin=''
	docs="${XDG_DATA_HOME:-$HOME/.local/share}/runeagent"
	mkdir -p "$docs"
	for f in LICENSE.txt THIRD-PARTY-NOTICES.txt; do
		[ ! -f "$package/$f" ] || cp "$package/$f" "$docs/$f"
	done
	step "Installed $(tilde "$dir")/rune"

	finish "$dir" || exit $?
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
	# The work runs in a subshell, so no option, umask, trap or failed step can
	# reach the calling shell (or close it). It ends with 0 when this window can
	# run rune, 3 when the install folder still has to be put on PATH.
	dir=$(install_dir)
	( install_rune ) && rc=0 || rc=$?
	case "$rc" in
	0 | 3) ;;
	*) return "$rc" ;;
	esac
	# The version to name on the ready screen: whatever the installed binary says.
	ver=$("$dir/rune" --version 2>/dev/null | sed -n 's/^rune //p')
	[ -n "$ver" ] || ver='RuneAgent'

	if [ "$rc" -eq 0 ]; then
		ready "$ver"
		return 0
	fi

	if [ "$sourced" = 1 ]; then
		PATH="$dir:$PATH"
		export PATH
		case "$-" in
		*i*) # an interactive shell read this file: that window is ready
			step "Added $(tilde "$dir") to PATH in this window"
			path_help "$dir" window
			ready "$ver"
			;;
		*) # a script sourced it: the script has the folder, the person's own
			# shell does not, and saying otherwise would be a lie
			step "Added $(tilde "$dir") to PATH for the script that sourced this installer"
			path_help "$dir" none
			;;
		esac
		return 0
	fi

	path_help "$dir" none
	return 0
}
main "$@"
