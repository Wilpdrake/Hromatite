#!/usr/bin/env bash
# Один живой прогон VRChat с payload'ом Hromatite и сбором логов.
#
#   live_run.sh <dll-win-path> <out-dir> <seconds> [KEY=VALUE ...]
#
# Пример:
#   live_run.sh 'Z:\home\wilpdrake\...\hromatite.dll' ~/EAC-RE/evidence/replay_0400 360 \
#       EAC_REPLAY_REPORT='Z:\home\wilpdrake\EAC-RE\evidence\report_full\report_a_sid158b.bin'
#
# Правила: логи <=32 МБ, Steam не трогаем, ~/.hromatite-mode восстанавливаем
# байт-в-байт (сверка sha256), игру убиваем только `pkill -f 'VRChat[.]exe'`.
set -uo pipefail
export PATH=$HOME/.cargo/bin:/run/current-system/sw/bin:$PATH

DLL="$1"; OUT="$2"; SECS="${3:-300}"; shift 3 || true

MODE="$HOME/.hromatite-mode"
BAK="$HOME/.hromatite-mode.bak-live-$(date +%H%M%S)"
cp -a "$MODE" "$BAK"
sha_before=$(sha256sum "$MODE" | cut -d' ' -f1)
mkdir -p "$OUT"

{
  echo "HROMATITE_DLL=$DLL"
  echo "HROMATITE_LATE_UPDATE=auto"
  echo "HROMATITE_PLAYER_LOOP=1"
  echo "HROMATITE_PLAYER_LOOP_TARGET=Update"
  echo "HROMATITE_LOG_AV=1"
  echo "HROMATITE_GCT_FIX=1"
  for kv in "$@"; do echo "$kv"; done
} > "$MODE"
cp -a "$MODE" "$OUT/mode.used"
echo "[live] mode written ($(wc -l < "$MODE") lines), backup $BAK"

PREFIX="$HOME/.local/share/Steam/steamapps/compatdata/438100/pfx"
LOG_DIR="$PREFIX/drive_c/users/steamuser/AppData/LocalLow/VRChat/VRChat"

( steam -applaunch 438100 --no-vr >"$OUT/steam.log" 2>&1 ) &
echo "[live] launched; waiting for the world (max 240 s)"
for i in $(seq 1 240); do
  sleep 1
  cur=$(ls -1t "$LOG_DIR"/output_log_*.txt 2>/dev/null | head -1)
  if [ -n "$cur" ] && grep -q "Entering Room" "$cur" 2>/dev/null; then
    echo "[live] world entered after ${i}s: $(grep -m1 'Entering Room' "$cur")"
    break
  fi
done
echo "[live] running ${SECS}s"
sleep "$SECS"

LOG=$(ls -1t "$LOG_DIR"/output_log_*.txt 2>/dev/null | head -1)
cp -a "$LOG" "$OUT/game_output_log.txt" 2>/dev/null
cp -a "$PREFIX/drive_c/users/steamuser/hromatite.log" "$OUT/hromatite.log" 2>/dev/null
cp -a "$PREFIX/drive_c/users/steamuser/hromatite_eac_hooks.log" "$OUT/hromatite_eac_hooks.log" 2>/dev/null

pkill -f 'VRChat[.]exe'
sleep 3
cp -a "$BAK" "$MODE"
sha_after=$(sha256sum "$MODE" | cut -d' ' -f1)
echo "[live] mode restored: $sha_before -> $sha_after $( [ "$sha_before" = "$sha_after" ] && echo OK || echo MISMATCH )"
echo "[live] artifacts in $OUT:"
ls -la "$OUT"
echo "[live] hooks log tail:"
tail -25 "$OUT/hromatite_eac_hooks.log" 2>/dev/null
echo "[live] disconnect markers in the game log:"
grep -iE "DisconnectByDisconnectMessage|could not maintain|unable to establish|code 104|Anti-Cheat" \
  "$OUT/game_output_log.txt" 2>/dev/null | tail -10
