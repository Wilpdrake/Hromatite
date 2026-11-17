#!/usr/bin/env bash
# eac_probe.sh — чек-лист живости цепочки EAC/Photon по логам прогона.
#
#   tools/eac_probe.sh <log|каталог> [ещё каталог ...]
#
# Аргументом можно дать один файл (hromatite.log, hooks-лог, output_log_*.txt)
# или каталог evidence: скрипт сам разложит найденные *.log/*.txt на «наши»
# логи (hromatite.log, *hooks*.log) и логи игры (output_log_*, game_output_log*).
#
# Печатает таблицу PASS/FAIL/UNKNOWN по живым артефактам и строку VERDICT.
# Коды выхода: 0 — PASS, 1 — FAIL, 2 — INCONCLUSIVE (не хватает данных).
#
# Переменные:
#   EAC_PROBE_NAME_FRAC   доля «похожих на Namespace.Name» имён в sample класса,
#                         ниже которой таблица классов считается битой (0.6)
#   EAC_PROBE_MAX_EVID    сколько символов цитаты печатать (150)
set -uo pipefail
export LC_ALL=C

usage() {
    printf 'usage: %s <log|dir> [more ...]\n' "${0##*/}" >&2
    exit 64
}
[ $# -ge 1 ] || usage

NAME_FRAC="${EAC_PROBE_NAME_FRAC:-0.6}"
MAX_EVID="${EAC_PROBE_MAX_EVID:-150}"

PAYLOAD=()   # наши логи: hromatite.log, hooks-лог
GAME=()      # логи игры: output_log_*, game_output_log*
for a in "$@"; do
    if [ ! -e "$a" ]; then
        printf 'eac_probe: нет такого пути: %s\n' "$a" >&2
        exit 64
    fi
    if [ -d "$a" ]; then
        while IFS= read -r f; do
            b=$(basename -- "$f")
            case "$b" in
                output_log_*.txt | game_output_log*.txt) GAME+=("$f") ;;
                *) PAYLOAD+=("$f") ;;
            esac
        done < <(find "$a" -maxdepth 3 -type f \( -name '*.log' -o -name '*.txt' \) 2>/dev/null | sort)
    else
        case "$(basename -- "$a")" in
            output_log_*.txt | game_output_log*.txt) GAME+=("$a") ;;
            *) PAYLOAD+=("$a") ;;
        esac
    fi
