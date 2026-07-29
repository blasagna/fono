#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Does the overlay come back after a hide?
#
# GitHub #4 / the 2026-07-28 regression: on KDE the recording overlay
# appeared on the first dictation and never again. Verifying that by
# hand means dictating twice and looking at the screen, which is slow,
# unrepeatable, and (on a real session) means screenshotting the user's
# desktop. This does it offline instead:
#
#   1. a nested, headless kwin_wayland renders to a virtual framebuffer
#      on its own private D-Bus session — no keyboard, no outputs, no
#      contact with the running desktop;
#   2. `examples/overlay_show_hide_probe` connects to it and cycles the
#      overlay show → hide → show → hide → show, cueing us each time it
#      reaches a steady state;
#   3. we screenshot the nested output at each cue (the nested KWin
#      hosts its own org.kde.KWin.ScreenShot2, so spectacle talks to it
#      and only it), and compare the frames.
#
# Each frame is diffed against a baseline taken while the overlay was
# hidden. A visible phase has to change the rectangle the overlay
# occupies (and nothing else); a hidden phase has to change nothing.
# That catches all three ways this has broken so far: the overlay never
# comes back, it comes back somewhere else, or it never goes away.
#
# Usage:
#   tests/overlay-show-hide-check.sh              # build + run
#   tests/overlay-show-hide-check.sh --no-build   # reuse last build
#   tests/overlay-show-hide-check.sh --keep       # keep the PNGs
#
# Environment:
#   FONO_PROBE_PANEL=1     run plasmashell in the nested session too, so
#                          the overlay has a real Plasma panel to be
#                          stacked against (slower; needs plasmashell)
#   FONO_PROBE_CYCLES=N    show/hide cycles (default 3)
#   FONO_PROBE_WIDTH/_HEIGHT  nested output size (default 1920x1080)
#
# Requires: kwin_wayland, spectacle, dbus-run-session, ImageMagick.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${FONO_OVERLAY_CHECK_OUT:-${TMPDIR:-/tmp}/fono-overlay-check}"

# Matches this laptop's panel so the geometry the overlay is asked for
# is the geometry it gets in the real session. Every frame is asserted
# to be exactly this size, so a screenshot that somehow came from
# somewhere other than the nested output is rejected, not analysed.
W="${FONO_PROBE_WIDTH:-1920}"
H="${FONO_PROBE_HEIGHT:-1080}"
CYCLES="${FONO_PROBE_CYCLES:-3}"
SETTLE_MS="${FONO_PROBE_SETTLE_MS:-1500}"

BUILD=1
KEEP=0
for arg in "$@"; do
    case "$arg" in
        --no-build) BUILD=0 ;;
        --keep) KEEP=1 ;;
        -h | --help)
            sed -n '3,38p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

for tool in kwin_wayland spectacle dbus-run-session magick; do
    command -v "$tool" >/dev/null || {
        echo "missing required tool: $tool" >&2
        exit 2
    }
done

PROBE="$REPO/target/debug/examples/overlay_show_hide_probe"
if [ "$BUILD" = 1 ]; then
    echo "==> building probe"
    cargo build -q -p fono-overlay --example overlay_show_hide_probe \
        --features backend-wlr || exit 1
fi
[ -x "$PROBE" ] || {
    echo "probe not built: $PROBE" >&2
    exit 2
}

rm -rf "$OUT"
mkdir -p "$OUT/run"
chmod 700 "$OUT/run"

# ---------------------------------------------------------------------
# Inner session: private D-Bus + private XDG_RUNTIME_DIR + nested KWin.
# ---------------------------------------------------------------------
cat > "$OUT/session.sh" << 'INNER'
set -uo pipefail
export XDG_CURRENT_DESKTOP=KDE

# Two ways to host the nested compositor:
#
#   --virtual (default)  renders to a virtual framebuffer, offscreen and
#                        fully isolated. But KWin composites it in
#                        software, and software compositing turns
#                        *animations off* — so the map/unmap effects
#                        that a real session applies to a layer surface
#                        never run here.
#   FONO_PROBE_HOST_DISPLAY=wayland-0
#                        runs the nested KWin as a window inside the
#                        host session, which puts it on the same OpenGL
#                        path (and therefore the same effects) as the
#                        real desktop. Visible on screen while it runs;
#                        capture is still of the nested output only.
if [ -n "${FONO_PROBE_HOST_DISPLAY:-}" ]; then
    export WAYLAND_DISPLAY="$FONO_PROBE_HOST_DISPLAY"
    BACKEND=(--wayland-display "$FONO_PROBE_HOST_DISPLAY")
