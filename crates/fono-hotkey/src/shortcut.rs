// SPDX-License-Identifier: GPL-3.0-only
//! Shared vocabulary and press/release bookkeeping for the Linux
//! desktop-integration hotkey backends.
//!
//! Both [`crate::portal`] (`org.freedesktop.portal.GlobalShortcuts`)
//! and [`crate::kde_kglobalaccel`] (`org.kde.KGlobalAccel`) register the
//! same three named shortcuts with the compositor and receive
//! press/release notifications back by name. They therefore share:
//!
//! * the shortcut **ids and human descriptions** ([`ALL`]) — these end
//!   up in the user's desktop configuration, so they must not drift
//!   between backends. `kde_kglobalaccel` also matches on the pair to
//!   recognise stray registrations left behind by an earlier portal
//!   attempt (see `find_stray_shortcuts`).
//! * the **short-press / long-press decision** ([`PressTracker`]),
//!   which mirrors the X11 listener: every press emits its action
//!   immediately, and a release emits a second one only if the key was
//!   held past [`LONG_PRESS_THRESHOLD`] (push-to-talk).

use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::warn;

use crate::fsm::HotkeyAction;
use crate::listener::LONG_PRESS_THRESHOLD;
use crate::KeyHeldFlags;

pub const DICTATION: &str = "dictation";
pub const ASSISTANT: &str = "assistant";
pub const CANCEL: &str = "cancel";

pub const DICTATION_DESC: &str = "Toggle voice dictation";
pub const ASSISTANT_DESC: &str = "Toggle voice assistant";
pub const CANCEL_DESC: &str = "Cancel recording / assistant turn";

/// Every `(id, description)` pair Fono registers, in binding order.
pub const ALL: [(&str, &str); 3] =
    [(DICTATION, DICTATION_DESC), (ASSISTANT, ASSISTANT_DESC), (CANCEL, CANCEL_DESC)];

/// Per-role press timestamps, so a release can tell a tap (recording
/// stays latched on) from a hold (release stops it).
#[derive(Debug)]
pub struct PressTracker {
    /// Backend name for log lines — `"portal"` or `"kglobalaccel"`.
    backend: &'static str,
    dictation_press_at: Option<Instant>,
    assistant_press_at: Option<Instant>,
}

impl PressTracker {
    #[must_use]
    pub fn new(backend: &'static str) -> Self {
        Self { backend, dictation_press_at: None, assistant_press_at: None }
    }

    /// Translate a shortcut activation into the matching FSM action and
    /// forward it. Returns `false` once the action channel has closed,
    /// which the caller treats as "shut down".
    pub fn pressed(
        &mut self,
        id: &str,
        held_flags: &KeyHeldFlags,
        tx: &mpsc::UnboundedSender<HotkeyAction>,
    ) -> bool {
        let action = match id {
            DICTATION => {
                self.dictation_press_at = Some(Instant::now());
                held_flags.dictation.store(true, Ordering::Relaxed);
                HotkeyAction::TogglePressed
            }
            ASSISTANT => {
                self.assistant_press_at = Some(Instant::now());
                held_flags.assistant.store(true, Ordering::Relaxed);
                HotkeyAction::AssistantPressed
            }
            // Cancel is a single-shot press: no long-press semantics,
            // no press-timestamp bookkeeping. Mirrors the X11 listener
            // path at `crate::listener::forward_press` for the cancel
            // role.
            CANCEL => HotkeyAction::CancelPressed,
            other => {
                warn!("{}: unknown shortcut id {other:?} activated; ignoring", self.backend);
                return true;
            }
        };
        tracing::debug!("{} pressed {id} -> {action:?}", self.backend);
        tx.send(action).is_ok()
    }

    /// Translate a shortcut release. Short presses emit no extra action
    /// (recording stays latched on); long presses emit a second
    /// `TogglePressed` / `AssistantPressed` to stop (push-to-talk).
    pub fn released(
        &mut self,
        id: &str,
        held_flags: &KeyHeldFlags,
        tx: &mpsc::UnboundedSender<HotkeyAction>,
    ) -> bool {
        let (slot, flag, action) = match id {
            DICTATION => {
                (&mut self.dictation_press_at, &held_flags.dictation, HotkeyAction::TogglePressed)
            }
            ASSISTANT => (
                &mut self.assistant_press_at,
                &held_flags.assistant,
                HotkeyAction::AssistantPressed,
            ),
            // The cancel action already fired on press.
            CANCEL => return true,
            other => {
                warn!("{}: unknown shortcut id {other:?} deactivated; ignoring", self.backend);
                return true;
            }
        };
        flag.store(false, Ordering::Relaxed);
        let Some(t0) = slot.take() else {
            return true;
        };
        if t0.elapsed() >= LONG_PRESS_THRESHOLD {
            tracing::debug!(
                "{} released {id} (held {} ms) -> {action:?}",
                self.backend,
                t0.elapsed().as_millis()
            );
            return tx.send(action).is_ok();
        }
        tracing::debug!("{} released {id} (short press, no synthetic stop)", self.backend);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> (mpsc::UnboundedSender<HotkeyAction>, mpsc::UnboundedReceiver<HotkeyAction>) {
        mpsc::unbounded_channel()
    }

    #[test]
    fn a_tap_emits_one_action_and_a_release_emits_none() {
        let (tx, mut rx) = channel();
        let flags = KeyHeldFlags::new();
        let mut t = PressTracker::new("test");
        assert!(t.pressed(DICTATION, &flags, &tx));
        assert!(flags.dictation.load(Ordering::Relaxed));
        assert!(t.released(DICTATION, &flags, &tx));
        assert!(!flags.dictation.load(Ordering::Relaxed));
        assert_eq!(rx.try_recv().unwrap(), HotkeyAction::TogglePressed);
        assert!(rx.try_recv().is_err(), "a short press must not synthesise a stop");
    }

    #[test]
    fn a_hold_synthesises_a_stop_on_release() {
        let (tx, mut rx) = channel();
        let flags = KeyHeldFlags::new();
        let mut t = PressTracker::new("test");
        assert!(t.pressed(ASSISTANT, &flags, &tx));
        // Backdate the press past the hold threshold.
        t.assistant_press_at = Instant::now().checked_sub(LONG_PRESS_THRESHOLD * 2);
        assert!(t.released(ASSISTANT, &flags, &tx));
        assert_eq!(rx.try_recv().unwrap(), HotkeyAction::AssistantPressed);
        assert_eq!(rx.try_recv().unwrap(), HotkeyAction::AssistantPressed);
    }

    #[test]
    fn cancel_fires_on_press_only() {
        let (tx, mut rx) = channel();
        let flags = KeyHeldFlags::new();
        let mut t = PressTracker::new("test");
        assert!(t.pressed(CANCEL, &flags, &tx));
        assert!(t.released(CANCEL, &flags, &tx));
        assert_eq!(rx.try_recv().unwrap(), HotkeyAction::CancelPressed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unknown_ids_are_ignored_without_tearing_down_the_listener() {
        let (tx, mut rx) = channel();
        let flags = KeyHeldFlags::new();
        let mut t = PressTracker::new("test");
        assert!(t.pressed("bogus", &flags, &tx));
        assert!(t.released("bogus", &flags, &tx));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_closed_channel_reports_shutdown() {
        let (tx, rx) = channel();
        drop(rx);
        let flags = KeyHeldFlags::new();
        let mut t = PressTracker::new("test");
        assert!(!t.pressed(DICTATION, &flags, &tx));
    }
}
