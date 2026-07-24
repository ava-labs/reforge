// Copyright (C) 2026, Ava Labs, Inc. All rights reserved.
// See the file LICENSE for licensing terms.

//! Test utilities for verifying macro expansions against expected Solidity output.

use std::{collections::HashSet, ops::ControlFlow, path::Path};

use foundry_compilers::{
    ProjectPathsConfig, SourceParser,
    artifacts::{SolcLanguage, Source, Sources},
};
use solar::{parse::interface::Session, sema::Compiler};

use crate::{Macro, PreprocessingData};

/// Loads all `.sol` files under `dir` into a `Sources` map keyed by absolute path.
pub(crate) fn load_sol_sources(dir: &Path) -> eyre::Result<Sources> {
    let mut sources = Sources::new();
    load_sol_sources_recursive(dir, &mut sources)?;
    Ok(sources)
}

fn load_sol_sources_recursive(dir: &Path, sources: &mut Sources) -> eyre::Result<()> {
    if dir.is_file() {
        if dir.extension().is_some_and(|e| e == "sol") {
            let src = Source::read(dir).map_err(|e| eyre::eyre!("{e}"))?;
            sources.insert(dir.to_path_buf(), src);
        }
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            load_sol_sources_recursive(&path, sources)?;
        } else if path.extension().is_some_and(|e| e == "sol") {
            let src = Source::read(&path).map_err(|e| eyre::eyre!("{e}"))?;
            sources.insert(path, src);
        }
    }
    Ok(())
}

/// Runs `macro_rule` over the Solidity sources in `source`, then compares the
/// expanded output file-by-file against the pre-expanded sources in `expected`
/// (matched by path relative to their respective roots).
///
/// On mismatch, the actual expanded content is written to `mismatches` (mirroring
/// the same relative path structure) so testers can inspect the output and copy
/// it to `expected` if it is correct.
pub fn test_macros(
    source: impl AsRef<Path>,
    expected: impl AsRef<Path>,
    mismatches: impl AsRef<Path>,
    macro_rules: &[Macro],
) -> eyre::Result<()> {
    let source = source.as_ref();
    let expected = expected.as_ref();
    let snapshot = mismatches.as_ref();

    let sources = expand_macros(source, None, macro_rules)?;
    let expected_sources = load_sol_sources(expected)?;

    let mut failures: Vec<(std::path::PathBuf, String)> = Vec::new();
    for (actual_path, actual_src) in &sources {
        let relative_path = actual_path.strip_prefix(source).unwrap();
        let expected_path = expected.join(relative_path);
        let actual_formatted = crate::display::format_sol(actual_src.content.as_str());
        let matches = expected_sources.get(&expected_path).is_some_and(|exp| {
            actual_formatted == crate::display::format_sol(exp.content.as_str())
        });
        if !matches {
            failures.push((relative_path.to_path_buf(), actual_formatted));
        }
    }

    if !failures.is_empty() {
        for (relative_path, content) in &failures {
            let snapshot_path = snapshot.join(relative_path);
            if let Some(parent) = snapshot_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&snapshot_path, content)?;
        }
        let paths: Vec<_> = failures.iter().map(|(p, _)| p.display().to_string()).collect();
        eyre::bail!(
            "macro expansion mismatch in: {}\nActual output written to '{}' — copy to '{}' if correct.",
            paths.join(", "),
            snapshot.display(),
            expected.display(),
        );
    }

    Ok(())
}

/// Runs `macro_rules` over the Solidity sources in `source` and expects at the
/// rule to return an error. Returns the error if one is produced, else fails
/// if the rule completes without error.
pub fn test_macro_err(source: impl AsRef<Path>, macro_rule: Macro) -> eyre::Result<eyre::Report> {
    match expand_macros(source, None, &[macro_rule]) {
        Err(e) => Ok(e),
        Ok(_) => eyre::bail!("expected macro to error but it succeeded"),
    }
}

/// Runs `macro_rules` over the Solidity sources in `source` and returns the
/// expanded [`Sources`] map. Rules are applied in order, with each rule seeing
/// the output of the previous.
///
/// When `paths` is `Some`, Solar is initialised with the provided
/// `ProjectPathsConfig` so that Foundry remappings are resolved correctly. Pass
/// `None` for self-contained test fixtures that have no external imports.
pub fn expand_macros(
    source: impl AsRef<Path>,
    paths: Option<&ProjectPathsConfig<SolcLanguage>>,
    macro_rules: &[Macro],
) -> eyre::Result<Sources> {
    let source = source.as_ref();
    let sources = load_sol_sources(source)?;
    expand_macros_with_sources(sources, source, paths, macro_rules)
}

/// Runs `macro_rules` over a pre-loaded [`Sources`] map and returns the expanded result.
///
/// Unlike [`expand_macros`], this does not load files from disk — callers supply the
/// sources directly. `root` is used as [`PreprocessingData::root_dir`].  When `paths`
/// is `Some`, Solar is initialised with the provided `ProjectPathsConfig` so that
/// Foundry remappings are resolved correctly for any imports that solar needs to follow.
pub fn expand_macros_with_sources(
    mut sources: Sources,
    root: &Path,
    paths: Option<&ProjectPathsConfig<SolcLanguage>>,
    macro_rules: &[Macro],
) -> eyre::Result<Sources> {
    let mut compiler = match paths {
        Some(paths) => {
            foundry_compilers::resolver::parse::SolParser::new(paths.with_language_ref())
                .into_compiler()
        }
        None => {
            let session = Session::builder().with_stderr_emitter().build();
            Compiler::new(session)
        }
    };

    compiler
        .enter_mut(|compiler| -> foundry_compilers::error::Result<()> {
            let mut pcx = compiler.parse();
            for (path, src) in sources.iter() {
                if let Ok(src_file) =
                    compiler.sess().source_map().new_source_file(path.clone(), src.content.as_str())
                {
                    pcx.add_file(src_file);
                }
            }
            pcx.parse();
            let Ok(ControlFlow::Continue(())) = compiler.lower_asts() else {
                return Ok(());
            };

            let relative_paths_storage;
            let src_dir = match paths {
                Some(paths) => {
                    relative_paths_storage = paths.paths_relative();
                    &relative_paths_storage.sources
                }
                None => root,
            };
            let mut mocks = HashSet::new();
            let mut data = PreprocessingData {
                input: &mut sources,
                root_dir: root,
                src_dir,
                mocks: &mut mocks,
                offset_adjustments: Vec::new(),
            };
            let gcx = compiler.gcx();
            for rule in macro_rules {
                rule(&gcx, &mut data)?;
            }
            Ok(())
        })
        .map_err(|e| eyre::eyre!("{e}"))?;

    Ok(sources)
}
