// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.
//
// Portions of this file are derived from Foundry
// (https://github.com/foundry-rs/foundry), file
// `crates/forge/src/cmd/build.rs`.
// Copyright (c) 2021 Georgios Konstantopoulos
// Licensed under the MIT License.

use std::{collections::BTreeMap, env, fs, path::PathBuf};

use eyre::Context;
use forge::cmd::{build::BuildArgs, install};
use forge_lint::{
    linter::Linter,
    sol::{SolLint, SolidityLinter},
};
use foundry_cli::{
    opts::{configure_pcx_from_solc, get_solar_sources_from_compile_output},
    utils::{LoadConfig, cache_local_signatures},
};
use foundry_common::{fs::canonicalize_path, sh_println, sh_warn, shell};
use foundry_compilers::{
    CompilationError, FileFilter, Language, Project, ProjectCompileOutput,
    artifacts::{SolcLanguage, Sources},
    multi::MultiCompilerLanguage,
    utils::source_files_iter,
};
use foundry_config::{Config, SkipBuildFilters, filter::expand_globs};
use solar::{interface::Session, sema::Compiler};

use crate::{
    lockfile::{check_foundry_lock_consistency, check_soldeer_lock_consistency},
    project_compiler::ProjectCompiler,
    testing::expand_macros_with_sources,
};

/// Builds the project. First it does macro expansion as a preprocessing step
/// before passing the modified sources to the solc compiler. Linting is performed
/// if enabled.
///
/// N.B. Only works for the solc compiler.
/// N.B. If macro expansions are enabled, it is not currently supported to also use dynamic
///      test linking
pub async fn build(build_args: BuildArgs, macros: crate::MacroRules) -> eyre::Result<()> {
    let mut config = build_args.load_config()?;
    if install::install_missing_dependencies(&mut config).await && config.auto_detect_remappings {
        // need to re-configure here to also catch additional remappings
        config = build_args.load_config()?;
    }
    check_soldeer_lock_consistency(&config).await;
    check_foundry_lock_consistency(&config);
    let project = config.project()?;

    // Collect sources to compile if build subdirectories specified.
    let mut files = vec![];
    if let Some(paths) = &build_args.paths {
        for path in paths {
            let joined = project.root().join(path);
            let path = if joined.exists() { &joined } else { path };
            files.extend(source_files_iter(path, MultiCompilerLanguage::FILE_EXTENSIONS));
        }
        if files.is_empty() {
            eyre::bail!("No source files found in specified build paths.")
        }
    }
    let format_json = shell::is_json();
    let compiler = ProjectCompiler {
        project_root: project.root().to_path_buf(),
        print_names: build_args.names,
        print_sizes: build_args.sizes,
        bail: !format_json,
        ignore_eip_3860: build_args.ignore_eip_3860,
        files,
    };

    let rules = macros.rules.clone();
    let input_sources = macros.preprocessed_sources.clone();
    let mut output = compiler.compile(&project, macros)?;

    // When compilation is skipped (all outputs cached) the preprocessor is never called, so
    // `preprocessed_sources` is None.  Run a lightweight expansion pass so the linter always
    // analyses macro-expanded source rather than the original pre-expansion text.
    let preprocessed_sources = match input_sources.lock().unwrap().take() {
        Some(sources) => Some(sources),
        None if !rules.is_empty() => {
            let paths = project.paths.clone().with_language::<SolcLanguage>();
            let sources = project.paths.read_input_files()?;
            expand_macros_with_sources(sources, project.root(), Some(&paths), &rules).ok()
        }
        None => None,
    };

    // Cache project selectors.
    cache_local_signatures(&output)?;

    if format_json && !build_args.names && !build_args.sizes {
        sh_println!("{}", serde_json::to_string_pretty(&output.output())?)?;
    }

    // Only run the `SolidityLinter` if lint on build and no compilation errors.
    if config.lint.lint_on_build && !output.output().errors.iter().any(CompilationError::is_error) {
        lint(&project, &config, build_args.paths.as_deref(), &mut output, preprocessed_sources)
            .wrap_err("Lint Failed")?;
    }
    Ok(())
}

fn lint(
    project: &Project,
    config: &Config,
    files: Option<&[PathBuf]>,
    output: &mut ProjectCompileOutput,
    preprocessed_sources: Option<Sources>,
) -> eyre::Result<()> {
    let format_json = shell::is_json();
    if project.compiler.solc.is_some() && !shell::is_quiet() {
        let linter = SolidityLinter::new(config.project_paths())
            .with_json_emitter(format_json)
            .with_description(!format_json)
            .with_severity(if config.lint.severity.is_empty() {
                None
            } else {
                Some(config.lint.severity.clone())
            })
            .without_lints(if config.lint.exclude_lints.is_empty() {
                None
            } else {
                Some(
                    config
                        .lint
                        .exclude_lints
                        .iter()
                        .filter_map(|s| SolLint::try_from(s.as_str()).ok())
                        .collect(),
                )
            });

        // Expand ignore globs and canonicalize from the get go
        let ignored = expand_globs(&config.root, config.lint.ignore.iter())?
            .iter()
            .flat_map(canonicalize_path)
            .collect::<Vec<_>>();

        let skip = SkipBuildFilters::new(config.skip.clone(), config.root.clone());
        let curr_dir = env::current_dir()?;
        let input_files = config
            .project_paths::<SolcLanguage>()
            .input_files_iter()
            .filter(|p| {
                // Lint only specified build files, if any.
                if let Some(files) = files {
                    return files.iter().any(|file| &curr_dir.join(file) == p);
                }
                skip.is_match(p) && !(ignored.contains(p) || ignored.contains(&curr_dir.join(p)))
            })
            .collect::<Vec<_>>();

        let mut solar_sources =
            get_solar_sources_from_compile_output(config, output, Some(&input_files), None)?;
        if let Some(preprocessed) = preprocessed_sources {
            // Canonicalize preprocessed keys to match those produced by
            // `get_solar_sources_from_compile_output`, which uses `dunce::canonicalize`.
            let canonicalized: BTreeMap<_, _> = preprocessed
                .into_iter()
                .filter_map(|(p, s)| {
                    let abs = if p.is_absolute() { p } else { project.root().join(&p) };
                    fs::canonicalize(&abs).ok().map(|cp| (cp, s))
                })
                .collect::<BTreeMap<_, _>>();
            for (path, source) in &mut solar_sources.input.sources {
                if let Some(pre_source) = canonicalized.get(path) {
                    *source = pre_source.clone();
                }
            }
        }
        if solar_sources.input.sources.is_empty() {
            if !input_files.is_empty() {
                sh_warn!("unable to lint. Solar only supports Solidity versions >=0.8.0")?;
            }
            return Ok(());
        }

        // NOTE(rusowsky): Once solar can drop unsupported versions, rather than creating a new
        // compiler, we should reuse the parser from the project output.
        let mut compiler = Compiler::new(Session::builder().with_stderr_emitter().build());

        // Load the solar-compatible sources to the pcx before linting
        compiler.enter_mut(|compiler| {
            let mut pcx = compiler.parse();
            configure_pcx_from_solc(&mut pcx, &config.project_paths(), &solar_sources, true);
            pcx.set_resolve_imports(true);
            pcx.parse();
        });
        linter.lint(&input_files, config.deny, &mut compiler)?;
    }

    Ok(())
}
