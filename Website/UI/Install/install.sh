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
# already on PATH. When it cannot, it looks at your shell setup and prints
# the one command that finishes the job: add the export, then refresh the
# terminal. Run it as a source (the second form above) and it adds the
# folder to that window itself.
#
# In a terminal you get colour, a spinner and a live download bar. In a pipe,
# a log or a test you get plain lines, no colour and no redraws. NO_COLOR
# drops the colour and keeps the rest.
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

# ---------------------------------------------------------------------------
# Terminal: colour, glyphs, animation
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
bold='' dim='' red='' green='' yellow='' cyan='' off=''
clr='' curs_off='' curs_on=''
r0='' r1='' r2='' r3='' r4='' l1='' l2='' l3='' l4='' l5=''
if [ "$tty" -eq 1 ]; then
	clr="${esc}[K" curs_off="${esc}[?25l" curs_on="${esc}[?25h"
	if [ -z "${NO_COLOR:-}" ]; then
		bold="${esc}[1m" dim="${esc}[2m" red="${esc}[31m" green="${esc}[32m" yellow="${esc}[33m" cyan="${esc}[36m" off="${esc}[0m"
		case "${COLORTERM:-}${TERM:-}" in
		*truecolor* | *24bit* | *256color*)
			# blue -> cyan ramp for the download bar, cyan -> violet for the logo
			r0="${esc}[38;5;27m" r1="${esc}[38;5;33m" r2="${esc}[38;5;39m" r3="${esc}[38;5;45m" r4="${esc}[38;5;51m"
			l1="${esc}[38;5;51m" l2="${esc}[38;5;45m" l3="${esc}[38;5;39m" l4="${esc}[38;5;63m" l5="${esc}[38;5;99m"
			;;
		*)
			r0=$cyan r1=$cyan r2=$cyan r3=$cyan r4=$cyan
			l1=$cyan l2=$cyan l3=$cyan l4=$cyan l5=$cyan
			;;
		esac
	fi
fi

# Fractional sleeps drive the animation; without them it runs at one frame a second.
tick=1 fine=0
if [ "$tty" -eq 1 ] && sleep 0.01 2>/dev/null; then tick=0.1 fine=1; fi

f0='' f1='' f2='' f3='' f4='' f5='' f6='' f7='' f8='' f9='' nframes=4
if [ "$utf" -eq 1 ]; then
	g_ok=$(printf '\342\234\223  ')
	g_note=$(printf '\342\200\242  ')
	g_warn='!  '
	g_bad=$(printf '\342\234\227  ')
	g_sp='  ' ind='     '
	g_fill=$(printf '\342\224\201')
	g_empty=$(printf '\342\224\200')
	g_dot=$(printf '\302\267')
	bx_tl=$(printf '\342\225\255') bx_tr=$(printf '\342\225\256')
	bx_bl=$(printf '\342\225\260') bx_br=$(printf '\342\225\257')
	bx_h=$(printf '\342\224\200') bx_v=$(printf '\342\224\202')
	f0=$(printf '\342\240\213') f1=$(printf '\342\240\231') f2=$(printf '\342\240\271')
	f3=$(printf '\342\240\270') f4=$(printf '\342\240\274') f5=$(printf '\342\240\264')
	f6=$(printf '\342\240\246') f7=$(printf '\342\240\247') f8=$(printf '\342\240\207')
	f9=$(printf '\342\240\217')
	nframes=10
else
	g_ok='ok  ' g_note='--  ' g_warn='!   ' g_bad='x   '
	g_sp='   ' ind='      '
	g_fill='#' g_empty='.' g_dot='-'
	bx_tl='+' bx_tr='+' bx_bl='+' bx_br='+' bx_h='-' bx_v='|'
	f0='|' f1='/' f2='-' f3='\'
fi