else
    export XDG_RUNTIME_DIR="$OUT/run"
    unset WAYLAND_DISPLAY DISPLAY
    BACKEND=(--virtual)
fi

kwin_wayland "${BACKEND[@]}" --width "$W" --height "$H" --no-lockscreen \
    --socket fono-verify > "$OUT/kwin.log" 2>&1 &
KWIN=$!
trap 'kill $KWIN 2>/dev/null' EXIT

for _ in $(seq 60); do
    [ -S "$XDG_RUNTIME_DIR/fono-verify" ] && break
    sleep 0.2
done
[ -S "$XDG_RUNTIME_DIR/fono-verify" ] || {
    echo "nested kwin never created its socket; see $OUT/kwin.log" >&2
    exit 3
}
# Let KWin settle before the first client connects.
sleep 1

# Optional: a real Plasma panel in the nested session. The panel is a
# layer-shell surface too, and it sits at the bottom of the screen —
# exactly where the overlay anchors — so it is the obvious candidate
# for a re-mapped overlay ending up stacked underneath something.
if [ "${FONO_PROBE_PANEL:-0}" = 1 ]; then
    WAYLAND_DISPLAY=fono-verify QT_QPA_PLATFORM=wayland \
        plasmashell > "$OUT/plasmashell.log" 2>&1 &
    PANEL=$!
    trap 'kill $PANEL $KWIN 2>/dev/null' EXIT
    sleep 12
fi

shoot() { # shoot <name>
    WAYLAND_DISPLAY=fono-verify QT_QPA_PLATFORM=wayland \
        spectacle -b -n -f -o "$OUT/$1.png" > /dev/null 2>&1
}

export WAYLAND_DISPLAY=fono-verify
export FONO_OVERLAY_BACKEND=wlr
export FONO_PROBE_CYCLES FONO_PROBE_SETTLE_MS
"$PROBE" 2> "$OUT/probe.log" | while read -r kind name; do
    case "$kind" in
        BACKEND) echo "$name" > "$OUT/backend" ;;
        GEOM) echo "$name" > "$OUT/geom" ;;
        CUE)
            shoot "$name"
            echo "$name" >> "$OUT/cues"
            ;;
    esac
done
INNER

echo "==> running ${CYCLES} show/hide cycles in a nested headless kwin"
# Output goes to a file rather than a pipe on purpose: the nested
# session activates portal services that inherit our stdout and outlive
# the shell, so a pipe here would never see EOF and the script would
# hang after a perfectly good run.
OUT="$OUT" W="$W" H="$H" PROBE="$PROBE" \
    FONO_PROBE_CYCLES="$CYCLES" FONO_PROBE_SETTLE_MS="$SETTLE_MS" \
    dbus-run-session -- bash "$OUT/session.sh" > "$OUT/session.log" 2>&1

BACKEND="$(cat "$OUT/backend" 2> /dev/null || echo '?')"
echo "==> backend: $BACKEND"
[ "$BACKEND" = "wlr-layer-shell" ] || {
    echo "FAIL: probe did not get the wlr-layer-shell backend (got '$BACKEND')" >&2
    echo "      see $OUT/probe.log" >&2
    exit 1
}

# ---------------------------------------------------------------------
# Analysis. Every frame is diffed against `baseline.png` — the screen
# with the overlay hidden — and we report how much of the screen changed
# and where. Measuring the difference rather than absolute brightness is
# what lets the check run against a nested desktop that has a wallpaper
# and a Plasma panel on it, and it makes "the overlay is mapped but
# something is drawn on top of it" a failure rather than a pass: an
# occluded overlay changes no pixels.
# ---------------------------------------------------------------------
BASE="$OUT/baseline.png"

# The rectangle the overlay is supposed to occupy, derived from the
# renderer constants the probe reported and the output size — anchored
# bottom, horizontally centred.
read -r RW RH RMARGIN < "$OUT/geom"
RX=$(((W - RW) / 2))
RY=$((H - RMARGIN - RH))
ROI="${RW}x${RH}+${RX}+${RY}"

# The "did anything change where nothing should" test masks out a
# slightly larger box than the ROI: the panel casts a shadow and the
# compositor blurs behind it, both of which land just outside the
# logical rectangle. The slack is tens of pixels; a panel that came
# back in the wrong place is off by hundreds.
PAD=64
MX=$((RX - PAD < 0 ? 0 : RX - PAD))
MY=$((RY - PAD < 0 ? 0 : RY - PAD))
MX2=$((RX + RW - 1 + PAD >= W ? W - 1 : RX + RW - 1 + PAD))
MY2=$((RY + RH - 1 + PAD >= H ? H - 1 : RY + RH - 1 + PAD))
MASK="rectangle $MX,$MY $MX2,$MY2"

