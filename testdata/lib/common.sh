#!/usr/bin/env bash
# Shared helpers for RECOVERY-CORE fixture generation.
#
# Sourced by build_fixtures.sh. Everything privileged goes through `sudo -n`
# against the narrow command set allowed by /etc/sudoers.d/recovery-core-fixtures;
# if that drop-in is missing these calls fail loudly rather than prompting.

set -euo pipefail

# --- environment -----------------------------------------------------------

# Uniform 4 KiB clusters across every filesystem so cluster arithmetic in the
# scoring and carving engines has one constant to reason about.
RC_CLUSTER_BYTES="${RC_CLUSTER_BYTES:-4096}"
RC_SECTOR_BYTES="${RC_SECTOR_BYTES:-512}"
RC_FIXTURE_SIZE="${RC_FIXTURE_SIZE:-512M}"

RC_LOOPS=()
RC_MOUNTS=()

# --- logging ---------------------------------------------------------------

rc_log()  { printf '  %s\n' "$*" >&2; }
rc_step() { printf '\n== %s\n' "$*" >&2; }
rc_die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# --- cleanup ---------------------------------------------------------------
#
# Unmount in reverse order, then detach loops. Registered as an EXIT trap by
# build_fixtures.sh so a failed run never strands a loop device or leaves an
# image mounted (which would keep the OS writing to it).