say() { printf '%s\n' "$*"; }
step() { printf '  %s%s%s%s\n' "$green" "$g_ok" "$off" "$*"; }
note() { printf '  %s%s%s%s\n' "$dim" "$g_note" "$off" "$*"; }
warn() { printf '  %s%s%s%s\n' "$yellow" "$g_warn" "$off" "$*"; }
fail() {
	if [ "$tty" -eq 1 ]; then
		printf '\r%s%s' "$clr" "$curs_on"
		printf '  %s%s%srune installer:%s %s\n' "$red" "$g_bad" "$off$bold$red" "$off" "$*" >&2
	else
		printf 'rune installer: %s\n' "$*" >&2
	fi
	exit 1
}

# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------
curl_do() { # curl arguments...
	case "$ORIGIN" in
	https://*) curl --proto '=https' --tlsv1.2 "$@" ;;
	*) curl "$@" ;; # development server only
	esac
}

fetch_q() { curl_do -fsSL --retry 3 --retry-delay 1 -o "$2" "$1"; } # url dest; returns curl's status

fetch_list() { # release url: the checksum list and its signature
	fetch_q "$1/SHA256SUMS" "$tmp/SHA256SUMS" && fetch_q "$1/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig"
}

remote_size() { # url: size in bytes as the server states it, or 0
	curl_do -fsIL --max-time 10 "$1" 2>/dev/null | tr -d '\r' |
		awk 'tolower($1) == "content-length:" { n = $2 } END { print n + 0 }'
}

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

short_fp() { # fingerprint: first and last eight characters on a terminal, all of it in a log
	fp=$1
	if [ "$tty" -eq 1 ] && [ "${#fp}" -gt 20 ]; then
		fp_tail=${fp#????????}
		fp_head=${fp%"$fp_tail"}
		fp_tail=${fp%????????}
		fp_tail=${fp#"$fp_tail"}
		fp="$fp_head...$fp_tail"
	fi
	printf '%s' "$fp"
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

# ---------------------------------------------------------------------------
# Animation: spinner, background jobs, download bar
# ---------------------------------------------------------------------------
pick_frame() { # index: sets $fr
	case $(($1 % nframes)) in
	0) fr=$f0 ;; 1) fr=$f1 ;; 2) fr=$f2 ;; 3) fr=$f3 ;; 4) fr=$f4 ;;
	5) fr=$f5 ;; 6) fr=$f6 ;; 7) fr=$f7 ;; 8) fr=$f8 ;; *) fr=$f9 ;;
	esac
}

make_bar() { # filled width: sets $bar, a gradient fill followed by a dim remainder
	bar='' bi=0
	while [ "$bi" -lt "$1" ]; do
		case $((bi * 5 / $2)) in 0) bc=$r0 ;; 1) bc=$r1 ;; 2) bc=$r2 ;; 3) bc=$r3 ;; *) bc=$r4 ;; esac
		bar="$bar$bc$g_fill"
		bi=$((bi + 1))
	done
	bar="$bar$off$dim"
	while [ "$bi" -lt "$2" ]; do
		bar="$bar$g_empty"
		bi=$((bi + 1))
	done
	bar="$bar$off"
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
		printf '\r  %s%s%s%s%s%s' "$cyan" "$fr" "$g_sp" "$off" "$1" "$clr"
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

download() { # url dest label: a live bar in a terminal; returns the transfer's status
	if [ "$tty" -eq 0 ]; then
		note "Downloading $3"
		fetch_q "$1" "$2" || return 1
		return 0
	fi
	cols=${COLUMNS:-}
	case "$cols" in '' | *[!0-9]*) cols=$(tput cols 2>/dev/null || echo 80) ;; esac
	case "$cols" in '' | *[!0-9]*) cols=80 ;; esac
	if [ "$cols" -ge 76 ]; then barw=24 compact=0; else barw=12 compact=1; fi
	total=$(remote_size "$1")
	bg fetch_q "$1" "$2"
	began=$(date +%s)
	n=0
	printf '%s' "$curs_off"
	while [ ! -f "$tmp/rc" ]; do
		got=$(file_size "$2")
		elapsed=$(($(date +%s) - began))
		pick_frame "$n"
		printf '\r  %s%s%s%sDownloading  ' "$cyan" "$fr" "$g_sp" "$off"
		if [ "$total" -gt 0 ]; then
			pct=$((got * 100 / total))
			[ "$pct" -le 100 ] || pct=100
			make_bar $((pct * barw / 100)) "$barw"
			printf '%s %3d%%  ' "$bar" "$pct"
			human "$total"
		else
			human "$got"
		fi
		if [ "$compact" -eq 0 ] && [ "$elapsed" -ge 1 ] && [ "$got" -gt 0 ]; then
			speed=$((got / elapsed))
			printf '  %s' "$dim"
			human "$speed"
			printf '/s%s' "$off"
			if [ "$total" -gt "$got" ] && [ "$speed" -gt 0 ]; then
				printf '  %seta %ds%s' "$dim" $(((total - got) / speed)) "$off"
			fi
		fi
		printf '%s' "$clr"
		n=$((n + 1))
		sleep "$tick"
	done
	printf '\r%s%s' "$clr" "$curs_on"
	reap
}

