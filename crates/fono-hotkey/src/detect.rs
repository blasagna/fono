// SPDX-License-Identifier: GPL-3.0-only
//! Hotkey backend selection.
//!
//! Picks between the X11 (`global-hotkey`) listener and the
//! Wayland-portal (`xdg-desktop-portal.GlobalShortcuts`) listener at
//! daemon startup, based on the session environment.
//!
//! On Wayland the two big desktops get a native shim ahead of the
//! portal, because `xdg-desktop-portal` cannot attribute an app id to
//! an unsandboxed daemon: [`crate::kde_kglobalaccel`] on KDE and
//! [`crate::gnome_gsettings`] on GNOME. Each falls through to the
//! portal, and then to X11, if it can't register.
//!
//! Auto-detection is the only path users ever hit. The
//! `FONO_HOTKEY_BACKEND={portal,x11,disabled}` env var is a diagnostic
//! escape hatch — unknown values fall through to auto-detection with a
//! warning, matching the `FONO_OVERLAY_BACKEND` selector in
//! `fono-overlay`.

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::info;

use crate::fsm::HotkeyAction;
use crate::listener::{HotkeyBindings, ListenerHandle};
use crate::KeyHeldFlags;

/// Forced backend override, parsed from `FONO_HOTKEY_BACKEND`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyBackend {
    /// `xdg-desktop-portal.GlobalShortcuts` (Wayland sessions).
    Portal,
    /// The X11 / Xwayland `global-hotkey` listener. On macOS this is
    /// the same listener with its Carbon `RegisterEventHotKey`
    /// backend — the variant name is historical.
    X11,
    /// Skip the listener entirely (headless / SSH / CI runners).
    Disabled,
}

impl HotkeyBackend {
    /// Parse the value of `FONO_HOTKEY_BACKEND` into a forced backend
    /// selection. Returns `None` for empty / unknown input so the
    /// caller falls back to auto-detection.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "portal" => Some(Self::Portal),
            "x11" => Some(Self::X11),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }

    /// Human-facing label for logs. The `X11` variant is really the
    /// cross-platform `global-hotkey` listener, so name it after the
    /// OS-native backend it actually drives — otherwise Windows and
    /// macOS logs confusingly report "X11" on a machine that has no X
    /// server. The enum variant name stays `X11` for backwards
    /// compatibility with `FONO_HOTKEY_BACKEND=x11`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Portal => "Wayland portal",
            Self::Disabled => "disabled",
            #[cfg(target_os = "windows")]
            Self::X11 => "Win32 RegisterHotKey",
            #[cfg(target_os = "macos")]
            Self::X11 => "Carbon RegisterEventHotKey",
            #[cfg(target_os = "linux")]
            Self::X11 => "X11",
            #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
            Self::X11 => "global-hotkey",
        }
    }
}

