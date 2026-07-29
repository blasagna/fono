// SPDX-License-Identifier: GPL-3.0-only
//! KDE-native global hotkeys via `org.kde.KGlobalAccel`.
//!
//! ## Why KDE doesn't use the portal
//!
//! `xdg-desktop-portal` refuses `GlobalShortcuts.CreateSession` for a
//! caller it can't attribute to an application:
//!
//! ```text
//! org.freedesktop.portal.Error.NotAllowed: An app id is required
//! ```
//!
//! For an unsandboxed process the app id is derived from the systemd
//! unit it happens to live in. Fono started from a terminal inherits
//! *that terminal's* scope; started from `fono.service` it has no
//! `app-*` unit at all, so the app id is empty and the portal rejects
//! it outright. Worse, in the cases where the portal *does* accept us,
//! it files Fono's keys under whatever launched us — a real session had
//! Fono's `dictation` shortcut stored under Konsole's component:
//!
//! ```text
//! [org.kde.konsole]
//! dictation=Ctrl+Alt+Space,none,Toggle voice dictation
//! ```
//!
//! There is no app id we can ask for that fixes the terminal-launched
//! case, so on KDE we skip the portal and talk to the same service the
//! portal itself is built on: KGlobalAccel, served by KWin.
//!
//! ## What this buys over the GNOME shim
//!
//! [`crate::gnome_gsettings`] registers a command line (`fono toggle`)
//! and therefore only ever sees a press. KGlobalAccel emits
//! `globalShortcutPressed` **and** `globalShortcutReleased`, so this
//! backend keeps the full short-press-toggles / long-press-holds
//! behaviour of the X11 and portal listeners, along with the dynamic
//! Escape grab (cancel is registered with no keys and only given a
//! binding while a recording is in flight).
//!
//! ## Wire contract
//!
//! Pinned against KF6 `kglobalaccel` / `kglobalacceld` and verified
//! against a live Plasma 6.7 session:
//!
//! - `actionId` is `[componentUnique, actionUnique, componentFriendly,
//!   actionFriendly]` (`KGlobalAccelPrivate::makeActionId`).
//! - Key combinations are Qt key codes OR'd with Qt modifier bits, one
//!   `int` per combination. See [`crate::qt_keys`].
//! - We bind via **`setShortcut` (`asaiu`)**, whose keys argument is a
//!   plain `QList<int>`, and read back with `shortcut` (`as` → `ai`).
//!
//! ## Why not `setShortcutKeys`
//!
//! The modern `setShortcutKeys` takes `a(ai)` — an array of
//! `QKeySequence` structs. Its demarshaller, KF6's
//! `operator>>(QDBusArgument&, QKeySequence&)`, reads a fixed number of
//! ints out of that inner array; supplying fewer walks off the end of
//! the message and lands in libdbus's `_dbus_abort`:
//!
//! ```text
//! dbus[3503]: type invalid 0 not a basic type
//! #9  _dbus_marshal_read_basic          (libdbus-1)
//! #11 operator>>(QDBusArgument const&, QKeySequence&)   (libKF6GlobalAccel)
//! #12 qDBusRegisterMetaType<QSet<QKeySequence>>::…      (libKGlobalAccelD)
//! ```
//!
//! Since KGlobalAccel is hosted **inside KWin** on Plasma 6, that abort
//! takes the whole compositor down with it. Confirmed the hard way on
//! Plasma 6.7.3.
//!
//! `setShortcut` avoids the hazard structurally rather than by getting
//! the padding right: the wire type is a flat `ai`, so nothing we send
//! can reach that demarshaller — `kglobalacceld` builds the
//! `QKeySequence`s itself, in-process, from the ints. It is marked
//! deprecated upstream but is a real implementation that delegates to
//! `setShortcutKeys`, and it is still present in 6.7.3. Should a future
//! release drop it, the call returns `UnknownMethod`, registration
//! fails cleanly, and `detect.rs` falls back to the portal and then
//! X11 — the same graceful path as any other registration failure.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
// `ashpd` re-exports the `zbus` / `zvariant` it was built against, so
// going through it keeps this module on exactly one D-Bus stack — a
// direct dependency could drift to a second major version and double
// the code in the shipped binary.
use ashpd::zbus::{Connection, Message, Proxy};
use ashpd::zvariant::OwnedObjectPath;
use crossbeam_channel::unbounded;
use futures::stream::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::fsm::HotkeyAction;
use crate::listener::{HotkeyBindings, HotkeyControl, ListenerHandle};
use crate::parse::parse_hotkey;
use crate::qt_keys::to_qt_key;
use crate::shortcut::{self, PressTracker};
use crate::KeyHeldFlags;

