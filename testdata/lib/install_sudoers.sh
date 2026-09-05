#!/usr/bin/env bash
# Install the narrow sudoers drop-in that fixture generation needs.
# Run as root inside the WSL distro:
#     wsl -d Ubuntu -u root -- bash testdata/lib/install_sudoers.sh [USER]
#
# This deliberately grants NOPASSWD for a fixed command list rather than ALL.
# Files inside a mounted image are written as the invoking user (FAT/exFAT via
# uid= mount options, ext4 via a one-time chown of the mountpoint), so cp, rm
# and dd are intentionally NOT granted here.

set -euo pipefail

TARGET_USER="${1:-${SUDO_USER:-}}"
if [[ -z "$TARGET_USER" ]]; then
    TARGET_USER="$(getent passwd 1000 | cut -d: -f1)"
fi
[[ -n "$TARGET_USER" ]] || { echo "cannot determine target user" >&2; exit 1; }

DEST=/etc/sudoers.d/recovery-core-fixtures

cat > "$DEST" <<EOF
# RECOVERY-CORE: fixture generation (SPEC.md section 7).
# Scoped to exactly the privileged commands testdata/build_fixtures.sh needs.
Cmnd_Alias RC_FIXTURES = \\
    /usr/sbin/losetup, \\
    /usr/bin/mount, /usr/bin/umount, \\
    /usr/sbin/mkfs.ntfs, /usr/sbin/mkfs.vfat, /usr/sbin/mkfs.exfat, /usr/sbin/mkfs.ext4, \\
    /usr/bin/chown, \\
    /usr/sbin/blockdev, /usr/sbin/sfdisk, \\
    /usr/bin/sync
$TARGET_USER ALL=(root) NOPASSWD: RC_FIXTURES
EOF

chmod 0440 "$DEST"
visudo -c -f "$DEST"
echo "installed $DEST for user '$TARGET_USER'"
