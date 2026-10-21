// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.

//! Test utilities for verifying macro expansions against expected Solidity output.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use foundry_compilers::{
    Language, ProjectPathsConfig, SourceParser,
    artifacts::{SolcLanguage, Source, Sources},
    error::Result as SolcResult,
    resolver::parse::SolParser,
    utils::source_files_iter,
};
use solar::{parse::interface::Session, sema::Compiler};

use crate::{Macro, PreprocessingData, display::format_sol, span_utils::OffsetAdjustment};

/// Loads all `.sol` files under `dir` into a `Sources` map keyed by absolute path.
pub(crate) fn load_sol_sources(dir: &Path) -> eyre::Result<Sources> {
    Source::read_all(source_files_iter(dir, SolcLanguage::FILE_EXTENSIONS))
        .map_err(|e| eyre::eyre!("{e}"))
}

/// Builder for testing macro expansion against expected output.
///
/// Use the setter methods to specify the [`source`](MacroTest::source),
/// [`expected`](MacroTest::expected), and [`mismatches`](MacroTest::mismatches)
/// directories, then call [`run`](MacroTest::run).
///
/// # Example
///
/// ```rust,no_run
/// # fn main() -> eyre::Result<()> {
/// # let rule: reforge::Macro = |_, _| Ok(());
/// reforge::testing::MacroTest::new()
///     .source("tests/source")
///     .expected("tests/expected")
///     .mismatches("tests/mismatches")
///     .run(&[rule])?;
/// # Ok(())
/// # }
/// ```
pub struct MacroTest {
    source: Option<PathBuf>,
    expected: Option<PathBuf>,
    mismatches: Option<PathBuf>,
}

impl MacroTest {
    /// Creates a new [`MacroTest`] with no directories set.
    pub fn new() -> Self {
        Self { source: None, expected: None, mismatches: None }
    }

    /// Sets the directory of input `.sol` files to run macros against (read-only).
    pub fn source(mut self, path: impl AsRef<Path>) -> Self {
        self.source = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the directory of pre-expanded `.sol` files to compare against.
    pub fn expected(mut self, path: impl AsRef<Path>) -> Self {
        self.expected = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the directory where actual expanded output is written on mismatch,
    /// so you can inspect it and copy it to [`expected`](MacroTest::expected) if correct.
    pub fn mismatches(mut self, path: impl AsRef<Path>) -> Self {
        self.mismatches = Some(path.as_ref().to_path_buf());
        self
    }

    /// Runs `macro_rules` over the source files and compares the expanded output
    /// against the expected files, file-by-file (matched by relative path).
    ///
    /// On mismatch the actual content is written to the mismatches directory and
    /// the call returns an error listing the differing files.
    ///
    /// # Errors
    ///
    /// Returns an error if the sources cannot be read, if macro expansion fails, if
    /// the mismatch output cannot be written, or if any expanded file differs from
    /// its counterpart in `expected`.
    pub fn run(self, macro_rules: &[Macro]) -> eyre::Result<()> {
        let source = self
            .source
            .ok_or_else(|| eyre::eyre!("MacroTest: source directory not set — call .source()"))?;
        let expected = self.expected.ok_or_else(|| {
            eyre::eyre!("MacroTest: expected directory not set — call .expected()")
        })?;
        let snapshot = self.mismatches.ok_or_else(|| {
            eyre::eyre!("MacroTest: mismatches directory not set — call .mismatches()")
        })?;

        let sources = expand_macros(&source, None, macro_rules)?;
        let expected_sources = load_sol_sources(&expected)?;

        let mut failures: Vec<(PathBuf, String)> = Vec::new();
        for (actual_path, actual_src) in &sources {
            let relative_path = actual_path.strip_prefix(&source).map_err(|_| {
                eyre::eyre!(
                    "expanded source path '{}' is not under source root '{}'",
                    actual_path.display(),
                    source.display()
                )
            })?;
            let expected_path = expected.join(relative_path);
            let actual_formatted = format_sol(actual_src.content.as_str());
            let matches = expected_sources
                .get(&expected_path)
                .is_some_and(|exp| actual_formatted == format_sol(exp.content.as_str()));
            if !matches {
                failures.push((relative_path.to_path_buf(), actual_formatted));
            }
        }

        if !failures.is_empty() {
            for (relative_path, content) in &failures {
                let snapshot_path = snapshot.join(relative_path);
                if let Some(parent) = snapshot_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&snapshot_path, content)?;
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
}

impl Default for MacroTest {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs `macro_rules` over the Solidity sources in `source`, then compares the
/// expanded output file-by-file against the pre-expanded sources in `expected`
/// (matched by path relative to their respective roots).
///
/// On mismatch, the actual expanded content is written to `mismatches` (mirroring
/// the same relative path structure) so testers can inspect the output and copy
/// it to `expected` if it is correct.
///
/// # Deprecated
///
/// Use [`MacroTest`] instead to avoid accidental argument transposition:
///
/// ```rust,no_run
/// # fn main() -> eyre::Result<()> {
/// # let rule: reforge::Macro = |_, _| Ok(());
/// reforge::testing::MacroTest::new()
///     .source("tests/source")
///     .expected("tests/expected")
///     .mismatches("tests/mismatches")
///     .run(&[rule])?;
/// # Ok(())
/// # }
/// ```
#[deprecated(since = "0.1.0", note = "use `MacroTest` builder instead")]
pub fn test_macros(
    source: impl AsRef<Path>,
    expected: impl AsRef<Path>,
    mismatches: impl AsRef<Path>,
    macro_rules: &[Macro],
) -> eyre::Result<()> {
    MacroTest::new().source(source).expected(expected).mismatches(mismatches).run(macro_rules)
}

/// Runs `macro_rule` over the Solidity sources in `source` and expects the
/// rule to return an error. Returns the error if one is produced, else fails
/// if the rule completes without error.
///
/// # Errors
///
/// Returns an error if the rule unexpectedly succeeds.
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
///
/// # Errors
///
/// Returns an error if the sources cannot be read from disk, if Solar fails to
/// parse them, or if any macro rule returns an error.
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
///
/// # Errors
///
/// Returns an error if Solar fails to parse the sources or if any macro rule
/// returns an error.
pub fn expand_macros_with_sources(
    mut sources: Sources,
    root: &Path,
    paths: Option<&ProjectPathsConfig<SolcLanguage>>,
    macro_rules: &[Macro],
) -> eyre::Result<Sources> {
    let mut compiler = if let Some(paths) = paths {
        SolParser::new(paths.with_language_ref()).into_compiler()
    } else {
        let session = Session::builder().with_silent_emitter(None).build();
        Compiler::new(session)
    };

    crate::solar_load_and_lower(
        &mut compiler,
        sources.iter().map(|(p, s)| (p.clone(), s.content.as_str())),
    );

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
        offset_adjustments: OffsetAdjustment::default(),
    };
    compiler
        .enter_mut(|compiler| -> SolcResult<()> {
            let gcx = compiler.gcx();
            for rule in macro_rules {
                rule(&gcx, &mut data)?;
            }
            Ok(())
        })
        .map_err(|e| eyre::eyre!("{e}"))?;

    Ok(sources)
}