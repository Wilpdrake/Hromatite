#!/bin/sh
# hromatite-overlay — окно телеметрии поверх игры, целиком на стороне хоста.
#
# В игру ничего не добавляется: payload пишет ответы `pos` / `who` в свой
# обычный лог-файл, а этот процесс читает лог, шлёт команды в hromatite-cmd.txt
# и рисует полупрозрачную панель в отдельном X11-окне (override-redirect,
# всегда сверху, клики проходят насквозь через пустой ShapeInput).
#
#   tools/hromatite-overlay.sh [--prefix DIR] [--x N] [--y N] [--w N] [--h N]
#
# Почему nix-shell: бинарник слинкован с libX11/libXext из nix-store, и на
# NixOS их надо явно положить в LD_LIBRARY_PATH (в системном профиле libX11
# есть не как .so, а только через зависимости).
#
# HROMATITE_DISPLAY / HROMATITE_XAUTHORITY переопределяют автоопределение.

set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$ROOT/crates/hromatite-overlay/target/x86_64-unknown-linux-gnu/release/hromatite-overlay"
PREFIX="${HROMATITE_PREFIX:-$HOME/.local/share/Steam/steamapps/compatdata/438100/pfx}"

if [ ! -x "$BIN" ]; then
    echo "overlay: сборка…" >&2
    ( cd "$ROOT" && nix-shell -p xorg.libX11 xorg.libXext --run \
        "cargo build --release --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu" ) || exit 1
fi

# Xwayland под Plasma Wayland: берём DISPLAY и XAUTHORITY у живого X-клиента
# (у агентских сессий своего DISPLAY нет).
if [ -z "${HROMATITE_DISPLAY:-}" ]; then
    HROMATITE_DISPLAY=":0"
    for p in $(pgrep -u "$(id -u)" -f 'Xwayland|Xorg' 2>/dev/null); do
        d=$(tr '\0' '\n' <"/proc/$p/cmdline" 2>/dev/null | sed -n 's/^:\([0-9]\+\)$/\1/p' | head -1)
        [ -n "${d:-}" ] && HROMATITE_DISPLAY=":$d" && break
    done
fi
if [ -z "${HROMATITE_XAUTHORITY:-}" ]; then
    for p in $(pgrep -u "$(id -u)" -f 'Xwayland|Xorg' 2>/dev/null); do
        a=$(tr '\0' '\n' <"/proc/$p/cmdline" 2>/dev/null | sed -n 's/^-auth$//; t; p' | grep '^/run/user' | head -1)
        [ -n "${a:-}" ] && HROMATITE_XAUTHORITY="$a" && break
    done
fi
[ -n "${HROMATITE_XAUTHORITY:-}" ] || HROMATITE_XAUTHORITY=$(ls /run/user/"$(id -u)"/xauth_* 2>/dev/null | head -1)

echo "overlay: display=$HROMATITE_DISPLAY xauth=${HROMATITE_XAUTHORITY:-<none>} prefix=$PREFIX" >&2

exec nix-shell -p xorg.libX11 xorg.libXext --run "
    export LD_LIBRARY_PATH=\$(echo \$NIX_LDFLAGS | tr ' ' '\n' | grep '^-L' | sed 's/^-L//' | paste -sd:)\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}
    exec env DISPLAY='$HROMATITE_DISPLAY' XAUTHORITY='${HROMATITE_XAUTHORITY:-}' '$BIN' --prefix '$PREFIX' $*
"
