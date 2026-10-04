#!/bin/sh
# Explicit administrator operation. Dockstride itself never elevates or commandeers directories.
set -eu
fail() { printf 'secret storage: %s\n' "$*" >&2; exit 1; }
directory=/opt/secrets
user=${SUDO_USER:-$(id -un)}
plan=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --directory|--user)
            [ "$#" -ge 2 ] || fail "$1 requires a value"
            if [ "$1" = --directory ]; then directory=$2; else user=$2; fi
            shift 2 ;;
        --plan) plan=1; shift ;;
        --help|-h)
            printf '%s\n' 'Usage: sudo scripts/setup-secret-storage.sh --user USER [--directory /opt/secrets] [--plan]' 'Creates an explicitly managed root-owned parent and a private per-user directory. Never installs packages.'
            exit 0 ;;
        *) fail "unknown argument: $1" ;;
    esac
done
case "$directory" in /*) ;; *) fail 'directory must be absolute' ;; esac
case "$directory" in /|*'/../'*|*/..|*'/./'*|*/.|*'//'*) fail 'directory must be a normalized, non-root absolute path' ;; esac
uid=$(id -u "$user")
gid=$(id -g "$user")
[ "$uid" -ne 0 ] || fail 'select a non-root application user explicitly with --user'
parent=$(dirname -- "$directory")
[ -d "$parent" ] && [ ! -L "$parent" ] || fail 'parent must already be an ordinary directory'
# Every ancestor must be root-owned and non-writable by ordinary users.
ancestor=$parent
while :; do
    [ ! -L "$ancestor" ] || fail "symlink ancestor: $ancestor"
    [ "$(stat -c %u -- "$ancestor")" = 0 ] || fail "non-root-owned ancestor: $ancestor"
    mode=$(stat -c %a -- "$ancestor")
    [ $((0$mode & 0022)) -eq 0 ] || fail "group/world-writable ancestor: $ancestor"
    [ "$ancestor" != / ] || break
    ancestor=$(dirname -- "$ancestor")
done
marker=$directory/.dockstride-owner
if [ -e "$directory" ] || [ -L "$directory" ]; then
    [ -d "$directory" ] && [ ! -L "$directory" ] || fail 'existing storage parent must not be a symlink'
    [ "$(stat -c %u -- "$directory")" = 0 ] && [ "$(stat -c %a -- "$directory")" = 755 ] || fail 'existing storage parent has incompatible ownership or mode; it will not be changed'
    [ -f "$marker" ] && [ ! -L "$marker" ] || fail 'existing storage parent has no valid Dockstride marker; it will not be adopted'
    [ "$(stat -c %u -- "$marker")" = 0 ] && [ "$(stat -c %a -- "$marker")" = 644 ] || fail 'incompatible marker ownership or permissions'
    printf 'dockstride-private-secrets-v1\n' | cmp -s - "$marker" || fail 'incompatible storage marker'
fi
private=$directory/u$uid
if [ -e "$private" ] || [ -L "$private" ]; then
    [ -d "$private" ] && [ ! -L "$private" ] || fail 'existing per-user path is not an ordinary directory'
    [ "$(stat -c %u -- "$private")" = "$uid" ] && [ "$(stat -c %a -- "$private")" = 300 ] || fail 'existing per-user directory has incompatible ownership or mode; it will not be changed'
fi
if [ "$plan" -eq 1 ]; then
    printf 'Managed parent: %s (root:root 0755)\nMarker: %s (root:root 0644)\nPrivate directory: %s (%s:%s 0300)\nNo changes made.\n' "$directory" "$marker" "$private" "$uid" "$gid"
    exit 0
fi
[ "$(id -u)" -eq 0 ] || fail 'run this explicit administrative operation with sudo; the CLI does not elevate itself'
if [ ! -d "$directory" ]; then
    mkdir -m 0755 -- "$directory"
    chown 0:0 -- "$directory"
    (umask 022; set -C; printf 'dockstride-private-secrets-v1\n' > "$marker")
    chown 0:0 -- "$marker"
    chmod 0644 -- "$marker"
fi
if [ ! -d "$private" ]; then
    mkdir -m 0300 -- "$private"
    chown "$uid:$gid" -- "$private"
fi
printf 'Secret storage ready: %s\nSet dockstride.setup.secretDirectory = "%s" in compose.ncl.\n' "$private" "$directory"
