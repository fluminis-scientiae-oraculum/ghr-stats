//! Every removal in `ops::uninstall`. Never strand a runner: shared hook scripts
//! go only after the hooks are reverted and no runner's `.env` points at them.

use std::path::Path;

use super::hooks::{self as hook_revert, RunnerHookPlan};
use crate::ops::systemd;
use crate::shared::hooks::{env, install};
use crate::shared::privileged;

use super::Plan;
use super::plan::BinaryAction;

impl Plan {
    pub(super) fn apply(&self) {
        if self.domains.hooks {
            println!("Hooks:");
            if !privileged::is_root() {
                println!(
                    "  ⚠ skipped — reverting hooks needs root; re-run `{}`",
                    privileged::sudo_hint("uninstall hooks")
                );
            } else {
                for rp in &self.runners {
                    println!("{}", hook_revert::apply_runner(rp));
                }
                gc_shared_scripts(&self.our_dir, &self.runners);
            }
        }

        if let Some(unit) = &self.service_unit {
            println!("Service:");
            match systemd::uninstall(self.scope) {
                Ok(()) => {}
                Err(e) => println!("  ✗ {} — {e}", unit.display()),
            }
        }

        if let Some(BinaryAction::Remove(p)) = &self.binary {
            println!("Binary:");
            match std::fs::remove_file(p) {
                Ok(()) => println!("  ✓ removed {}", p.display()),
                Err(e) => println!("  ✗ {} — {e}", p.display()),
            }
        }

        if !self.config.is_empty() {
            println!("Config:");
            for c in &self.config {
                remove_reporting(&c.path);
                let _ = c.path.parent().map(std::fs::remove_dir); // only if empty
            }
        }

        if !self.data.is_empty() {
            println!("Data:");
            for p in &self.data {
                remove_reporting(p);
            }
            let _ = std::fs::remove_dir(self.scope.data_dir()); // only if empty
        }

        println!("\nDone.");
    }
}

fn gc_shared_scripts(our_dir: &Path, plans: &[RunnerHookPlan]) {
    let still_referenced = plans.iter().any(|rp| env_may_point_into(&rp.dir, our_dir));
    if still_referenced {
        println!(
            "  · kept {} — still referenced by a runner not managed by ghr-stats",
            our_dir.display()
        );
        return;
    }
    let _ = std::fs::remove_file(our_dir.join("job-started.sh"));
    let _ = std::fs::remove_file(our_dir.join("job-completed.sh"));
    if std::fs::remove_dir(our_dir).is_ok() {
        println!("  ✓ removed shared hook scripts {}", our_dir.display());
    }
}

fn env_may_point_into(runner_dir: &Path, our_dir: &Path) -> bool {
    let Ok(env) = env::read(runner_dir) else {
        return true;
    };
    let (s, c) = install::current_hook_paths(&env.text);
    [s, c]
        .into_iter()
        .flatten()
        .any(|v| Path::new(&v).starts_with(our_dir))
}

fn remove_reporting(p: &Path) {
    match std::fs::remove_file(p) {
        Ok(()) => println!("  ✓ removed {}", p.display()),
        Err(e) => println!("  ✗ {} — {e}", p.display()),
    }
}