const SERVICE: &str = "org.kde.kglobalaccel";
const MAIN_PATH: &str = "/kglobalaccel";
const MAIN_IFACE: &str = "org.kde.KGlobalAccel";
const COMPONENT_IFACE: &str = "org.kde.kglobalaccel.Component";

/// Our component's unique id. Also the section name under which the
/// bindings land in `~/.config/kglobalshortcutsrc`, and the name System
/// Settings → Shortcuts groups them under.
const COMPONENT: &str = "fono";
const COMPONENT_FRIENDLY: &str = "Fono";

/// `KGlobalAccelPrivate::SetShortcutFlag`: `SetPresent` marks the
/// shortcut active, `NoAutoloading` makes our request win over whatever
/// is stored in `kglobalshortcutsrc` — `config.toml` is the source of
/// truth for Fono's bindings.
const SET_PRESENT: u32 = 2;
const NO_AUTOLOADING: u32 = 4;
const SET_FLAGS: u32 = SET_PRESENT | NO_AUTOLOADING;

/// One `allShortcutInfos` row (`(ssssssaiai)`), in wire order.
type ShortcutInfo = (String, String, String, String, String, String, Vec<i32>, Vec<i32>);

/// A Fono shortcut that ended up registered under some *other*
/// component — the residue of an earlier portal attempt that guessed
/// the app id from whatever launched the daemon.
#[derive(Debug, Clone)]
pub struct StrayShortcut {
    /// Owning component's unique name, e.g. `org.kde.konsole`.
    pub component: String,
    /// Owning component's display name, e.g. `Konsole`.
    pub component_friendly: String,
    /// Fono's shortcut id, e.g. `dictation`.
    pub id: String,
    /// Qt key codes currently bound, if any.
    pub keys: Vec<i32>,
}

impl StrayShortcut {
    /// The command that removes this entry. Fono never runs it — the
    /// binding lives under another application's name, so taking it
    /// away is the user's call.
    #[must_use]
    pub fn removal_command(&self) -> String {
        format!(
            "busctl --user call {SERVICE} {MAIN_PATH} {MAIN_IFACE} unregister ss {:?} {:?}",
            self.component, self.id
        )
    }
}

/// Returns true on KDE Plasma (Wayland or X11). Mirrors
/// [`crate::gnome_gsettings::is_gnome_session`].
#[must_use]
pub fn is_kde_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|v| is_kde_desktop(&v))
}

/// Test seam for [`is_kde_session`]. `XDG_CURRENT_DESKTOP` is
/// colon-delimited per the XDG spec, and distributions prepend their own
/// name (`KDE`, `KDE:plasma`, and on some Fedora spins `plasma:KDE`).
#[doc(hidden)]
#[must_use]
pub fn is_kde_desktop(value: &str) -> bool {
    value.to_ascii_uppercase().split(':').any(|p| p == "KDE")
}