done
[ ${#PAYLOAD[@]} -eq 0 ] && PAYLOAD=("${GAME[@]}")
[ ${#GAME[@]} -eq 0 ] && GAME=("${PAYLOAD[@]}")

# --- helpers ----------------------------------------------------------------
trim() { printf '%s' "$1" | cut -c1-"$MAX_EVID"; }

# Последняя строка по регулярке (chronological enough: файлы в порядке аргументов).
lastline() {
    local pat=$1
    shift
    [ $# -gt 0 ] || return 1
    grep -aEh -- "$pat" "$@" 2>/dev/null | tr -d '\r' | tail -1
}

count() {
    local pat=$1
    shift
    [ $# -gt 0 ] || {
        echo 0
        return
    }
    # shellcheck disable=SC2126  # grep -c would print one count per file
    grep -aEh -- "$pat" "$@" 2>/dev/null | wc -l | tr -d ' '
}

# ESC-коды в логах не должны ломать таблицу.
sanitize() { printf '%s' "$1" | tr -d '\000-\010\013\014\016-\037' | tr -s ' '; }

STATUS=()   # id=status, для VERDICT
FIELDS=()   # строки таблицы

row() { # id, title, status, evidence
    FIELDS+=("$(printf '%-3s %-38s %-7s %s' "$1" "$2" "$3" "$(trim "$(sanitize "$4")")")")
    STATUS+=("$1=$3")
}

# --- (a) хуки EOSSDK --------------------------------------------------------
a=$(lastline 'hooks installed [0-9]+/[0-9]+' "${PAYLOAD[@]}")
if [ -z "$a" ]; then
    row a "EOSSDK hooks installed" UNKNOWN "нет строки 'hooks installed N/M'"
else
    ok=$(printf '%s' "$a" | sed -n 's/.*hooks installed \([0-9]\+\)\/\([0-9]\+\).*/\1/p')
    tot=$(printf '%s' "$a" | sed -n 's/.*hooks installed \([0-9]\+\)\/\([0-9]\+\).*/\2/p')
    if [ -n "$ok" ] && [ -n "$tot" ] && [ "$ok" -ge "$tot" ] && [ "$tot" -ge 8 ]; then
        row a "EOSSDK hooks installed" PASS "$a"
    elif [ -n "$ok" ] && [ -n "$tot" ] && [ "$ok" -lt "$tot" ]; then
        row a "EOSSDK hooks installed" FAIL "$a"
    else
        row a "EOSSDK hooks installed" UNKNOWN "$a"
    fi
fi

# --- (b) таблица классов m3 -------------------------------------------------
b_tab=$(lastline '\[m3\] class table:' "${PAYLOAD[@]}")
if [ -z "$b_tab" ]; then
    # Прежние сборки печатали записи без заголовка "class table:", только
    # `[m3]   [i] entry=.. ns=".." name=".."`. Это тоже найденная таблица.
    b_tab=$(grep -aEh -- '\[m3\].*entry=.*ns="' "${PAYLOAD[@]}" 2>/dev/null | tr -d '\r' | tail -1)
fi
if [ -z "$b_tab" ]; then
    row b "m3 class table located" UNKNOWN "нет строки '[m3] class table:'"
else
    case "$b_tab" in
        *"nothing scored"*)
            row b "m3 class table located" FAIL "$b_tab"
            ;;
        *)
            # Sample: "[0] Flush, [1] null, [2] System.String ..." — прежний формат
            # печатал ns=/name= по отдельности, поддерживаем оба.
            sample=""
            sample=$(lastline 'class table: sample ' "${PAYLOAD[@]}")
            [ -z "$sample" ] && sample=$(grep -aEh -- '\[m3\].*entry=.*ns="' "${PAYLOAD[@]}" 2>/dev/null | tr -d '\r' | tail -6)
            names=()
            if [ -n "$sample" ]; then
                case "$sample" in
                    *"ns="*)
                        # [N] entry=0x.. ns="X" name="Y"
                        while IFS= read -r n; do names+=("$n"); done < <(
                            printf '%s\n' "$sample" | sed -n 's/.*ns="\([^"]*\)" name="\([^"]*\)".*/\1.\2/p'
                        )
                        ;;
                    *)
                        # class table: sample [0] A.B, [1] null, ...
                        body=${sample#*sample }
                        old_ifs=$IFS
                        IFS=','
                        # shellcheck disable=SC2206
                        parts=($body)
                        IFS=$old_ifs
                        for p in "${parts[@]}"; do
                            p=$(printf '%s' "$p" | sed 's/^ *\[[0-9]*\] *//; s/^ *//; s/ *$//')
                            [ -n "$p" ] && names+=("$p")
                        done
                        ;;
                esac
            fi
            tot=${#names[@]}
            good=0
            dotted=0
            for n in "${names[@]}"; do
                [ "$n" = "null" ] && continue
                if printf '%s' "$n" | grep -qE '^[A-Za-z_][A-Za-z0-9_]{1,}(\.[A-Za-z_][A-Za-z0-9_]*)*$'; then
                    good=$((good + 1))
                    case "$n" in
                        *.*) dotted=$((dotted + 1)) ;;
                    esac
                fi
            done
            # Для цитаты — строка о том, где таблица нашлась (не sample).
            b_loc=$(grep -aEh -- '\[m3\] class table:' "${PAYLOAD[@]}" 2>/dev/null | tr -d '\r' \
                | grep -v 'class table: sample' | tail -1)
            [ -z "$b_loc" ] && b_loc=$b_tab
            if [ "$tot" -eq 0 ]; then
                row b "m3 class table located" UNKNOWN "$b_loc (sample не печатался)"
            else
                frac=$(awk -v g="$good" -v t="$tot" 'BEGIN { printf "%.2f", (t ? g / t : 0) }')
                verdict=$(awk -v f="$frac" -v lim="$NAME_FRAC" 'BEGIN { print (f + 0 >= lim + 0) ? "PASS" : "FAIL" }')
                row b "m3 class table located" "$verdict" \
                    "$b_loc | имена-идентификаторы $good/$tot (Namespace.Name $dotted/$tot), порог $NAME_FRAC"
            fi
            ;;
    esac
fi

# --- (c) Photon: SendOperation захукан --------------------------------------
c=$(lastline '\[photon\] hooked .*method=' "${PAYLOAD[@]}")
if [ -n "$c" ]; then
    row c "Photon SendOperation hooked" PASS "$c"
else
    c_fail=$(lastline '\[photon\].*(not found|no .* found|resolve)' "${PAYLOAD[@]}")
    if [ -n "$c_fail" ]; then
        row c "Photon SendOperation hooked" FAIL "$c_fail"
    else
        row c "Photon SendOperation hooked" UNKNOWN "нет строки '[photon] hooked ... method='"
    fi
fi

# --- (d) раскладка словаря --------------------------------------------------
d=$(lastline '\[photon\] dictionary layout:' "${PAYLOAD[@]}")
if [ -n "$d" ]; then
    row d "Photon dictionary layout" PASS "$d"
else
    d_fail=$(lastline 'dictionary probe failed' "${PAYLOAD[@]}")
    if [ -n "$d_fail" ]; then
        row d "Photon dictionary layout" FAIL "$d_fail"
    else
        row d "Photon dictionary layout" UNKNOWN "нет строки 'dictionary layout:'"
    fi
fi

# --- (e) событие 66 ---------------------------------------------------------
e_n=$(count '\[photon\] event 66 len=' "${PAYLOAD[@]}")
e=$(lastline '\[photon\] event 66 len=' "${PAYLOAD[@]}")
if [ "$e_n" -gt 0 ]; then
    row e "event 66 observed" PASS "$e_n шт: $e"
else
    e_cap=$(lastline '\[photon\] captured [0-9]+ events' "${PAYLOAD[@]}")
    if [ -n "$e_cap" ]; then
        row e "event 66 observed" FAIL "$e_cap, но event 66 среди них нет"
    else
        row e "event 66 observed" UNKNOWN "нет 'event 66 len=' (захват событий не включался?)"
    fi
fi

# --- (f) кадры 512 Б и медианный dt -----------------------------------------
f_frames=$(grep -aEh -- '\[photon\] event 66 len=' "${PAYLOAD[@]}" 2>/dev/null | tr -d '\r' \
    | sed -n 's/.*event 66 len=\([0-9]\+\).*/\1/p')
f_n512=$(printf '%s\n' "$f_frames" | grep -c '^512$')
f_total=$(printf '%s\n' "$f_frames" | grep -c '^[0-9]\+$')
if [ "$f_n512" -ge 1 ]; then
    dts=$(grep -aEh -- '\[photon\] event 66 len=512 dt=' "${PAYLOAD[@]}" 2>/dev/null | tr -d '\r' \
        | sed -n 's/.*dt=\([0-9]\+\) ms.*/\1/p')
    med=$(printf '%s\n' "$dts" | grep -E '^[0-9]+$' | sort -n | awk '
        { a[NR] = $1 }
        END {
            if (NR == 0) { print "-" }
            else if (NR % 2) { print a[(NR + 1) / 2] }
            else { printf "%.0f", (a[NR / 2] + a[NR / 2 + 1]) / 2 }
        }')
    note=""
    if [ "$med" != "-" ]; then
        if [ "$med" -lt 1200 ] || [ "$med" -gt 3000 ]; then
            note=" (dt вне 1200..3000 мс — проверь каденс)"
        fi
    fi
    row f "512-byte frames / median dt" PASS \
        "512B: $f_n512 из $f_total event 66; медианный dt=${med} мс$note"
else
    if [ "$f_total" -gt 0 ]; then
        row f "512-byte frames / median dt" FAIL "event 66 есть ($f_total), но кадров 512 Б нет"
    else
        row f "512-byte frames / median dt" UNKNOWN "нет event 66 — кадров не видели"
    fi
fi

# --- (g) отправки эмулятора -------------------------------------------------
g_sent=$(count '\[photon\] sent event 66:' "${PAYLOAD[@]}")
g_fail=$(count '\[photon\] send failed' "${PAYLOAD[@]}")
g_last=$(lastline '\[photon\] sent event 66:' "${PAYLOAD[@]}")
if [ "$g_sent" -gt 0 ]; then
    row g "emulated sends" PASS "$g_sent отправок; последняя: $g_last"
elif [ "$g_fail" -gt 0 ]; then
    row g "emulated sends" FAIL "нет успешных отправок, $g_fail неудач: $(lastline '\[photon\] send failed' "${PAYLOAD[@]}")"
elif [ -n "$(lastline 'emulate' "${PAYLOAD[@]}")" ]; then
    row g "emulated sends" FAIL "$(lastline 'emulate' "${PAYLOAD[@]}")"
else
    row g "emulated sends" UNKNOWN "эмуляция не включалась (нет 'sent event 66')"
fi

# --- (h) кик античита -------------------------------------------------------
h=$(lastline 'code 104|verify installation integrity|Anti-cheat client not available|DisconnectByDisconnectMessage' "${GAME[@]}")
h_n=$(count 'code 104|verify installation integrity|Anti-cheat client not available' "${GAME[@]}")
if [ ${#GAME[@]} -eq 0 ]; then
    row h "no EAC kick" UNKNOWN "лог игры не найден"
elif [ "$h_n" -gt 0 ]; then
    row h "no EAC kick" FAIL "$h_n совпадений; последнее: $h"
else
    world=$(lastline 'Entering Room' "${GAME[@]}")
    if [ -n "$world" ]; then
        row h "no EAC kick" PASS "кика нет; $(printf '%s' "$world" | cut -c1-80)"
    else
        row h "no EAC kick" UNKNOWN "кика нет, но и 'Entering Room' не видно (игра дошла до мира?)"
    fi
fi

# --- вывод ------------------------------------------------------------------
printf '\n=== eac_probe ===\n'
printf 'источники: %s\n' "$*" 
printf '\n%-3s %-38s %-7s %s\n' "#" "проверка" "статус" "цитата"
printf '%s\n' "-------------------------------------------------------------------------------"
for line in "${FIELDS[@]}"; do printf '%s\n' "$line"; done
printf '%s\n' "-------------------------------------------------------------------------------"

# Критичные пункты: хуки, таблица классов, фотон-хук, событие 66, отсутствие кика.
fail=()
unk=()
for s in "${STATUS[@]}"; do
    id=${s%%=*}
    st=${s#*=}
    # shellcheck disable=SC2194  # literal list of critical check ids
    case " a b c e h " in
        *" $id "*) ;;
        *) continue ;;
    esac
    case "$st" in
        FAIL) fail+=("$id") ;;
        UNKNOWN) unk+=("$id") ;;
    esac
done

if [ ${#fail[@]} -gt 0 ]; then
    printf 'VERDICT: FAIL — провалены: %s\n' "${fail[*]}"
    rc=1
elif [ ${#unk[@]} -gt 0 ]; then
    printf 'VERDICT: INCONCLUSIVE — нет данных по: %s\n' "${unk[*]}"
    rc=2
else
    printf 'VERDICT: PASS — хуки 8/8, таблица классов, photon-хук и event 66 на месте, кика нет\n'
    rc=0
fi
printf 'некритичные: %s\n' "$(printf '%s ' "${STATUS[@]}" | tr ' ' '\n' | grep -E '^(d|f|g)=' | tr '\n' ' ')"
exit "$rc"