rc_cleanup() {
    local rc=$?
    set +e
    local i
    for (( i=${#RC_MOUNTS[@]}-1; i>=0; i-- )); do
        local mp="${RC_MOUNTS[$i]}"
        if mountpoint -q "$mp" 2>/dev/null; then
            sudo -n umount "$mp" 2>/dev/null || sudo -n umount -l "$mp" 2>/dev/null
        fi
        rmdir "$mp" 2>/dev/null
    done
    for (( i=${#RC_LOOPS[@]}-1; i>=0; i-- )); do
        sudo -n losetup -d "${RC_LOOPS[$i]}" 2>/dev/null
    done
    RC_MOUNTS=()
    RC_LOOPS=()
    return $rc
}

# --- tool checks -----------------------------------------------------------

rc_require_tools() {
    local missing=()
    local t
    for t in "$@"; do
        command -v "$t" >/dev/null 2>&1 || missing+=("$t")
    done
    if (( ${#missing[@]} )); then
        rc_die "missing required tools: ${missing[*]}
Install with:
  sudo apt-get install ntfs-3g dosfstools exfatprogs exfat-fuse e2fsprogs util-linux"
    fi
}

rc_require_sudo() {
    # Probe with a command that is actually in the allowlist. `sudo -n true`
    # would fail here by design, because `true` is deliberately not granted.
    if ! sudo -n losetup --list >/dev/null 2>&1; then
        rc_die "passwordless sudo is not configured.
Expected /etc/sudoers.d/recovery-core-fixtures granting NOPASSWD for
losetup, mount, umount, mkfs.*, chown, blockdev and sync.
Create it with:  wsl -d Ubuntu -u root -- bash testdata/lib/install_sudoers.sh"
    fi
}

# --- loop devices ----------------------------------------------------------

# rc_image_create PATH [SIZE]
rc_image_create() {
    local path="$1" size="${2:-$RC_FIXTURE_SIZE}"
    rm -f "$path"
    mkdir -p "$(dirname "$path")"
    truncate -s "$size" "$path"
    printf '%s' "$path"
}

# rc_loop_attach IMAGE [--partscan] -> echoes /dev/loopN
rc_loop_attach() {
    local img="$1"; shift
    local extra=()
    [[ "${1:-}" == "--partscan" ]] && extra+=(--partscan)
    local dev
    dev="$(sudo -n losetup --find --show "${extra[@]}" "$img")" \
        || rc_die "losetup failed for $img"
    RC_LOOPS+=("$dev")
    printf '%s' "$dev"
}

rc_loop_detach() {
    local dev="$1"
    sudo -n losetup -d "$dev" 2>/dev/null || true
    local i out=()
    for i in "${RC_LOOPS[@]}"; do [[ "$i" == "$dev" ]] || out+=("$i"); done
    RC_LOOPS=("${out[@]:-}")
}

# --- filesystems -----------------------------------------------------------

# rc_mkfs FSTYPE DEVICE LABEL
#
# Cluster/block size is pinned to RC_CLUSTER_BYTES for every filesystem.
rc_mkfs() {
    local fs="$1" dev="$2" label="$3"
    case "$fs" in
        ntfs)
            # -F force, -Q quick (skips the full zero pass; we want the
            # unwritten regions left alone anyway so carving sees real slack)
            sudo -n mkfs.ntfs -F -Q -c "$RC_CLUSTER_BYTES" -L "$label" "$dev" >/dev/null
            ;;
        vfat)
            local spc=$(( RC_CLUSTER_BYTES / RC_SECTOR_BYTES ))
            sudo -n mkfs.vfat -F 32 -s "$spc" -S "$RC_SECTOR_BYTES" -n "$label" "$dev" >/dev/null
            ;;
        exfat)
            sudo -n mkfs.exfat -c "$RC_CLUSTER_BYTES" -L "$label" "$dev" >/dev/null
            ;;
        ext4)
            # ^has_journal is NOT used: the JBD2 journal is the whole point of
            # the Milestone 6 ext4 recovery path.
            #
            # -m 0 removes the default 5% root-reserved blocks. With them, a
            # non-root fill hits ENOSPC while ~25 MiB remains free to root, so
            # the "volume is full except for a field of holes" setup the
            # fragmentation fixture depends on is never actually reached and
            # the file lands contiguously.
            sudo -n mkfs.ext4 -q -F -m 0 -b "$RC_CLUSTER_BYTES" -L "$label" "$dev"
            ;;
        *) rc_die "unknown filesystem: $fs" ;;
    esac
}

# rc_mount FSTYPE DEVICE -> echoes mountpoint
#
# FAT and exFAT have no on-disk ownership, so they take uid/gid mount options;
# ext4 gets a one-time chown instead. exFAT has no kernel driver in the WSL2
# kernel, so it is mounted through the FUSE implementation.
rc_mount() {
    local fs="$1" dev="$2"
    local mp
    mp="$(mktemp -d)"
    local u g
    u="$(id -u)"; g="$(id -g)"
    case "$fs" in
        ntfs)  sudo -n mount -t ntfs3       -o "uid=$u,gid=$g" "$dev" "$mp" 2>/dev/null \
            || sudo -n mount -t ntfs-3g     -o "uid=$u,gid=$g" "$dev" "$mp" ;;
        # utf8=1 is required or the kernel vfat driver mangles non-ASCII long
        # filenames on the way in, which would make the fixture's ground truth
        # a lie rather than a test.
        vfat)  sudo -n mount -t vfat        -o "uid=$u,gid=$g,utf8=1" "$dev" "$mp" ;;
        exfat) sudo -n mount -t exfat       -o "uid=$u,gid=$g" "$dev" "$mp" 2>/dev/null \
            || sudo -n mount -t exfat-fuse  -o "uid=$u,gid=$g" "$dev" "$mp" ;;
        # nodelalloc: ext4's delayed allocation lets the small filler files be
        # freed before writeback ever places them, which leaves a large
        # contiguous free region and defeats the hole field.
        ext4)  sudo -n mount -t ext4 -o nodelalloc "$dev" "$mp"; sudo -n chown "$u:$g" "$mp" ;;
        *) rc_die "unknown filesystem: $fs" ;;
    esac || rc_die "mount failed: $fs $dev"
    RC_MOUNTS+=("$mp")
    printf '%s' "$mp"
}

rc_umount() {
    local mp="$1"
    sync
    sudo -n umount "$mp" || rc_die "umount failed: $mp"
    rmdir "$mp" 2>/dev/null || true
    local i out=()
    for i in "${RC_MOUNTS[@]}"; do [[ "$i" == "$mp" ]] || out+=("$i"); done
    RC_MOUNTS=("${out[@]:-}")
}

# --- hashing ---------------------------------------------------------------

rc_sha256() { sha256sum "$1" | cut -d' ' -f1; }
