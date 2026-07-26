//! Pushing [`App`]'s changes back out to the world.
//!
//! The modal overlays (config wizard, help sheet, info block) and the two config
//! writes they produce. Every write takes the same two-step route: through the
//! root collector over IPC when Persistent and authorized, else a direct write to
//! the config file — which succeeds as root and otherwise fails with a "re-run
//! with sudo" message. That fallback is why a non-root dashboard can still edit a
//! root-owned `/etc` config, and why [`MutateOutcome::Denied`] is a distinct
//! outcome from [`MutateOutcome::Unreachable`]: one means try sudo, the other
//! means there is no collector to ask.
//!
//! Distinct from the suspend/resume path in `tui::input` used for privileged
//! shell-outs (Restart/Recycle/hook-install), which tears the terminal down.

use std::path::PathBuf;

use ratatui::crossterm::event::KeyEvent;

use crate::shared::collectors::runners;
use crate::shared::config::Config;
use crate::tui::history::MutateOutcome;
use crate::tui::widgets::wizard::{self, WizardMode};

use super::{App, Overlay};

/// Shown when the collector refuses a config mutation — the TUI's peer is neither
/// root nor in the `ghr-stats` group. The collector resolves group membership
/// fresh from the group DB by uid, so `usermod -aG` takes effect immediately (no
/// re-login needed); running the TUI as root also works.
const NOT_AUTHORIZED: &str = "not authorized — add yourself to the `ghr-stats` group \
    (`sudo usermod -aG ghr-stats $USER`) or run `sudo ghr-stats`";

impl App {
    // ---- modal overlays (config wizard / help / info) ----

    /// Whether a modal overlay is open (⇒ the loop routes every key to it).
    pub(crate) fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    /// The open overlay to render, if any.
    pub(crate) fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// Open the config wizard at its action menu (from the Config tab `[a]`).
    pub(crate) fn open_wizard(&mut self) {
        self.overlay = Some(Overlay::Wizard(WizardMode::new()));
    }

    /// Open the context-sensitive help sheet (`[?]`).
    pub(crate) fn open_help(&mut self) {
        self.overlay = Some(Overlay::Help);
    }

    /// Open a read-only info block (e.g. privilege guidance) — dismissed by any key.
    pub(crate) fn open_info(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.overlay = Some(Overlay::Info {
            title: title.into(),
            body: body.into(),
        });
    }

    /// Route one key to the open overlay. The wizard advances/closes via its
    /// typestate (and delegates text editing to its input widget, hence the full
    /// `KeyEvent`); help/info are dismissed by any key. A wizard close that
    /// changed the config reloads it so the Config tab reflects the new token.
    pub(crate) fn overlay_key(&mut self, key: KeyEvent) {
        match self.overlay.take() {
            Some(Overlay::Wizard(mode)) => {
                let ctx = self.wizard_ctx();
                let target = self.config_target();
                // Persist sink for the validated token: route through the root
                // collector (IPC) when Persistent+authorized, else fall back to a
                // direct config write (succeeds as root, else the "re-run with
                // sudo" error). Borrows only `self.source`, so it is released
                // before the `Close` arm reloads config.
                let source = &mut self.source;
                // ONE sink for both add/replace and remove — a single `&mut source`
                // borrow (two closures could not both hold it). Each op routes
                // through the root collector (IPC) when authorized, else a direct
                // config write (root ⇒ ok, else a "re-run with sudo" error).
                let apply = |op: wizard::TokenOp| -> Result<(), String> {
                    match op {
                        wizard::TokenOp::Set { org, token } => match source
                            .add_org_token(org, token)
                        {
                            MutateOutcome::Mutated => Ok(()),
                            MutateOutcome::Denied => Err(NOT_AUTHORIZED.to_string()),
                            MutateOutcome::Unreachable => {
                                crate::shared::config::persist::set_org_token(&target, org, token)
                                    .map_err(|e| e.to_string())
                            }
                        },
                        wizard::TokenOp::Remove { org } => match source.remove_org_token(org) {
                            MutateOutcome::Mutated => Ok(()),
                            MutateOutcome::Denied => Err(NOT_AUTHORIZED.to_string()),
                            MutateOutcome::Unreachable => {
                                crate::shared::config::persist::remove_org_token(&target, org)
                                    .map_err(|e| e.to_string())
                            }
                        },
                    }
                };
                match mode.on_key(key, &ctx, apply) {
                    wizard::Step::Stay(next) => self.overlay = Some(Overlay::Wizard(next)),
                    wizard::Step::Close(changed) => {
                        // Re-read to reflect the new token in the Config tab. As a
                        // non-root TUI that saved via IPC we can't read root-owned
                        // /etc — the reload is then a graceful no-op and the
                        // wizard's own "✓ saved" screen is the confirmation.
                        if changed {
                            self.reload_cfg();
                        }
                    }
                }
            }
            // Help / Info: any key dismisses (already removed by `take`).
            Some(Overlay::Help | Overlay::Info { .. }) | None => {}
        }
    }

    /// What the wizard needs to act: the locally-discovered runner ids (for the
    /// agentId match). The persist mechanism is injected separately (see
    /// [`Self::overlay_key`]), so `WizardCtx` no longer carries a write target.
    fn wizard_ctx(&self) -> wizard::WizardCtx {
        wizard::WizardCtx {
            local_ids: self.runners.iter().map(|r| r.agent_id).collect(),
        }
    }

    /// The config file the TUI writes — the `--config` override, else the
    /// canonical system config at `/etc` (matches `ghr-stats config`). Writing it
    /// needs root, so config edits work when the dashboard is run as `sudo
    /// ghr-stats`; a non-root edit fails with a clear "re-run with sudo" error.
    pub(crate) fn config_target(&self) -> PathBuf {
        crate::shared::paths::config_write_target(self.config_path.as_deref())
    }

    /// Toggle the Prometheus `/metrics` pull endpoint (Config `[m]`), persisting
    /// to `/etc`. Routes through the root collector (IPC) when Persistent+
    /// authorized, else a direct config write. Takes effect on the next `serve`
    /// start.
    pub(crate) fn toggle_metrics(&mut self) {
        let enabled = !self.cfg.metrics.pull.enabled;
        let addr = self.cfg.metrics.pull.addr.clone();
        let result = match self.source.set_metrics_pull(enabled, &addr) {
            MutateOutcome::Mutated => Ok(()),
            MutateOutcome::Denied => Err(NOT_AUTHORIZED.to_string()),
            MutateOutcome::Unreachable => crate::shared::config::persist::set_metrics_pull(
                &self.config_target(),
                enabled,
                &addr,
            )
            .map_err(|e| e.to_string()),
        };
        match result {
            Ok(()) => {
                // Mirror the one field locally so the Config tab flips immediately.
                // A non-root TUI that saved via IPC cannot re-read root-owned /etc,
                // so `reload_cfg` would be a no-op here — the collector already
                // persisted the change; this keeps the on-screen state truthful.
                self.cfg.metrics.pull.enabled = enabled;
                let state = if enabled { "enabled" } else { "disabled" };
                self.status = Some(format!(
                    "metrics pull {state} — restart the service to apply"
                ));
            }
            Err(e) => self.status = Some(format!("✗ metrics toggle failed: {e}")),
        }
    }

    /// Reload config from disk after a write, then refresh the views.
    fn reload_cfg(&mut self) {
        if let Ok(mut cfg) = Config::load(self.config_path.as_deref()) {
            cfg.runner_roots = runners::effective_roots(&cfg.runner_roots);
            self.cfg = cfg;
        }
        self.refresh();
    }
}
