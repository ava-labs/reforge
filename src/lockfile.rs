// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.
//
// Portions of this file are derived from Foundry
// (https://github.com/foundry-rs/foundry), file
// `crates/forge/src/lockfile.rs`.
// Copyright (c) 2021 Georgios Konstantopoulos
// Licensed under the MIT License.

use std::path::{Path, PathBuf};

pub use forge::DepIdentifier;
use forge::revm::primitives::HashMap;
use foundry_cli::utils::Git;
use foundry_common::{fs::read_to_string, sh_warn};
use foundry_config::Config;
use serde::{Deserialize, Serialize};
use soldeer_core::{install::check_dependency_integrity, lock::read_lockfile};
use tracing::trace;

pub const FOUNDRY_LOCK: &str = "foundry.lock";

/// A lockfile handler that keeps track of the dependencies and their current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile<'a> {
    /// A map of the dependencies keyed by relative path to the submodule dir.
    #[serde(flatten)]
    deps: forge::DepMap,
    /// This is optional to handle no-git scenarios.
    #[serde(skip)]
    git: Option<&'a Git<'a>>,
    /// Absolute path to the lockfile.
    #[serde(skip)]
    lockfile_path: PathBuf,
}

impl<'a> Lockfile<'a> {
    /// Create a new [`forge::Lockfile`] instance.
    ///
    /// `project_root` is the absolute path to the project root.
    ///
    /// You will need to call [`forge::Lockfile::read`] or [`forge::Lockfile::sync`] to load the
    /// lockfile.
    pub fn new(project_root: &Path) -> Self {
        Self {
            deps: HashMap::default(),
            git: None,
            lockfile_path: project_root.join(forge::FOUNDRY_LOCK),
        }
    }

    /// Set the git instance to be used for submodule operations.
    pub fn with_git(mut self, git: &'a Git<'_>) -> Self {
        self.git = Some(git);
        self
    }

    /// Loads the lockfile from the project root.
    ///
    /// Throws an error if the lockfile does not exist.
    pub fn read(&mut self) -> eyre::Result<()> {
        if !self.lockfile_path.exists() {
            return Err(eyre::eyre!("Lockfile not found at {}", self.lockfile_path.display()));
        }

        let lockfile_str = read_to_string(&self.lockfile_path)?;

        self.deps = serde_json::from_str(&lockfile_str)?;

        trace!(lockfile = ?self.deps, "loaded lockfile");

        Ok(())
    }
}

/// Check soldeer.lock file consistency using soldeer_core APIs
pub(crate) async fn check_soldeer_lock_consistency(config: &Config) {
    let soldeer_lock_path = config.root.join("soldeer.lock");
    if !soldeer_lock_path.exists() {
        return;
    }

    // Note: read_lockfile returns Ok with empty entries for malformed files
    let Ok(lockfile) = read_lockfile(&soldeer_lock_path) else {
        return;
    };

    let deps_dir = config.root.join("dependencies");
    for entry in &lockfile.entries {
        let dep_name = entry.name();

        // Use soldeer_core's integrity check
        match check_dependency_integrity(entry, &deps_dir).await {
            Ok(status) => {
                use soldeer_core::install::DependencyStatus;
                // Check if status indicates a problem
                if matches!(status, DependencyStatus::Missing | DependencyStatus::FailedIntegrity) {
                    sh_warn!("Dependency '{}' integrity check failed: {:?}", dep_name, status).ok();
                }
            }
            Err(e) => {
                sh_warn!("Dependency '{}' integrity check error: {}", dep_name, e).ok();
            }
        }
    }
}

/// Check foundry.lock file consistency with git submodules
pub(crate) fn check_foundry_lock_consistency(config: &Config) {
    use crate::lockfile::{DepIdentifier, FOUNDRY_LOCK, Lockfile};

    let foundry_lock_path = config.root.join(FOUNDRY_LOCK);
    if !foundry_lock_path.exists() {
        return;
    }

    let git = Git::new(&config.root);

    let mut lockfile = Lockfile::new(&config.root).with_git(&git);
    if let Err(e) = lockfile.read() {
        if !e.to_string().contains("Lockfile not found") {
            sh_warn!("Failed to parse foundry.lock: {}", e).ok();
        }
        return;
    }

    for (dep_path, dep_identifier) in lockfile.deps.iter() {
        let full_path = config.root.join(dep_path);

        if !full_path.exists() {
            sh_warn!("Dependency '{}' not found at expected path", dep_path.display()).ok();
            continue;
        }

        let actual_rev = match git.get_rev("HEAD", &full_path) {
            Ok(rev) => rev,
            Err(_) => {
                sh_warn!("Failed to get git revision for dependency '{}'", dep_path.display()).ok();
                continue;
            }
        };

        // Compare with the expected revision from lockfile
        let expected_rev = match dep_identifier {
            DepIdentifier::Branch { rev, .. }
            | DepIdentifier::Tag { rev, .. }
            | DepIdentifier::Rev { rev, .. } => rev.clone(),
        };

        if actual_rev != expected_rev {
            sh_warn!(
                "Dependency '{}' revision mismatch: expected '{}', found '{}'",
                dep_path.display(),
                expected_rev,
                actual_rev
            )
            .ok();
        }
    }
}
