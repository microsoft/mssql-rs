#!/bin/bash
# print-disk-info.sh - Print disk space and top consumers for Linux CI agents

echo "=== Disk Space - All Mount Points ==="
df -h

echo ""
echo "=== Block Devices (TYPE / TRAN=transport / ROTA=1 spinning,0 SSD) ==="
# TRAN column shows 'nvme' for NVMe-attached devices; ROTA=0 confirms SSD/NVMe.
lsblk -o NAME,SIZE,TYPE,TRAN,ROTA,FSTYPE,MOUNTPOINT,MODEL 2>/dev/null || lsblk

echo ""
echo "=== NVMe devices present ==="
if ls /dev/nvme* >/dev/null 2>&1; then
  ls -l /dev/nvme*
  command -v nvme >/dev/null 2>&1 && nvme list 2>/dev/null || true
else
  echo "No /dev/nvme* devices found (agent is NOT on NVMe)."
fi

echo ""
echo "=== Backing device + transport for key mount points ==="
for m in / /mnt /mnt/vss /mnt/azure_nvme_temp "${AGENT_WORKFOLDER:-/mnt/vss/_work}"; do
  [ -e "$m" ] || continue
  src=$(findmnt -no SOURCE --target "$m" 2>/dev/null)
  fstype=$(findmnt -no FSTYPE --target "$m" 2>/dev/null)
  # Resolve the parent block device and read its transport from sysfs.
  dev=$(lsblk -no PKNAME "$src" 2>/dev/null | head -1)
  [ -z "$dev" ] && dev=$(basename "$src" 2>/dev/null)
  tran=$(cat "/sys/class/block/$dev/device/../transport" 2>/dev/null)
  case "$src" in *nvme*) tran="${tran:-nvme}";; esac
  echo "$m -> ${src:-?} (${fstype:-?}) transport=${tran:-unknown}"
done

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