# Anything below this share of the screen is treated as noise rather
# than as the overlay: a live desktop has a ticking clock and a system
# tray in it, and those change between frames all by themselves.
NOISE=0.004
# How much of the ROI has to change before we call the overlay visible.
VISIBLE=0.05

measure() { # measure <png> -> "<inside> <outside> <outside-bbox>"
    local png="$1" size inside outside
    size="$(magick identify -format '%wx%h' "$png")"
    [ "$size" = "${W}x${H}" ] || {
        # A frame that isn't the nested output's size did not come from
        # the nested output. Refuse to look at it.
        echo "REFUSED - -"
        return
    }
    # Inside: how much of the overlay's own rectangle changed.
    inside="$(magick "$BASE" "$png" -alpha off -compose difference -composite \
        -crop "$ROI" +repage -colorspace Gray -threshold 8% \
        -format '%[fx:mean]' info:)"
    # Outside: the same measure with the overlay's rectangle blacked
    # out, so a panel that came back in the wrong place still shows up
    # (as change where no change belongs) instead of just vanishing.
    outside="$(magick "$BASE" "$png" -alpha off -compose difference -composite \
        -colorspace Gray -fill black \
        -draw "$MASK" -threshold 8% -format '%[fx:mean]' info:)"
    if [ "$(awk -v v="$outside" -v n="$NOISE" 'BEGIN { print (v > n) ? 1 : 0 }')" = 1 ]; then
        echo "$inside $outside $(magick "$BASE" "$png" -alpha off -compose difference \
            -composite -colorspace Gray -fill black \
            -draw "$MASK" -threshold 8% -format '%@' info: 2> /dev/null)"
    else
        echo "$inside $outside -"
    fi
}

status=0
shots=0
# The probe owns the phase list; we just replay the cues it emitted, so
# the two stay in step when the sequence grows.
[ -s "$OUT/cues" ] || {
    echo "FAIL: probe emitted no cues; see $OUT/probe.log" >&2
    exit 1
}
[ -f "$BASE" ] || {
    echo "FAIL: no baseline frame was captured; see $OUT/probe.log" >&2
    exit 1
}
echo "==> overlay rectangle: $ROI"
echo
printf '%-13s %-9s %-9s %s\n' FRAME IN-RECT ELSEWHERE STRAY-BBOX
while read -r name; do
    [ "$name" = baseline ] && continue
    png="$OUT/$name.png"
    [ -f "$png" ] || {
        printf '%-13s %s\n' "$name" 'MISSING (no screenshot)'
        status=1
        continue
    }
    read -r inside outside stray <<< "$(measure "$png")"
    printf '%-13s %-9s %-9s %s\n' "$name" "$inside" "$outside" "$stray"

    case "$inside" in
        REFUSED*)
            echo "   FAIL: frame is not the nested output's size; refusing to analyse it"
            status=1
            continue
            ;;
    esac
    lit="$(awk -v v="$inside" -v t="$VISIBLE" 'BEGIN { print (v > t) ? 1 : 0 }')"
    elsewhere="$(awk -v v="$outside" -v n="$NOISE" 'BEGIN { print (v > n) ? 1 : 0 }')"

    # Any cue named `hidden-*` must leave the screen as it found it;
    # every other cue is a phase the user is supposed to be able to see.
    case "$name" in
        hidden-*)
            [ "$lit" = 0 ] || {
                echo "   FAIL: overlay still on screen after hide"
                status=1
            }
            continue
            ;;
    esac

    if [ "$lit" = 1 ]; then
        shots=$((shots + 1))
    else
        status=1
        if [ "$elsewhere" = 1 ]; then
            echo "   FAIL: overlay is on screen but not in its rectangle (stray: $stray)"
        else
            echo "   FAIL: overlay not visible at all in this phase"
        fi
        continue
    fi
    [ "$elsewhere" = 0 ] || {
        echo "   FAIL: overlay is in place, but something also changed at $stray"
        status=1
    }
done < "$OUT/cues"

echo
if [ "$status" = 0 ]; then
    echo "PASS: overlay filled $ROI in all $shots visible phases across $CYCLES cycles," \
        "and left no trace between them"
else
    echo "FAIL: see $OUT/{probe,kwin}.log and the frames in $OUT"
fi

if [ "$KEEP" = 0 ] && [ "$status" = 0 ]; then
    rm -rf "$OUT"
else
    echo "frames + logs kept in $OUT"
fi
exit "$status"
