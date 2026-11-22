#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/macos/NightfirePort.app"

LAUNCH=0
EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --launch)
      LAUNCH=1
      shift
      ;;
    -h|--help)
      cat << 'EOF'
Usage: preflight.sh [--launch] [-- <app args...>]

Inspects macOS memory headroom, CPU load average, and thermal pressure,
selects an appropriate NF_TIER (low or balanced), and prints a headroom report.

Options:
  --launch    Launch NightfirePort.app with the selected NF_TIER exported.
              Without --launch, runs in check-only mode.
  -h, --help  Show this help message.
EOF
      exit 0
      ;;
    --)
      shift
      EXTRA_ARGS=("$@")
      break
      ;;
    *)
      EXTRA_ARGS+=("$1")
      shift
      ;;
  esac
done

# 1. Total RAM (MiB)
TOTAL_BYTES="$(sysctl -n hw.memsize 2>/dev/null || echo 0)"
TOTAL_RAM_MB=$(( TOTAL_BYTES / 1048576 ))

# 2. Available / free RAM (MiB) from vm_stat (free + inactive + speculative)
VM_OUT="$(vm_stat 2>/dev/null || true)"
PAGE_SIZE="$(printf '%s\n' "$VM_OUT" | awk '/page size of/ { for (i=1; i<=NF; i++) if ($i=="of") { print $(i+1); exit } }')"
PAGE_SIZE="${PAGE_SIZE:-4096}"

PAGES_FREE="$(printf '%s\n' "$VM_OUT" | awk -F: '/^Pages free:/ { gsub(/[^0-9]/, "", $2); print $2 }')"
PAGES_INACTIVE="$(printf '%s\n' "$VM_OUT" | awk -F: '/^Pages inactive:/ { gsub(/[^0-9]/, "", $2); print $2 }')"
PAGES_SPECULATIVE="$(printf '%s\n' "$VM_OUT" | awk -F: '/^Pages speculative:/ { gsub(/[^0-9]/, "", $2); print $2 }')"
PAGES_FREE="${PAGES_FREE:-0}"
PAGES_INACTIVE="${PAGES_INACTIVE:-0}"
PAGES_SPECULATIVE="${PAGES_SPECULATIVE:-0}"

FREE_RAM_MB=$(( (PAGES_FREE + PAGES_INACTIVE + PAGES_SPECULATIVE) * PAGE_SIZE / 1048576 ))
if (( TOTAL_RAM_MB > 0 && FREE_RAM_MB > TOTAL_RAM_MB )); then
  FREE_RAM_MB="$TOTAL_RAM_MB"
fi

# 3. CPU cores and load average
CPU_CORES="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"
LOAD_AVG="$(sysctl -n vm.loadavg 2>/dev/null | sed 's/^[[:space:]]*{[[:space:]]*//; s/[[:space:]]*}[[:space:]]*$//' || echo "unknown")"

# 4. Thermal pressure from pmset -g therm
THERM_OUT="$(pmset -g therm 2>&1 || true)"
THERMAL_STATE="nominal"

SPEED_LIMIT="$(printf '%s\n' "$THERM_OUT" | awk -F= 'tolower($1) ~ /cpu_speed_limit/ { gsub(/[^0-9]/, "", $2); print $2; exit }')"
THERM_LEVEL="$(printf '%s\n' "$THERM_OUT" | awk -F= 'tolower($1) ~ /thermal_level/ { gsub(/[^0-9]/, "", $2); print $2; exit }')"

if [[ -n "$SPEED_LIMIT" ]]; then
  if (( SPEED_LIMIT <= 50 )); then
    THERMAL_STATE="critical"
  elif (( SPEED_LIMIT <= 75 )); then
    THERMAL_STATE="serious"
  elif (( SPEED_LIMIT < 100 )); then
    THERMAL_STATE="fair"
  fi
fi

if [[ -n "$THERM_LEVEL" && "$THERM_LEVEL" -gt 0 ]]; then
  if (( THERM_LEVEL >= 3 )); then
    THERMAL_STATE="critical"
  elif (( THERM_LEVEL == 2 )) && [[ "$THERMAL_STATE" != "critical" ]]; then
    THERMAL_STATE="serious"
  elif [[ "$THERMAL_STATE" == "nominal" ]]; then
    THERMAL_STATE="fair"
  fi
fi

if printf '%s\n' "$THERM_OUT" | grep -Eiv 'no (thermal|performance) warning|no cpu power status' | grep -Eiq 'critical|heavy'; then
  THERMAL_STATE="critical"
elif [[ "$THERMAL_STATE" == "nominal" ]] && printf '%s\n' "$THERM_OUT" | grep -Eiv 'no (thermal|performance) warning|no cpu power status' | grep -Eiq 'serious|moderate'; then
  THERMAL_STATE="serious"
elif [[ "$THERMAL_STATE" == "nominal" ]] && printf '%s\n' "$THERM_OUT" | grep -Eiv 'no (thermal|performance) warning|no cpu power status' | grep -Eiq 'fair|light'; then
  THERMAL_STATE="fair"
fi

# 5. Tier selection
TIER_REASON="headroom nominal"
if [[ -n "${NF_TIER:-}" ]]; then
  SELECTED_TIER="$NF_TIER"
  TIER_REASON="preserved from existing NF_TIER env"
elif (( FREE_RAM_MB < 4096 )) || [[ "$THERMAL_STATE" != "nominal" ]]; then
  SELECTED_TIER="low"
  if (( FREE_RAM_MB < 4096 )) && [[ "$THERMAL_STATE" != "nominal" ]]; then
    TIER_REASON="free RAM < 4096 MB and thermal pressure elevated ($THERMAL_STATE)"
  elif (( FREE_RAM_MB < 4096 )); then
    TIER_REASON="free RAM < 4096 MB"
  else
    TIER_REASON="thermal pressure elevated ($THERMAL_STATE)"
  fi
else
  SELECTED_TIER="balanced"
fi

echo "=== NightfirePort macOS Pre-Flight Headroom Report ==="
echo "  RAM:      ${FREE_RAM_MB} MB available / ${TOTAL_RAM_MB} MB total (page=${PAGE_SIZE}B)"
echo "  CPU:      ${CPU_CORES} cores, load avg: ${LOAD_AVG}"
echo "  Thermal:  ${THERMAL_STATE}"
echo "  NF_TIER:  ${SELECTED_TIER} (${TIER_REASON})"
echo "======================================================"

if (( LAUNCH == 0 )); then
  echo "[preflight] Check-only mode. Run with --launch to start NightfirePort.app."
  exit 0
fi

if [[ ! -d "$APP" ]]; then
  echo "[preflight] ERROR: $APP not found. Run macos/build_app.sh first." >&2
  exit 1
fi

export NF_TIER="$SELECTED_TIER"
echo "[preflight] Launching $APP with NF_TIER=$NF_TIER..."
if (( ${#EXTRA_ARGS[@]} > 0 )); then
  open --env "NF_TIER=$NF_TIER" "$APP" --args "${EXTRA_ARGS[@]}"
else
  open --env "NF_TIER=$NF_TIER" "$APP"
fi
