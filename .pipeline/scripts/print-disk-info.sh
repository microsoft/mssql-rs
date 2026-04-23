#!/bin/bash
# print-disk-info.sh - Print disk space and top consumers for Linux CI agents

echo "=== Disk Space - All Mount Points ==="
df -h

echo ""
echo "=== Agent Work Folder Disk Space ==="
WORK_DIR="${AGENT_WORKFOLDER:-/mnt/vss/_work}"
df -h "$WORK_DIR" 2>/dev/null || echo "Work folder $WORK_DIR not found"

echo ""
echo "=== Top Directory Space Consumers ==="
du -sh /home /tmp /var /opt /usr /root /var/lib/docker /mnt 2>/dev/null | sort -rh

echo ""
echo "=== Docker Disk Usage ==="
docker system df 2>/dev/null || echo "Docker not available or not running"