/// Spawn the KGlobalAccel listener. Same return shape as
/// [`crate::listener::spawn`] and [`crate::portal::spawn`], so the
/// daemon's `EnableCancel` send-site needs no per-backend branching.
///
/// Registration is preflighted synchronously: if KWin isn't serving
/// KGlobalAccel, or refuses our bindings, this returns `Err` before the
/// caller has committed to the backend, so `detect.rs` can fall through
/// to the portal and then X11 deterministically.
pub fn spawn(
    bindings: HotkeyBindings,
    tx: mpsc::UnboundedSender<HotkeyAction>,
    held_flags: KeyHeldFlags,
) -> Result<ListenerHandle> {
    let (ctrl_tx, ctrl_rx) = unbounded::<HotkeyControl>();
    let (preflight_tx, preflight_rx) = std::sync::mpsc::channel::<Result<()>>();

    let thread = std::thread::Builder::new()
        .name("fono-hotkey-kglobalaccel".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .thread_name("fono-kglobalaccel-rt")
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = preflight_tx.send(Err(anyhow::anyhow!("build runtime: {e:#}")));
                    return;
                }
            };
            rt.block_on(async move {
                let conn = match Connection::session().await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ =
                            preflight_tx.send(Err(anyhow::anyhow!("session bus unavailable: {e}")));
                        return;
                    }
                };
                let proxy = match main_proxy(&conn).await {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = preflight_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = register_all(&proxy, &bindings).await {
                    let _ = preflight_tx.send(Err(e));
                    return;
                }
                let _ = preflight_tx.send(Ok(()));

                // Non-fatal diagnostics, once the keys are ours.
                match find_strays(&conn, &proxy).await {
                    Ok(strays) => warn_about_strays(&strays),
                    Err(e) => debug!("kglobalaccel: stray-shortcut scan skipped: {e:#}"),
                }

                if let Err(e) = run(&conn, &proxy, &bindings, &tx, ctrl_rx, &held_flags).await {
                    warn!("kglobalaccel listener exited: {e:#}");
                }
                // Best-effort: don't leave Escape grabbed session-wide
                // if we're going down mid-recording.
                let _ = apply_keys(&proxy, shortcut::CANCEL, shortcut::CANCEL_DESC, &[]).await;
            });
        })
        .context("spawn kglobalaccel hotkey thread")?;

    // A healthy KWin answers in a few milliseconds; 2 s is generous and
    // matches the portal preflight budget.
    match preflight_rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(ListenerHandle { thread, control: ctrl_tx }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!("kglobalaccel preflight timed out after 2 s")),
    }
}

async fn main_proxy(conn: &Connection) -> Result<Proxy<'static>> {
    Proxy::new_owned(
        conn.clone(),
        SERVICE.to_string(),
        MAIN_PATH.to_string(),
        MAIN_IFACE.to_string(),
    )
    .await
    .context("KGlobalAccel is not available on the session bus")
}

/// `[componentUnique, actionUnique, componentFriendly, actionFriendly]`
/// per `KGlobalAccelPrivate::makeActionId`.
fn action_id(id: &str, description: &str) -> Vec<String> {
    vec![
        COMPONENT.to_string(),
        id.to_string(),
        COMPONENT_FRIENDLY.to_string(),
        description.to_string(),
    ]
}

/// Register `id` and set its key list, returning the keys KWin actually
/// granted. An empty `keys` clears the binding without unregistering
/// the action, which is how the cancel key stays dormant.
///
/// Uses the flat-`ai` `setShortcut` rather than `setShortcutKeys` — see
/// the module docs; the `a(ai)` form can abort KWin.
async fn apply_keys(
    proxy: &Proxy<'_>,
    id: &str,
    description: &str,
    keys: &[i32],
) -> Result<Vec<i32>> {
    let action = action_id(id, description);
    proxy
        .call::<_, _, ()>("doRegister", &(&action,))
        .await
        .with_context(|| format!("doRegister({id})"))?;
    let granted: Vec<i32> = proxy
        .call("setShortcut", &(&action, keys, SET_FLAGS))
        .await
        .with_context(|| format!("setShortcut({id})"))?;
    Ok(granted)
}