/// Resolve a backend for the current session. `forced` is the parsed
/// `FONO_HOTKEY_BACKEND` value (`None` = auto-detect).
///
/// Auto-detect matrix (Linux):
/// - `WAYLAND_DISPLAY` set → `Portal` (which [`spawn`] may resolve to
///   the KDE or GNOME shim first; the portal listener also falls back
///   gracefully at spawn time if `xdg-desktop-portal-*` isn't running).
/// - `DISPLAY` set, no `WAYLAND_DISPLAY` → `X11`.
/// - Neither set → `Disabled`.
///
/// macOS / Windows: always the `global-hotkey` listener (the `X11`
/// variant — same listener, OS-native backend: Carbon
/// `RegisterEventHotKey` on macOS, Win32 `RegisterHotKey` on Windows).
/// The `WAYLAND_DISPLAY` / `DISPLAY` probes below are Linux session
/// signals with no meaning on those targets, and the daemon only calls
/// [`spawn`] inside a real desktop session; registration failures
/// degrade gracefully in the listener itself.
#[must_use]
pub fn detect_backend(forced: Option<HotkeyBackend>) -> HotkeyBackend {
    detect_backend_with(forced, |k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Test seam for [`detect_backend`] with an injectable env-lookup.
#[doc(hidden)]
pub fn detect_backend_with(
    forced: Option<HotkeyBackend>,
    env_present: impl Fn(&str) -> bool,
) -> HotkeyBackend {
    if let Some(b) = forced {
        return b;
    }
    // Non-Linux (macOS, Windows): the global-hotkey listener drives the
    // OS-native backend directly, so the Linux-only display-server
    // probes below never apply — resolve straight to the listener.
    if !cfg!(target_os = "linux") {
        return HotkeyBackend::X11;
    }
    if env_present("WAYLAND_DISPLAY") {
        HotkeyBackend::Portal
    } else if env_present("DISPLAY") {
        HotkeyBackend::X11
    } else {
        HotkeyBackend::Disabled
    }
}

/// Which backend [`spawn`] will actually try first, phrased for humans
/// (`fono doctor`). Unlike [`detect_backend`] this names the
/// desktop-native shims that sit ahead of the portal on KDE and GNOME.
/// Reads environment variables only — no D-Bus, no side effects.
#[must_use]
pub fn describe_backend(forced: Option<HotkeyBackend>) -> &'static str {
    let backend = detect_backend(forced);
    #[cfg(target_os = "linux")]
    if backend == HotkeyBackend::Portal {
        if crate::kde_kglobalaccel::is_kde_session() {
            return "KDE KGlobalAccel via KWin (falls back to the portal, then X11)";
        }
        if crate::gnome_gsettings::is_gnome_session() {
            return "GNOME gsettings custom-keybindings (falls back to the portal, then X11)";
        }
    }
    backend.label()
}

/// Top-level orchestrator: detect, dispatch, return a unified handle.
pub fn spawn(
    forced: Option<HotkeyBackend>,
    bindings: HotkeyBindings,
    tx: mpsc::UnboundedSender<HotkeyAction>,
    held_flags: KeyHeldFlags,
) -> Result<Option<ListenerHandle>> {
    let backend = detect_backend(forced);
    info!("hotkey backend resolved: {} (forced: {forced:?})", backend.label());
    match backend {
        HotkeyBackend::Portal => {
            #[cfg(target_os = "linux")]
            {
                // GNOME-Wayland short-circuit. On any version of
                // xdg-desktop-portal-gnome the portal path is a
                // dead-end for unsandboxed Fono builds:
                //
                // * v46 (Ubuntu 24.04) — the `GlobalShortcuts`
                //   interface isn't implemented at all.
                // * v47+ — `CreateSession` rejects unsandboxed
                //   callers with
                //   `org.freedesktop.portal.Error.NotAllowed: An
                //   app id is required`.
                //
                // Both surface as scary warns in the log even though
                // the gsettings shim already handles them. Skip the
                // portal preflight entirely on GNOME-Wayland and go
                // straight to the deterministic happy path.
                if crate::gnome_gsettings::is_gnome_session() {
                    match crate::gnome_gsettings::spawn(
                        &bindings.dictation,
                        &bindings.assistant,
                        &bindings.cancel,
                    ) {
                        Ok((exe, handle)) => {
                            info!(
                                "GNOME-Wayland: registered gsettings custom-keybindings \
                                 {} → {} toggle, {} → {} assistant; \
                                 cancel key {:?} bound dynamically while recording",
                                bindings.dictation,
                                exe.display(),
                                bindings.assistant,
                                exe.display(),
                                bindings.cancel,
                            );
                            return Ok(Some(handle));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "GNOME gsettings install failed: {e:#}; \
                                 trying portal then X11"
                            );
                            // Fall through to the portal attempt
                            // below — last-resort safety net.
                        }
                    }
                }
                // KDE-Wayland short-circuit, for the same reason as
                // GNOME's: xdg-desktop-portal derives an unsandboxed
                // caller's app id from whatever systemd unit it landed
                // in, so Fono is either rejected outright
                // (`org.freedesktop.portal.Error.NotAllowed: An app id
                // is required`) or has its keys filed under the
                // terminal that launched it. KWin's own KGlobalAccel
                // service — the one the KDE portal is implemented on
                // top of — takes us directly, keeps press *and* release
                // events, and files the bindings under "Fono".
                //
                // The portal remains the path for sway, Hyprland and
                // wlroots, where it works.
                if crate::kde_kglobalaccel::is_kde_session() {
                    match crate::kde_kglobalaccel::spawn(
                        bindings.clone(),
                        tx.clone(),
                        held_flags.clone(),
                    ) {
                        Ok(h) => return Ok(Some(h)),
                        Err(e) => {
                            tracing::warn!(
                                "KDE KGlobalAccel registration failed: {e:#}; \
                                 trying portal then X11"
                            );
                            // Fall through to the portal attempt
                            // below — last-resort safety net.
                        }
                    }
                }
                match crate::portal::spawn(bindings.clone(), tx.clone(), held_flags.clone()) {
                    Ok(h) => Ok(Some(h)),
                    Err(e) => {
                        tracing::warn!("portal hotkey backend unavailable: {e:#}");
                        tracing::warn!("falling back to X11 listener (Xwayland-only events)");
                        crate::listener::spawn(bindings, tx, held_flags).map(Some)
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (bindings, tx, held_flags);
                anyhow::bail!("portal hotkey backend is Linux-only");
            }
        }
        HotkeyBackend::X11 => crate::listener::spawn(bindings, tx, held_flags).map(Some),
        HotkeyBackend::Disabled => {
            info!("hotkey listener disabled (headless session)");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_canonical_names() {
        assert_eq!(HotkeyBackend::parse("portal"), Some(HotkeyBackend::Portal));
        assert_eq!(HotkeyBackend::parse("Portal"), Some(HotkeyBackend::Portal));
        assert_eq!(HotkeyBackend::parse("x11"), Some(HotkeyBackend::X11));
        assert_eq!(HotkeyBackend::parse("disabled"), Some(HotkeyBackend::Disabled));
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert_eq!(HotkeyBackend::parse(""), None);
        assert_eq!(HotkeyBackend::parse("auto"), None);
        assert_eq!(HotkeyBackend::parse("wayland"), None);
        assert_eq!(HotkeyBackend::parse("bogus"), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn auto_detect_picks_portal_on_wayland() {
        let b = detect_backend_with(None, |k| k == "WAYLAND_DISPLAY");
        assert_eq!(b, HotkeyBackend::Portal);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn auto_detect_picks_x11_on_xorg() {
        let b = detect_backend_with(None, |k| k == "DISPLAY");
        assert_eq!(b, HotkeyBackend::X11);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn auto_detect_disabled_when_headless() {
        let b = detect_backend_with(None, |_| false);
        assert_eq!(b, HotkeyBackend::Disabled);
    }

    /// macOS has no display env vars — auto-detect always lands on the
    /// global-hotkey (Carbon) listener; headless gating happens in the
    /// daemon via the WindowServer-session probe, not here.
    #[test]
    #[cfg(target_os = "macos")]
    fn auto_detect_on_macos_is_always_the_global_hotkey_listener() {
        assert_eq!(detect_backend_with(None, |_| false), HotkeyBackend::X11);
        assert_eq!(detect_backend_with(None, |_| true), HotkeyBackend::X11);
    }

    /// Windows has no `DISPLAY` / `WAYLAND_DISPLAY` — auto-detect must
    /// still land on the global-hotkey (Win32 `RegisterHotKey`) listener
    /// rather than falling through to `Disabled`. Regression guard for
    /// Windows port plan Task 8.1.
    #[test]
    #[cfg(target_os = "windows")]
    fn auto_detect_on_windows_is_always_the_global_hotkey_listener() {
        assert_eq!(detect_backend_with(None, |_| false), HotkeyBackend::X11);
        assert_eq!(detect_backend_with(None, |_| true), HotkeyBackend::X11);
    }

    #[test]
    fn forced_override_wins() {
        // Forced X11 wins even with WAYLAND_DISPLAY set.
        let b = detect_backend_with(Some(HotkeyBackend::X11), |_| true);
        assert_eq!(b, HotkeyBackend::X11);
    }
}
