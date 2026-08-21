// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.
//
// Portions of this file are derived from Foundry
// (https://github.com/foundry-rs/foundry), file
// `crates/forge/src/cmd/test/filter.rs`.
// Copyright (c) 2021 Georgios Konstantopoulos
// Licensed under the MIT License.

use std::{fmt, path::Path};

use forge::cmd::test::FilterArgs;
use foundry_compilers::FileFilter;
use foundry_config::Config;

/// A filter that combines all command line arguments and the paths of the current project.
/// This mirrors forge's private `ProjectPathsAwareFilter` so we can name the type.
#[derive(Clone, Debug)]
pub struct ProjectPathsAwareFilter {
    args_filter: FilterArgs,
    paths: foundry_compilers::ProjectPathsConfig,
}

impl ProjectPathsAwareFilter {
    /// Returns true if the filter is empty (no filtering criteria set).
    pub fn is_empty(&self) -> bool {
        self.args_filter.is_empty()
    }

    /// Returns the CLI filter arguments.
    pub fn args(&self) -> &FilterArgs {
        &self.args_filter
    }
}

impl FileFilter for ProjectPathsAwareFilter {
    fn is_match(&self, mut file: &Path) -> bool {
        file = file.strip_prefix(&self.paths.root).unwrap_or(file);
        self.args_filter.is_match(file)
    }
}

impl forge::TestFilter for ProjectPathsAwareFilter {
    fn matches_test(&self, test_signature: &str) -> bool {
        self.args_filter.matches_test(test_signature)
    }

    fn matches_contract(&self, contract_name: &str) -> bool {
        self.args_filter.matches_contract(contract_name)
    }

    fn matches_path(&self, mut path: &Path) -> bool {
        path = path.strip_prefix(&self.paths.root).unwrap_or(path);
        self.args_filter.matches_path(path) && !self.paths.has_library_ancestor(path)
    }
}

impl fmt::Display for ProjectPathsAwareFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.args_filter.fmt(f)
    }
}

/// Builds a `ProjectPathsAwareFilter` from `FilterArgs` and `Config`, mirroring
/// `FilterArgs::merge_with_config` but returning our own named type.
pub fn merge_filter_with_config(
    mut filter: FilterArgs,
    config: &Config,
) -> ProjectPathsAwareFilter {
    if filter.test_pattern.is_none() {
        filter.test_pattern = config.test_pattern.clone().map(Into::into);
    }
    if filter.test_pattern_inverse.is_none() {
        filter.test_pattern_inverse = config.test_pattern_inverse.clone().map(Into::into);
    }
    if filter.contract_pattern.is_none() {
        filter.contract_pattern = config.contract_pattern.clone().map(Into::into);
    }
    if filter.contract_pattern_inverse.is_none() {
        filter.contract_pattern_inverse = config.contract_pattern_inverse.clone().map(Into::into);
    }
    if filter.path_pattern.is_none() {
        filter.path_pattern = config.path_pattern.clone().map(Into::into);
    }
    if filter.path_pattern_inverse.is_none() {
        filter.path_pattern_inverse = config.path_pattern_inverse.clone().map(Into::into);
    }
    if filter.coverage_pattern_inverse.is_none() {
        filter.coverage_pattern_inverse = config.coverage_pattern_inverse.clone().map(Into::into);
    }
    ProjectPathsAwareFilter { args_filter: filter, paths: config.project_paths() }
}