/// Bind one configured hotkey string, warning (rather than failing) on
/// anything we can't honour — a bad assistant binding must not stop the
/// daemon, exactly as in [`crate::listener::spawn`].
async fn bind_role(proxy: &Proxy<'_>, id: &str, description: &str, binding: &str) -> Result<bool> {
    let binding = binding.trim();
    if binding.is_empty() {
        // Still register the action so it exists (and, for cancel, so
        // it can be given keys later) but leave it unbound.
        apply_keys(proxy, id, description, &[]).await?;
        return Ok(false);
    }
    let parsed = match parse_hotkey(binding) {
        Ok(p) => p,
        Err(e) => {
            warn!("kglobalaccel: could not parse {id} binding {binding:?}: {e:#}; skipping");
            return Ok(false);
        }
    };
    let Some(key) = to_qt_key(&parsed) else {
        warn!(
            "kglobalaccel: {binding:?} has no Qt equivalent, so the {id} key cannot be \
             registered with Plasma; pick a different binding or use `fono {id}` from the CLI"
        );
        return Ok(false);
    };
    let granted = apply_keys(proxy, id, description, &[key]).await?;
    if granted.contains(&key) {
        return Ok(true);
    }
    // KWin hands back the *current* keys when the combination is
    // already spoken for, so this is the conflict signal.
    warn!(
        "kglobalaccel: Plasma refused {binding:?} for the {id} key (it is already assigned to \
         another application). Change the binding, or free it in System Settings → Shortcuts. \
         Granted instead: {granted:?}"
    );
    Ok(false)
}

async fn register_all(proxy: &Proxy<'_>, bindings: &HotkeyBindings) -> Result<()> {
    let dictation =
        bind_role(proxy, shortcut::DICTATION, shortcut::DICTATION_DESC, &bindings.dictation)
            .await?;
    let assistant =
        bind_role(proxy, shortcut::ASSISTANT, shortcut::ASSISTANT_DESC, &bindings.assistant)
            .await?;
    // Cancel is registered but deliberately left unbound: we only grab
    // Escape while a recording / assistant turn is in flight.
    apply_keys(proxy, shortcut::CANCEL, shortcut::CANCEL_DESC, &[]).await?;

    if !dictation && !assistant {
        anyhow::bail!(
            "KGlobalAccel accepted no shortcuts (dictation={:?}, assistant={:?})",
            bindings.dictation,
            bindings.assistant
        );
    }
    info!(
        "KDE: registered KGlobalAccel shortcuts under \"{COMPONENT_FRIENDLY}\" \
         (dictation={}, assistant={}); cancel key {:?} bound only while recording",
        if dictation { bindings.dictation.trim() } else { "unavailable" },
        if assistant { bindings.assistant.trim() } else { "unavailable" },
        bindings.cancel.trim(),
    );
    Ok(())
}

/// Subscribe to our component's press/release signals and pump them
/// into the FSM until the daemon shuts down.
async fn run(
    conn: &Connection,
    proxy: &Proxy<'_>,
    bindings: &HotkeyBindings,
    tx: &mpsc::UnboundedSender<HotkeyAction>,
    ctrl_rx: crossbeam_channel::Receiver<HotkeyControl>,
    held_flags: &KeyHeldFlags,
) -> Result<()> {
    let path: OwnedObjectPath = proxy
        .call("getComponent", &(COMPONENT,))
        .await
        .context("kglobalaccel getComponent(fono)")?;
    let component = Proxy::new_owned(
        conn.clone(),
        SERVICE.to_string(),
        path.as_str().to_string(),
        COMPONENT_IFACE.to_string(),
    )
    .await
    .context("open the fono KGlobalAccel component")?;

    let mut pressed = component
        .receive_signal("globalShortcutPressed")
        .await
        .context("subscribe to globalShortcutPressed")?;
    let mut released = component
        .receive_signal("globalShortcutReleased")
        .await
        .context("subscribe to globalShortcutReleased")?;

    // Bridge crossbeam ctrl_rx into the async task without blocking the
    // runtime, mirroring the portal listener.
    let (ctrl_async_tx, mut ctrl_async_rx) = mpsc::unbounded_channel::<HotkeyControl>();
    std::thread::Builder::new()
        .name("fono-kglobalaccel-ctrl-bridge".into())
        .spawn(move || {
            while let Ok(msg) = ctrl_rx.recv() {
                if ctrl_async_tx.send(msg).is_err() {
                    break;
                }
            }
        })
        .ok();

    let cancel_key = cancel_qt_key(&bindings.cancel);
    let cancel_bound = AtomicBool::new(false);
    let mut presses = PressTracker::new("kglobalaccel");
    info!("kglobalaccel listener armed; waiting for shortcut activations");

    loop {
        tokio::select! {
            msg = pressed.next() => {
                let Some(id) = signal_shortcut_id(msg.as_ref(), "pressed") else { break };
                if !presses.pressed(&id, held_flags, tx) {
                    break;
                }
            }
            msg = released.next() => {
                let Some(id) = signal_shortcut_id(msg.as_ref(), "released") else { break };
                if !presses.released(&id, held_flags, tx) {
                    break;
                }
            }
            ctrl = ctrl_async_rx.recv() => {
                let Some(ctrl) = ctrl else {
                    debug!("kglobalaccel: ctrl channel closed; listener shutting down");
                    break;
                };
                set_cancel_grab(proxy, ctrl, cancel_key, &cancel_bound).await;
            }
        }
    }
    Ok(())
}