# ---------------------------------------------------------------------------
# Screens: banner, boxes, PATH help
# ---------------------------------------------------------------------------
banner() { # version platform
	if [ "$tty" -eq 0 ]; then
		printf 'RuneAgent %s for %s\n' "$1" "$2"
		return 0
	fi
	say ""
	bn=0
	while IFS= read -r brow; do
		bn=$((bn + 1))
		case $bn in 1) bc=$l1 ;; 2) bc=$l2 ;; 3) bc=$l3 ;; 4) bc=$l4 ;; *) bc=$l5 ;; esac
		printf '  %s%s%s%s\n' "$bold" "$bc" "$brow" "$off"
		[ "$fine" -eq 0 ] || sleep 0.05
	done <<'LOGO'
 ____
|  _ \ _   _ _ __   ___
| |_) | | | | '_ \ / _ \
|  _ <| |_| | | | |  __/
|_| \_\\__,_|_| |_|\___|
LOGO
	printf '  %sAtoryn Studio  %s  %s%sRuneAgent %s%s  %s%s%s  %s\n\n' \
		"$dim" "$g_dot" "$off" "$bold" "$1" "$off" "$dim" "$g_dot" "$off" "$2"
}

box() { # colour row...: a framed block; a row is "style:text" (t title, c command, d dim, n plain)
	bcol=$1
	shift
	bw=0
	for brow in "$@"; do
		btext=${brow#*:}
		[ "${#btext}" -le "$bw" ] || bw=${#btext}
	done
	printf '  %s%s' "$bcol" "$bx_tl"
	rep "$bx_h" $((bw + 4))
	printf '%s%s\n' "$bx_tr" "$off"
	for brow in "$@"; do
		bkind=${brow%%:*}
		btext=${brow#*:}
		case "$bkind" in
		t) bst="$bold$bcol" ;;
		c) bst="$bold$cyan" ;;
		d) bst=$dim ;;
		*) bst='' ;;
		esac
		printf '  %s%s%s  %s%s%s' "$bcol" "$bx_v" "$off" "$bst" "$btext" "$off"
		rep ' ' $((bw - ${#btext}))
		printf '  %s%s%s\n' "$bcol" "$bx_v" "$off"
	done
	printf '  %s%s' "$bcol" "$bx_bl"
	rep "$bx_h" $((bw + 4))
	printf '%s%s\n' "$bx_br" "$off"
}

ready_box() { # version
	if [ "$tty" -eq 0 ]; then
		printf '\nReady. Start it in a project folder with: rune\n'
		return 0
	fi
	say ""
	box "$green" "t:RuneAgent $1 is ready" "n:" "n:Start it in a project folder:" "c:    rune"
	say ""
}

cmdline() { printf '%s  %s%s%s%s\n' "$ind" "$bold" "$cyan" "$1" "$off"; }

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
		note "New terminals will not find rune yet: no export for $ph_show in ${ph_where:-your fish setup}."
		printf '%sRun this once to add it:\n' "$ind"
		cmdline "$ph_add"
		say ""
		return 0
	fi

	say ""
	warn "$ph_show is not on your PATH, so ${bold}rune${off} is not found yet."
	if [ -n "$ph_found" ]; then
		ph_file=$(tilde "$ph_found")
		printf '%s%s already adds it; this terminal only needs a refresh:\n' "$ind" "$ph_file"
		cmdline "$ph_src $ph_file"
	elif [ "$ph_fish" -eq 1 ]; then
		printf '%sNo export for it was found. Run this once (works at once, stays):\n' "$ind"
		cmdline "$ph_add"
	else
		printf '%sNo export for it was found in %s.\n' "$ind" "$ph_where"
		printf '%sRun this once: it adds the export and refreshes this terminal:\n' "$ind"
		cmdline "$ph_add && $ph_src $ph_where"
	fi
	say ""
	printf '%sThen start it with:  %s%srune%s\n' "$ind" "$bold" "$cyan" "$off"
	if [ -z "$ph_found" ] && [ "$ph_fish" -eq 0 ]; then
		printf '%s%sOnly for this window:  %s%s\n' "$ind" "$dim" "$ph_export" "$off"
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

install_rune() {
	set -eu
	umask 022
	# No "set +m" here: posh refuses it, and under set -e that killed the whole
	# run silently. Job control is already off in a non-interactive shell, which
	# is every way this installer is started.

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
	bgpid='' newbin=''
	trap cleanup EXIT
	trap abort INT TERM

	banner "$version" "$name_system $name_arch"

	task "Fetching signed release list" fetch_list "$url" ||
		fail "download failed: $url/SHA256SUMS"
	printf '%s\n' "$SIGNING_KEY" >"$tmp/signing-key.pem"

	# 1. The checksum list must carry the release key's signature (ECDSA P-256
	#    over SHA-256, which OpenSSL and the LibreSSL of macOS both check).
	openssl dgst -sha256 -verify "$tmp/signing-key.pem" -signature "$tmp/SHA256SUMS.sig" \
		"$tmp/SHA256SUMS" >/dev/null 2>&1 ||
		fail "SIGNATURE CHECK FAILED for $version - nothing was installed. Do not use these files."
	step "Signature verified  ${dim}(key $(short_fp "$KEY_FINGERPRINT"))${off}"

	# 2. The archive must be in that signed list with exactly that hash.
	expected=$(tr -d '\r' <"$tmp/SHA256SUMS" | awk -v n="$name" '$2 == n { print $1 }')
	[ -n "$expected" ] || fail "no build for $name_system $name_arch in release $version"
	download "$url/$name" "$tmp/$name" "$name" || fail "download failed: $url/$name"
	step "Downloaded $name  ${dim}($(human "$(file_size "$tmp/$name")"))${off}"
	actual=$(sha256_of "$tmp/$name")
	[ "$actual" = "$expected" ] || fail "CHECKSUM MISMATCH for $name - nothing was installed. Do not use this file."
	step "Checksum verified  ${dim}(SHA-256)${off}"

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
	newbin="$dir/rune.new.$$"
	cp "$tmp/x/rune" "$newbin"
	mv -f "$newbin" "$dir/rune"
	newbin=''
	docs="${XDG_DATA_HOME:-$HOME/.local/share}/runeagent"
	mkdir -p "$docs"
	for f in LICENSE.txt THIRD-PARTY-NOTICES.txt; do
		[ ! -f "$tmp/x/$f" ] || cp "$tmp/x/$f" "$docs/$f"
	done
	step "Installed $(tilde "$dir")/rune"

	finish "$dir" || exit $?
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
	# The work runs in a subshell, so no option, umask, trap or failed step can
	# reach the calling shell (or close it). It ends with 0 when this window can
	# run rune, 3 when the install folder still has to be put on PATH.
	dir=$(install_dir)
	( install_rune ) && rc=0 || rc=$?
	case "$rc" in
	0 | 3) ;;
	*) return "$rc" ;;
	esac
	ver=${RUNE_VERSION:-$LATEST}

	if [ "$rc" -eq 0 ]; then
		ready_box "$ver"
		return 0
	fi

	if [ "$sourced" = 1 ]; then
		PATH="$dir:$PATH"
		export PATH
		case "$-" in
		*i*) # an interactive shell read this file: that window is ready
			step "Added $(tilde "$dir") to PATH in this window"
			ready_box "$ver"
			path_help "$dir" window
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
