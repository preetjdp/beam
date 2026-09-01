#!/usr/bin/env bash
# Reproducible Beam process/network sample collector.
# Usage: sudo ./scripts/stream-benchmark.sh OUTPUT_DIR DURATION_SECS [NETEM_ARGS]
# Example: sudo ... ../tmp/bench/baseline-scroll 60 'delay 50ms loss 1% rate 20mbit'
set -Eeuo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 OUTPUT_DIR DURATION_SECS [NETEM_ARGS]" >&2
  exit 2
fi

output=$1
duration=$2
netem=${3:-}
[[ $output != /tmp/* ]] || { echo "refusing OS /tmp; use the workspace tmp directory" >&2; exit 2; }
[[ $duration =~ ^[0-9]+$ ]] && (( duration > 0 && duration <= 3600 )) || { echo "duration must be 1..3600" >&2; exit 2; }
mkdir -p "$output"
output=$(cd "$output" && pwd)
iface=${BEAM_BENCH_INTERFACE:-$(ip route show default | awk '{print $5; exit}')}
original_qdisc="$output/qdisc-before.txt"
tc qdisc show dev "$iface" > "$original_qdisc"
original_kind=$(awk 'NR==1 {print $2}' "$original_qdisc")
shaping_applied=0

cleanup() {
  if (( shaping_applied )); then
    tc qdisc del dev "$iface" root 2>/dev/null || true
    case "$original_kind" in
      fq|fq_codel|pfifo_fast) tc qdisc replace dev "$iface" root "$original_kind" 2>/dev/null || true ;;
      noqueue) ;;
    esac
  fi
}
trap cleanup EXIT INT TERM HUP

if [[ -n $netem ]]; then
  [[ ${BEAM_BENCH_ALLOW_NETEM:-0} == 1 ]] || {
    echo "set BEAM_BENCH_ALLOW_NETEM=1 to opt in to host network shaping" >&2
    exit 2
  }
  if ! grep -qE ' (noqueue|pfifo_fast|fq_codel|fq) ' "$original_qdisc"; then
    echo "refusing to replace a non-standard qdisc; shape externally" >&2
    exit 2
  fi
  # shellcheck disable=SC2086 # operator-provided tc argument vector
  tc qdisc replace dev "$iface" root netem $netem
  shaping_applied=1
fi

commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)
version=$(beam-server --version 2>/dev/null || true)
cat > "$output/manifest.json" <<JSON
{
  "schema_version": 1,
  "captured_at_utc": "$(date -u +%FT%TZ)",
  "beam_commit": "$commit",
  "beam_version": "$version",
  "host": "$(hostname)",
  "kernel": "$(uname -sr)",
  "network_interface": "$iface",
  "network_profile": "${netem//\"/\\\"}",
  "duration_seconds": $duration
}
JSON

printf 'timestamp,pid,comm,cpu_percent,rss_kib\n' > "$output/process.csv"
end=$((SECONDS + duration))
while (( SECONDS < end )); do
  now=$(date -u +%FT%T.%3NZ)
  ps -C beam-server -C beam-agent -o pid=,comm=,%cpu=,rss= | awk -v now="$now" '{print now "," $1 "," $2 "," $3 "," $4}' >> "$output/process.csv"
  sleep 1
done

curl_args=(--fail --silent --show-error --max-time 5)
if [[ -n ${BEAM_METRICS_TOKEN:-} ]]; then
  curl_args+=(-H "Authorization: Bearer $BEAM_METRICS_TOKEN")
fi
curl "${curl_args[@]}" "${BEAM_METRICS_URL:-https://127.0.0.1:8444/metrics}" \
  -o "$output/prometheus.txt" || true
ss -s > "$output/socket-summary.txt"
tc -s qdisc show dev "$iface" > "$output/qdisc-after.txt"
echo "artifacts: $output"