/// Grab / release Escape in response to the orchestrator, keeping the
/// same contract as the X11 and portal backends: the cancel key is only
/// held while a recording or assistant turn is actually in flight.
async fn set_cancel_grab(
    proxy: &Proxy<'_>,
    ctrl: HotkeyControl,
    cancel_key: Option<i32>,
    bound: &AtomicBool,
) {
    let Some(key) = cancel_key else {
        debug!("kglobalaccel: cancel control ignored (no usable cancel binding)");
        return;
    };
    let want = matches!(ctrl, HotkeyControl::EnableCancel);
    if want == bound.load(Ordering::SeqCst) {
        return;
    }
    let keys: &[i32] = if want { &[key] } else { &[] };
    match apply_keys(proxy, shortcut::CANCEL, shortcut::CANCEL_DESC, keys).await {
        Ok(granted) if want && !granted.contains(&key) => {
            warn!(
                "kglobalaccel: Plasma refused the cancel key (already assigned elsewhere). \
                 Use `fono cancel` to abort instead."
            );
        }
        Ok(_) => {
            bound.store(want, Ordering::SeqCst);
            debug!("kglobalaccel: cancel binding {}", if want { "added" } else { "removed" });
        }
        Err(e) => {
            warn!(
                "kglobalaccel: failed to {} the cancel key: {e:#}. \
                 Use `fono cancel` to abort instead.",
                if want { "grab" } else { "release" }
            );
        }
    }
}

fn cancel_qt_key(binding: &str) -> Option<i32> {
    let binding = binding.trim();
    if binding.is_empty() {
        return None;
    }
    match parse_hotkey(binding) {
        Ok(p) => to_qt_key(&p).or_else(|| {
            warn!("kglobalaccel: cancel binding {binding:?} has no Qt equivalent; Esc won't grab");
            None
        }),
        Err(e) => {
            warn!("kglobalaccel: could not parse cancel binding {binding:?}: {e:#}");
            None
        }
    }
}

/// Pull the shortcut id out of a `globalShortcut{Pressed,Released}`
/// signal (`componentUnique, shortcutUnique, timestamp`). `None` means
/// the stream ended and the listener should shut down.
fn signal_shortcut_id(msg: Option<&Message>, kind: &'static str) -> Option<String> {
    let Some(msg) = msg else {
        debug!("kglobalaccel: {kind} stream closed; listener shutting down");
        return None;
    };
    match msg.body().deserialize::<(String, String, i64)>() {
        Ok((_component, id, _timestamp)) => Some(id),
        Err(e) => {
            warn!("kglobalaccel: malformed {kind} signal: {e}");
            // A single unparseable signal is not a reason to tear the
            // listener down; hand back an id nothing matches.
            Some(String::new())
        }
    }
}

// ---------------------------------------------------------------------
// Stray-registration diagnostics
// ---------------------------------------------------------------------

/// Find Fono shortcuts registered under some component other than
/// `fono`, and still holding a key. Both the shortcut id *and* its
/// description must match one of ours — a deliberately narrow
/// signature, so a genuine Konsole binding that happens to be called
/// `dictation` is never implicated.
///
/// Blocking wrapper for callers outside an async context (`fono
/// doctor`). Runs on a detached thread, so it is safe to call from
/// inside a tokio runtime and cannot wedge the caller if the session bus
/// stops answering. Returns an empty list on any failure or timeout:
/// this is a diagnostic, never a reason to fail.
#[must_use]
pub fn find_stray_shortcuts() -> Vec<StrayShortcut> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let found = (|| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
            rt.block_on(async {
                let conn = Connection::session().await.ok()?;
                let proxy = main_proxy(&conn).await.ok()?;
                find_strays(&conn, &proxy).await.ok()
            })
        })();
        let _ = tx.send(found.unwrap_or_default());
    });
    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or_default()
}

