#!/bin/sh
# Polaris macOS 真机内存采样：只统计显式给定的 GUI 主进程 / 主窗口 WebContent PID，不混入 sing-box。

set -eu

usage() {
  echo "usage: $0 APP_PID [WEB_PID|-] [DURATION_SECS] [INTERVAL_SECS] [STAGE] [OUTPUT_CSV]" >&2
  exit 2
}

[ "$(uname -s)" = "Darwin" ] || {
  echo "macos-memory-sample.sh 只能在 macOS 上运行" >&2
  exit 1
}

app_pid=${1:-}
web_pid=${2:--}
duration_secs=${3:-1800}
interval_secs=${4:-60}
stage=${5:-soak}
output_csv=${6:-/tmp/polaris-memory-$(date -u +%Y%m%dT%H%M%SZ).csv}

for numeric_value in "$app_pid" "$duration_secs" "$interval_secs"; do
  case "$numeric_value" in
    ''|*[!0-9]*) usage ;;
  esac
done
case "$web_pid" in
  -) ;;
  ''|*[!0-9]*) usage ;;
esac
[ "$duration_secs" -ge 0 ] 2>/dev/null || usage
[ "$interval_secs" -gt 0 ] 2>/dev/null || usage
kill -0 "$app_pid" 2>/dev/null || {
  echo "Polaris APP_PID $app_pid 不存在" >&2
  exit 1
}
[ ! -e "$output_csv" ] || {
  echo "输出文件已存在，不覆盖：$output_csv" >&2
  exit 1
}

footprint_field() {
  sample_pid=$1
  sample_field=$2
  if [ "$sample_pid" = "-" ] || ! kill -0 "$sample_pid" 2>/dev/null; then
    echo 0
    return
  fi
  footprint -f bytes --noCategories -p "$sample_pid" 2>/dev/null |
    awk -v field="$sample_field" '$1 == field ":" { print $2; found=1; exit } END { if (!found) print 0 }'
}

echo "timestamp_utc,stage,app_pid,app_phys_bytes,app_peak_bytes,web_pid,web_phys_bytes,web_peak_bytes" > "$output_csv"
started_at=$(date +%s)
while :; do
  now=$(date +%s)
  elapsed=$((now - started_at))
  [ "$elapsed" -le "$duration_secs" ] || break

  app_phys=$(footprint_field "$app_pid" phys_footprint)
  app_peak=$(footprint_field "$app_pid" phys_footprint_peak)
  web_phys=$(footprint_field "$web_pid" phys_footprint)
  web_peak=$(footprint_field "$web_pid" phys_footprint_peak)
  printf '%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$stage" "$app_pid" "$app_phys" "$app_peak" \
    "$web_pid" "$web_phys" "$web_peak" >> "$output_csv"

  [ "$elapsed" -eq "$duration_secs" ] && break
  sleep "$interval_secs"
done

echo "$output_csv"
