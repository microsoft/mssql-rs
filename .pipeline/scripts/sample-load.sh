#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Samples agent load until killed, so a slow or failed stress test can be lined
# up against what the machine was doing at that second.
#
# Usage: sample-load.sh <output-dir>
#   host.csv    once a second: CPU busy % since the last sample, runnable
#               threads, 1-minute load average, available memory (MiB)
#   docker.txt  per-container CPU and memory from `docker stats`, every ~2 s
# Reads /proc rather than vmstat so it does not depend on the agent image.

set -u

out="${1:?usage: sample-load.sh <output-dir>}"
mkdir -p "$out"

sample_host() {
    echo "# cpus=$(nproc)"
    echo "utc,cpu_busy_pct,procs_running,load1,mem_available_mib"
    local prev_total=0 prev_idle=0
    while true; do
        local cpu user nice system idle iowait irq softirq steal rest
        read -r cpu user nice system idle iowait irq softirq steal rest < /proc/stat
        local idle_all=$((idle + iowait))
        local total=$((user + nice + system + idle_all + irq + softirq + steal))
        local busy=""
        if [ "$prev_total" -gt 0 ] && [ "$total" -gt "$prev_total" ]; then
            busy=$(( 100 * ((total - prev_total) - (idle_all - prev_idle)) / (total - prev_total) ))
        fi
        prev_total=$total
        prev_idle=$idle_all
        local running load1 mem_kb
        running=$(awk '/^procs_running/ {print $2}' /proc/stat)
        read -r load1 _ < /proc/loadavg
        mem_kb=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
        echo "$(date -u +%FT%T),$busy,$running,$load1,$((mem_kb / 1024))"
        sleep 1
    done
}

sample_docker() {
    while true; do
        # --no-stream blocks for about a second to measure CPU.
        docker stats --no-stream --format '{{.ID}} {{.Name}} cpu={{.CPUPerc}} mem={{.MemUsage}}' \
            2>&1 | sed "s/^/$(date -u +%FT%T) /"
        sleep 1
    done
}

sample_host > "$out/host.csv" &
pids=$!
if command -v docker > /dev/null 2>&1; then
    sample_docker > "$out/docker.txt" &
    pids="$pids $!"
fi
trap 'kill $pids 2> /dev/null; exit 0' TERM INT
wait