async fn find_strays(conn: &Connection, proxy: &Proxy<'_>) -> Result<Vec<StrayShortcut>> {
    let components: Vec<Vec<String>> =
        proxy.call("allMainComponents", &()).await.context("allMainComponents")?;
    let mut strays = Vec::new();
    for c in components {
        let Some(unique) = c.first() else { continue };
        if unique.is_empty() || unique == COMPONENT {
            continue;
        }
        let friendly = c.get(1).filter(|s| !s.is_empty()).unwrap_or(unique).clone();
        let Ok(path) =
            proxy.call::<_, _, OwnedObjectPath>("getComponent", &(unique.as_str(),)).await
        else {
            continue;
        };
        let Ok(component) = Proxy::new_owned(
            conn.clone(),
            SERVICE.to_string(),
            path.as_str().to_string(),
            COMPONENT_IFACE.to_string(),
        )
        .await
        else {
            continue;
        };
        let Ok(infos) = component.call::<_, _, Vec<ShortcutInfo>>("allShortcutInfos", &()).await
        else {
            continue;
        };
        for (id, description, .., keys, _default_keys) in infos {
            // An entry with no keys bound can't intercept anything, so
            // it isn't worth bothering the user about.
            if keys.is_empty() {
                continue;
            }
            if shortcut::ALL.iter().any(|(oid, odesc)| *oid == id && *odesc == description) {
                strays.push(StrayShortcut {
                    component: unique.clone(),
                    component_friendly: friendly.clone(),
                    id,
                    keys,
                });
            }
        }
    }
    Ok(strays)
}

fn warn_about_strays(strays: &[StrayShortcut]) {
    for s in strays {
        warn!(
            "kglobalaccel: a leftover Fono {:?} shortcut is registered under {} ({}), from an \
             earlier attempt to bind keys through the desktop portal. Plasma may swallow that \
             key before Fono sees it. Remove it with:\n  {}",
            s.id,
            s.component_friendly,
            s.component,
            s.removal_command()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_id_follows_kglobalaccel_order() {
        let id = action_id(shortcut::DICTATION, shortcut::DICTATION_DESC);
        assert_eq!(id, vec!["fono", "dictation", "Fono", "Toggle voice dictation"]);
    }

    #[test]
    fn set_flags_are_set_present_plus_no_autoloading() {
        assert_eq!(SET_FLAGS, 6);
    }

    #[test]
    fn removal_command_names_the_offending_component() {
        let s = StrayShortcut {
            component: "org.kde.konsole".into(),
            component_friendly: "Konsole".into(),
            id: "dictation".into(),
            keys: vec![201_326_624],
        };
        let cmd = s.removal_command();
        assert!(cmd.contains("unregister ss \"org.kde.konsole\" \"dictation\""), "{cmd}");
    }

    #[test]
    fn kde_is_recognised_in_every_shape_distros_ship() {
        assert!(is_kde_desktop("KDE"));
        assert!(is_kde_desktop("KDE:plasma"));
        assert!(is_kde_desktop("plasma:KDE"));
        assert!(is_kde_desktop("kde"));
        assert!(!is_kde_desktop("GNOME"));
        assert!(!is_kde_desktop("ubuntu:GNOME"));
        assert!(!is_kde_desktop(""));
        // Substring matches must not count — only whole components.
        assert!(!is_kde_desktop("KDEISH"));
    }

    #[test]
    fn cancel_binding_is_optional_and_never_panics() {
        assert_eq!(cancel_qt_key(""), None);
        assert_eq!(cancel_qt_key("   "), None);
        assert_eq!(cancel_qt_key("not-a-key"), None);
        assert_eq!(cancel_qt_key("Escape"), Some(0x0100_0000));
    }
}
