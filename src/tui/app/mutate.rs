//! Modal overlays and the config writes they produce: via the root collector over
//! IPC when one is reachable, else a direct file write, which needs root.

use std::path::PathBuf;

use ratatui::crossterm::event::KeyEvent;

use crate::shared::collectors::runners;
use crate::shared::config::Config;
use crate::tui::overlay::wizard::{self, WizardMode};
use crate::tui::source::MutateOutcome;

use super::{App, Overlay};

/// The collector resolves group membership fresh by uid, so `usermod -aG` needs no re-login.
const NOT_AUTHORIZED: &str = "not authorized — add yourself to the `ghr-stats` group \
    (`sudo usermod -aG ghr-stats $USER`) or run `sudo ghr-stats`";

impl App {
    pub(crate) fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    pub(crate) fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    pub(crate) fn open_wizard(&mut self) {
        self.overlay = Some(Overlay::Wizard(WizardMode::new()));
    }

    pub(crate) fn open_help(&mut self) {
        self.overlay = Some(Overlay::Help);
    }

    pub(crate) fn open_info(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.overlay = Some(Overlay::Info {
            title: title.into(),
            body: body.into(),
        });
    }

    pub(crate) fn overlay_key(&mut self, key: KeyEvent) {
        match self.overlay.take() {
            Some(Overlay::Wizard(mode)) => {
                let ctx = self.wizard_ctx();
                let target = self.config_target();
                let source = &mut self.source;
                // One closure for both ops: two could not both borrow `source` mutably.
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
                        // No-op for a non-root TUI: it cannot re-read root-owned /etc.
                        if changed {
                            self.reload_cfg();
                        }
                    }
                }
            }
            Some(Overlay::Help | Overlay::Info { .. }) | None => {}
        }
    }

    fn wizard_ctx(&self) -> wizard::WizardCtx {
        wizard::WizardCtx {
            local: self
                .runners
                .iter()
                .map(|r| (r.scope.clone(), r.agent_id))
                .collect(),
        }
    }

    /// The `--config` override, else the system config under `/etc`.
    pub(crate) fn config_target(&self) -> PathBuf {
        crate::shared::paths::config_write_target(self.config_path.as_deref())
    }

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
                // Mirrored, not reloaded: a non-root TUI cannot re-read root-owned /etc.
                self.cfg.metrics.pull.enabled = enabled;
                let state = if enabled { "enabled" } else { "disabled" };
                self.status = Some(format!(
                    "metrics pull {state} — restart the service to apply"
                ));
            }
            Err(e) => self.status = Some(format!("✗ metrics toggle failed: {e}")),
        }
    }

    fn reload_cfg(&mut self) {
        if let Ok(mut cfg) = Config::load(self.config_path.as_deref()) {
            cfg.runner_roots = runners::effective_roots(&cfg.runner_roots);
            self.cfg = cfg;
        }
        self.refresh();
    }
}
