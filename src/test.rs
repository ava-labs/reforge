// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.
//
// Portions of this file are derived from Foundry
// (https://github.com/foundry-rs/foundry), file
// `crates/forge/src/cmd/test/mod.rs`.
// Copyright (c) 2021 Georgios Konstantopoulos
// Licensed under the MIT License.

mod filter;
mod utils;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::BufWriter,
    panic::resume_unwind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc::channel},
    time::Instant,
};

use clap::{Parser, ValueHint};
use eyre::{Context, OptionExt, bail};
use forge::{
    MultiContractRunner, MultiContractRunnerBuilder,
    cmd::{install, test::FilterArgs, watch::WatchArgs},
    core::opts::EvmOpts,
    decode::decode_console_logs,
    fuzz::strategies::LiteralsDictionary,
    gas_report::GasReport,
    multi_runner::matches_artifact,
    result::{SuiteResult, TestKind, TestOutcome, TestStatus},
    revm::primitives::U256,
    traces::{
        CallTraceDecoderBuilder, InternalTraceMode, TraceKind,
        backtrace::BacktraceBuilder,
        debug::{ContractSources, DebugTraceIdentifier},
        decode_trace_arena, folded_stack_trace,
        identifier::{SignaturesIdentifier, TraceIdentifiers},
        prune_trace_depth, render_trace_arena_inner,
    },
};
use foundry_cli::{
    opts::{BuildOpts, EvmArgs, GlobalArgs},
    utils::{LoadConfig, did_you_mean},
};
use foundry_common::{EmptyTestFilter, fs, shell};
use foundry_compilers::{
    ProjectCompileOutput,
    artifacts::{Sources, output_selection::OutputSelection},
    compilers::{
        Language,
        multi::{MultiCompiler, MultiCompilerLanguage},
    },
    utils::source_files_iter,
};
use foundry_config::{
    Config,
    figment::{
        self, Metadata, Profile, Provider,
        value::{Dict, Map},
    },
    filter::GlobMatcher,
};
use foundry_debugger::Debugger;
use inferno::flamegraph::from_lines;
use tokio::task::spawn_blocking;
use yansi::Paint;

use crate::{
    project_compiler::ProjectCompiler,
    test::{
        filter::{ProjectPathsAwareFilter, merge_filter_with_config},
        utils::{
            TestSummaryReport, create_silent_solar_analysis, format_invariant_metrics_table,
            junit_xml_report, last_run_failures, list_tests, persist_run_failures, suppress_stderr,
        },
    },
};

// Loads project's figment and merges the build cli arguments into it
foundry_config::merge_impl_figment_convert!(TestArgs, build, evm);

/// CLI arguments for `forge test`.
#[derive(Clone, Debug, Parser)]
#[command(next_help_heading = "Test options")]
pub struct TestArgs {
    // Include global options for users of this struct.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// The contract file you want to test, it's a shortcut for --match-path.
    #[arg(value_hint = ValueHint::FilePath)]
    pub path: Option<GlobMatcher>,

    /// Run a single test in the debugger.
    ///
    /// The matching test will be opened in the debugger regardless of the outcome of the test.
    ///
    /// If the matching test is a fuzz test, then it will open the debugger on the first failure
    /// case. If the fuzz test does not fail, it will open the debugger on the last fuzz case.
    #[arg(long, conflicts_with_all = ["flamegraph", "flamechart", "decode_internal", "rerun"])]
    debug: bool,

    /// Generate a flamegraph for a single test. Implies `--decode-internal`.
    ///
    /// A flame graph is used to visualize which functions or operations within the smart contract
    /// are consuming the most gas overall in a sorted manner.
    #[arg(long)]
    flamegraph: bool,

    /// Generate a flamechart for a single test. Implies `--decode-internal`.
    ///
    /// A flame chart shows the gas usage over time, illustrating when each function is
    /// called (execution order) and how much gas it consumes at each point in the timeline.
    #[arg(long, conflicts_with = "flamegraph")]
    flamechart: bool,

    /// Identify internal functions in traces.
    ///
    /// This will trace internal functions and decode stack parameters.
    ///
    /// Parameters stored in memory (such as bytes or arrays) are currently decoded only when a
    /// single function is matched, similarly to `--debug`, for performance reasons.
    #[arg(long)]
    decode_internal: bool,

    /// Dumps all debugger steps to file.
    #[arg(
        long,
        requires = "debug",
        value_hint = ValueHint::FilePath,
        value_name = "PATH"
    )]
    dump: Option<PathBuf>,

    /// Print a gas report.
    #[arg(long, env = "FORGE_GAS_REPORT")]
    gas_report: bool,

    /// Check gas snapshots against previous runs.
    #[arg(long, env = "FORGE_SNAPSHOT_CHECK")]
    gas_snapshot_check: Option<bool>,

    /// Enable/disable recording of gas snapshot results.
    #[arg(long, env = "FORGE_SNAPSHOT_EMIT")]
    gas_snapshot_emit: Option<bool>,

    /// Exit with code 0 even if a test fails.
    #[arg(long, env = "FORGE_ALLOW_FAILURE")]
    allow_failure: bool,

    /// Suppress successful test traces and show only traces for failures.
    #[arg(long, short, env = "FORGE_SUPPRESS_SUCCESSFUL_TRACES", help_heading = "Display options")]
    suppress_successful_traces: bool,

    /// Defines the depth of a trace
    #[arg(long)]
    trace_depth: Option<usize>,

    /// Output test results as JUnit XML report.
    #[arg(long, conflicts_with_all = ["quiet", "json", "gas_report", "summary", "list", "show_progress"], help_heading = "Display options")]
    pub junit: bool,

    /// Stop running tests after the first failure.
    #[arg(long)]
    pub fail_fast: bool,

    /// The Etherscan (or equivalent) API key.
    #[arg(long, env = "ETHERSCAN_API_KEY", value_name = "KEY")]
    etherscan_api_key: Option<String>,

    /// List tests instead of running them.
    #[arg(long, short, conflicts_with_all = ["show_progress", "decode_internal", "summary"], help_heading = "Display options")]
    list: bool,

    /// Set seed used to generate randomness during your fuzz runs.
    #[arg(long)]
    pub fuzz_seed: Option<U256>,

    #[arg(long, env = "FOUNDRY_FUZZ_RUNS", value_name = "RUNS")]
    pub fuzz_runs: Option<u64>,

    /// Timeout for each fuzz run in seconds.
    #[arg(long, env = "FOUNDRY_FUZZ_TIMEOUT", value_name = "TIMEOUT")]
    pub fuzz_timeout: Option<u64>,

    /// File to rerun fuzz failures from.
    #[arg(long)]
    pub fuzz_input_file: Option<String>,

    /// Show test execution progress.
    #[arg(long, conflicts_with_all = ["quiet", "json"], help_heading = "Display options")]
    pub show_progress: bool,

    /// Re-run recorded test failures from last run.
    /// If no failure recorded then regular test run is performed.
    #[arg(long)]
    pub rerun: bool,

    /// Print test summary table.
    #[arg(long, help_heading = "Display options")]
    pub summary: bool,

    /// Print detailed test summary table.
    #[arg(long, help_heading = "Display options", requires = "summary")]
    pub detailed: bool,

    /// Disables the labels in the traces.
    #[arg(long, help_heading = "Display options")]
    pub disable_labels: bool,

    #[command(flatten)]
    filter: FilterArgs,

    #[command(flatten)]
    evm: EvmArgs,

    #[command(flatten)]
    pub build: BuildOpts,

    #[command(flatten)]
    pub watch: WatchArgs,
}

impl TestArgs {
    /// Returns the flattened [`FilterArgs`] arguments merged with [`Config`].
    /// Loads and applies filter from file if only last test run failures should be re-run.
    pub fn filter(&self, config: &Config) -> eyre::Result<ProjectPathsAwareFilter> {
        let mut filter = self.filter.clone();
        if self.rerun {
            filter.test_pattern = last_run_failures(config);
        }
        if filter.path_pattern.is_some() {
            if self.path.is_some() {
                bail!("Can not supply both --match-path and |path|");
            }
        } else {
            filter.path_pattern = self.path.clone();
        }
        Ok(merge_filter_with_config(filter, config))
    }

    /// Returns a list of files that need to be compiled in order to run all the tests that match
    /// the given filter.
    pub fn get_sources_to_compile(
        &self,
        config: &Config,
        test_filter: &ProjectPathsAwareFilter,
    ) -> eyre::Result<BTreeSet<PathBuf>> {
        if test_filter.is_empty() {
            return Ok(source_files_iter(&config.src, MultiCompilerLanguage::FILE_EXTENSIONS)
                .chain(source_files_iter(&config.test, MultiCompilerLanguage::FILE_EXTENSIONS))
                .collect());
        }

        let mut project = config.create_project(true, true)?;
        project.update_output_selection(|selection| {
            *selection = OutputSelection::common_output_selection(["abi".to_string()]);
        });
        let output = project.compile()?;
        if output.has_compiler_errors() {
            foundry_common::sh_println!("{output}")?;
            eyre::bail!("Compilation failed");
        }

        Ok(output
            .artifact_ids()
            .filter_map(|(id, artifact)| artifact.abi.as_ref().map(|abi| (id, abi)))
            .filter(|(id, abi)| {
                id.source.starts_with(&config.src) || matches_artifact(test_filter, id, abi)
            })
            .map(|(id, _)| id.source)
            .collect())
    }
}

impl Provider for TestArgs {
    fn metadata(&self) -> Metadata {
        Metadata::named("Core Build Args Provider")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut dict = Dict::default();

        let mut fuzz_dict = Dict::default();
        if let Some(fuzz_seed) = self.fuzz_seed {
            fuzz_dict.insert("seed".to_string(), fuzz_seed.to_string().into());
        }
        if let Some(fuzz_runs) = self.fuzz_runs {
            fuzz_dict.insert("runs".to_string(), fuzz_runs.into());
        }
        if let Some(fuzz_timeout) = self.fuzz_timeout {
            fuzz_dict.insert("timeout".to_string(), fuzz_timeout.into());
        }
        if let Some(fuzz_input_file) = self.fuzz_input_file.clone() {
            fuzz_dict.insert("failure_persist_file".to_string(), fuzz_input_file.into());
        }
        dict.insert("fuzz".to_string(), fuzz_dict.into());

        if let Some(etherscan_api_key) =
            self.etherscan_api_key.as_ref().filter(|s| !s.trim().is_empty())
        {
            dict.insert("etherscan_api_key".to_string(), etherscan_api_key.to_string().into());
        }

        if self.show_progress {
            dict.insert("show_progress".to_string(), true.into());
        }

        Ok(Map::from([(Config::selected_profile(), dict)]))
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Compiles the project with macro expansion applied and runs its tests, returning the outcome.
///
/// This mirrors [`TestArgs::compile_and_run`], but compiles through reforge's macro-aware
/// [`ProjectCompiler`]. It is shared by the `test` and `snapshot` subcommands.
pub(crate) async fn compile_and_run(
    test_args: &mut TestArgs,
    macros: crate::MacroRules,
) -> eyre::Result<TestOutcome> {
    let (mut config, evm_opts) = test_args.load_config_and_evm_opts()?;

    if install::install_missing_dependencies(&mut config).await && config.auto_detect_remappings {
        config = test_args.load_config()?;
    }

    let project = config.project()?;
    let filter = test_args.filter(&config)?;

    let files = test_args.get_sources_to_compile(&config, &filter)?.into_iter().collect::<Vec<_>>();

    let compiler = ProjectCompiler {
        project_root: project.root().to_path_buf(),
        print_names: false,
        print_sizes: false,
        bail: true,
        ignore_eip_3860: false,
        files,
    };

    // Clone the Arc *before* consuming `macros` so we can access expanded sources later.
    let preprocessed_sources_arc = Arc::clone(&macros.preprocessed_sources);

    let output = compiler.compile(&project, macros)?;

    let outcome = run_tests(
        test_args,
        &project.paths.root,
        config,
        evm_opts,
        &output,
        preprocessed_sources_arc,
        false,
    )
    .await?;
    Ok(outcome)
}

pub async fn test(mut test_args: TestArgs, macros: crate::MacroRules) -> eyre::Result<()> {
    let silent = test_args.junit || shell::is_json();
    let outcome = compile_and_run(&mut test_args, macros).await?;
    outcome.ensure_ok(silent)
}

/// Executes all the tests in the project.
pub async fn run_tests(
    args: &mut TestArgs,
    project_root: &Path,
    mut config: Config,
    mut evm_opts: EvmOpts,
    output: &ProjectCompileOutput,
    preprocessed_sources: Arc<Mutex<Option<Sources>>>,
    coverage: bool,
) -> eyre::Result<TestOutcome> {
    let filter = args.filter(&config)?;

    // Explicitly enable isolation for gas reports for more correct gas accounting.
    if args.gas_report {
        evm_opts.isolate = true;
    } else {
        config.fuzz.gas_report_samples = 0;
        config.invariant.gas_report_samples = 0;
    }

    let should_debug = args.debug;
    let should_draw = args.flamegraph || args.flamechart;

    let verbosity = evm_opts.verbosity;
    if (args.gas_report && evm_opts.verbosity < 3) || args.flamegraph || args.flamechart {
        evm_opts.verbosity = 3;
    }

    let env = evm_opts.evm_env().await?;

    if should_draw && !args.decode_internal {
        args.decode_internal = true;
    }

    let decode_internal =
        if args.decode_internal { InternalTraceMode::Simple } else { InternalTraceMode::None };

    let config = Arc::new(config);

    // Build the runner, suppressing Solar's stderr output.  Solar emits diagnostics on the
    // pre-macro (on-disk) sources at this point; those errors are spurious and will disappear
    // once the runner analysis is replaced below with one that uses the expanded sources.
    let mut runner = suppress_stderr(|| {
        MultiContractRunnerBuilder::new(config.clone())
            .set_debug(should_debug)
            .set_decode_internal(decode_internal)
            .initial_balance(evm_opts.initial_balance)
            .evm_spec(config.evm_spec_id())
            .sender(evm_opts.sender)
            .with_fork(evm_opts.get_fork(&config, env.clone()))
            .enable_isolation(evm_opts.isolate)
            .networks(evm_opts.networks)
            .fail_fast(args.fail_fast)
            .set_coverage(coverage)
            .build::<MultiCompiler>(output, env, evm_opts)
    })?;

    // If macro expansion produced expanded sources, replace the runner's Solar analysis
    // (which was built on stale on-disk sources) with a fresh silent one.
    let expanded_sources = preprocessed_sources.lock().unwrap().clone();
    if let Some(ref sources) = expanded_sources {
        let analysis = create_silent_solar_analysis(sources)?;
        let analysis = Arc::new(analysis);
        let fuzz_literals = LiteralsDictionary::new(
            Some(analysis.clone()),
            Some(config.project_paths()),
            config.fuzz.dictionary.max_fuzz_dictionary_literals,
        );
        runner.analysis = analysis;
        runner.fuzz_literals = fuzz_literals;
    }

    let libraries = runner.libraries.clone();
    let mut outcome =
        run_tests_inner(args, runner, config.clone(), verbosity, &filter, output).await?;

    if should_draw {
        let (suite_name, test_name, mut test_result) =
            outcome.remove_first().ok_or_eyre("no tests were executed")?;

        let (_, arena) =
            test_result.traces.iter_mut().find(|(kind, _)| *kind == TraceKind::Execution).unwrap();

        let decoder = outcome.last_run_decoder.as_ref().unwrap();
        decode_trace_arena(arena, decoder).await;
        let mut fst = folded_stack_trace::build(arena);

        let label = if args.flamegraph { "flamegraph" } else { "flamechart" };
        let contract = suite_name.split(':').next_back().unwrap();
        let test_name = test_name.trim_end_matches("()");
        let file_name = format!("cache/{label}_{contract}_{test_name}.svg");
        let file = File::create(&file_name).wrap_err("failed to create file")?;
        let file = BufWriter::new(file);

        let mut options = inferno::flamegraph::Options::default();
        options.title = format!("{label} {contract}::{test_name}");
        options.count_name = "gas".to_string();
        if args.flamechart {
            options.flame_chart = true;
            fst.reverse();
        }

        from_lines(&mut options, fst.iter().map(String::as_str), file)
            .wrap_err("failed to write svg")?;
        foundry_common::sh_println!("Saved to {file_name}")?;

        if let Err(e) = opener::open(&file_name) {
            foundry_common::sh_err!("Failed to open {file_name}; please open it manually: {e}")?;
        }
    }

    if should_debug {
        let (_, _, test_result) = outcome.remove_first().ok_or_eyre("no tests were executed")?;

        let sources = ContractSources::from_project_output(output, project_root, Some(&libraries))?;

        let mut builder = Debugger::builder()
            .traces(test_result.traces.iter().filter(|(t, _)| t.is_execution()).cloned().collect())
            .sources(sources)
            .breakpoints(test_result.breakpoints.clone());

        if let Some(decoder) = &outcome.last_run_decoder {
            builder = builder.decoder(decoder);
        }

        let mut debugger = builder.build();
        if let Some(dump_path) = &args.dump {
            debugger.dump_to_file(dump_path)?;
        } else {
            debugger.try_run_tui()?;
        }
    }

    Ok(outcome)
}

// ---------------------------------------------------------------------------
// run_tests_inner — duplicated from forge's private method, using public APIs
// ---------------------------------------------------------------------------

async fn run_tests_inner(
    args: &TestArgs,
    mut runner: MultiContractRunner,
    config: Arc<Config>,
    verbosity: u8,
    filter: &ProjectPathsAwareFilter,
    output: &ProjectCompileOutput,
) -> eyre::Result<TestOutcome> {
    if args.list {
        return list_tests(runner, filter);
    }

    let silent = args.gas_report && shell::is_json() || args.summary && shell::is_json();

    let num_filtered = runner.matching_test_functions(filter).count();

    if num_filtered == 0 {
        let mut total_tests = num_filtered;
        if !filter.is_empty() {
            total_tests = runner.matching_test_functions(&EmptyTestFilter::default()).count();
        }
        if total_tests == 0 {
            foundry_common::sh_println!(
                "No tests found in project! Forge looks for functions that start with `test`"
            )?;
        } else {
            let mut msg = format!("no tests match the provided pattern:\n{filter}");
            if let Some(test_pattern) = &filter.args().test_pattern {
                let test_name = test_pattern.as_str();
                let candidates = runner.all_test_functions(filter).map(|f| &f.name);
                if let Some(suggestion) = did_you_mean(test_name, candidates).pop() {
                    use std::fmt::Write;
                    write!(msg, "\nDid you mean `{suggestion}`?")?;
                }
            }
            foundry_common::sh_warn!("{msg}")?;
        }
        return Ok(TestOutcome::empty(Some(runner), false));
    }

    if num_filtered != 1 && (args.debug || args.flamegraph || args.flamechart) {
        let action = if args.flamegraph {
            "generate a flamegraph"
        } else if args.flamechart {
            "generate a flamechart"
        } else {
            "run the debugger"
        };
        let filter_str =
            if filter.is_empty() { String::new() } else { format!("\n\nFilter used:\n{filter}") };
        eyre::bail!(
            "{num_filtered} tests matched your criteria, but exactly 1 test must match in order to {action}.\n\n\
             Use --match-contract and --match-path to further limit the search.{filter_str}",
        );
    }

    if num_filtered == 1 && args.decode_internal {
        runner.decode_internal = InternalTraceMode::Full;
    }

    // Non-streaming JSON mode.
    if !args.gas_report && !args.summary && shell::is_json() {
        let mut results = runner.test_collect(filter)?;
        results.values_mut().for_each(|suite_result| {
            for test_result in suite_result.test_results.values_mut() {
                if verbosity >= 2 {
                    test_result.decoded_logs = decode_console_logs(&test_result.logs);
                } else {
                    test_result.logs = vec![];
                }
            }
        });
        foundry_common::sh_println!("{}", serde_json::to_string(&results)?)?;
        return Ok(TestOutcome::new(Some(runner), results, args.allow_failure));
    }

    if args.junit {
        let results = runner.test_collect(filter)?;
        foundry_common::sh_println!("{}", junit_xml_report(&results, verbosity).to_string()?)?;
        return Ok(TestOutcome::new(Some(runner), results, args.allow_failure));
    }

    let remote_chain =
        if runner.fork.is_some() { runner.env.tx.chain_id.map(Into::into) } else { None };
    let known_contracts = runner.known_contracts.clone();
    let libraries = runner.libraries.clone();

    // Run tests in a streaming fashion.
    let (tx, rx) = channel::<(String, SuiteResult)>();
    let timer = Instant::now();
    let show_progress = config.show_progress;
    let handle = spawn_blocking({
        let filter = filter.clone();
        move || runner.test(&filter, tx, show_progress).map(|()| runner)
    });

    // Set up trace identifiers.
    let mut identifier = TraceIdentifiers::new().with_local(&known_contracts);
    if !args.gas_report {
        identifier = identifier.with_external(&config, remote_chain)?;
    }

    // Build the trace decoder.
    let mut builder = CallTraceDecoderBuilder::new()
        .with_known_contracts(&known_contracts)
        .with_label_disabled(args.disable_labels)
        .with_verbosity(verbosity);
    if !args.gas_report {
        builder = builder.with_signature_identifier(SignaturesIdentifier::from_config(&config)?);
    }

    if args.decode_internal {
        let sources = ContractSources::from_project_output(output, &config.root, Some(&libraries))?;
        builder = builder.with_debug_identifier(DebugTraceIdentifier::new(sources));
    }
    let mut decoder = builder.build();

    let mut gas_report = args.gas_report.then(|| {
        GasReport::new(
            config.gas_reports.clone(),
            config.gas_reports_ignore.clone(),
            config.gas_reports_include_tests,
        )
    });

    let mut gas_snapshots = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut outcome = TestOutcome::empty(None, args.allow_failure);
    let mut any_test_failed = false;
    let mut backtrace_builder = None;

    for (contract_name, mut suite_result) in rx {
        let tests = &mut suite_result.test_results;
        let has_tests = !tests.is_empty();

        decoder.clear_addresses();

        let identify_addresses =
            verbosity >= 3 || args.gas_report || args.debug || args.flamegraph || args.flamechart;

        if !silent {
            foundry_common::sh_println!()?;
            for warning in &suite_result.warnings {
                foundry_common::sh_warn!("{warning}")?;
            }
            if has_tests {
                let len = tests.len();
                let tests_str = if len > 1 { "tests" } else { "test" };
                foundry_common::sh_println!("Ran {len} {tests_str} for {contract_name}")?;
            }
        }

        for (name, result) in tests {
            let show_traces =
                !args.suppress_successful_traces || result.status == TestStatus::Failure;
            if !silent {
                foundry_common::sh_println!("{}", result.short_result(name))?;

                if let TestKind::Invariant { metrics, .. } = &result.kind
                    && !metrics.is_empty()
                {
                    let _ = foundry_common::sh_println!(
                        "\n{}\n",
                        format_invariant_metrics_table(metrics)
                    );
                }

                if verbosity >= 2 && show_traces {
                    let console_logs = decode_console_logs(&result.logs);
                    if !console_logs.is_empty() {
                        foundry_common::sh_println!("Logs:")?;
                        for log in console_logs {
                            foundry_common::sh_println!("  {log}")?;
                        }
                        foundry_common::sh_println!()?;
                    }
                }
            }

            any_test_failed |= result.status == TestStatus::Failure;

            decoder.clear_addresses();
            decoder.labels.extend(result.labels.iter().map(|(k, v)| (*k, v.clone())));

            let mut decoded_traces = Vec::with_capacity(result.traces.len());
            for (kind, arena) in &mut result.traces {
                if identify_addresses {
                    decoder.identify(arena, &mut identifier);
                }
                let should_include = match kind {
                    TraceKind::Execution => {
                        (verbosity == 3 && result.status.is_failure()) || verbosity >= 4
                    }
                    TraceKind::Setup => {
                        (verbosity == 4 && result.status.is_failure()) || verbosity >= 5
                    }
                    TraceKind::Deployment => false,
                };
                if should_include {
                    decode_trace_arena(arena, &decoder).await;
                    if let Some(trace_depth) = args.trace_depth {
                        prune_trace_depth(arena, trace_depth);
                    }
                    decoded_traces.push(render_trace_arena_inner(arena, false, verbosity > 4));
                }
            }

            if !silent && show_traces && !decoded_traces.is_empty() {
                foundry_common::sh_println!("Traces:")?;
                for trace in &decoded_traces {
                    foundry_common::sh_println!("{trace}")?;
                }
            }

            if !silent
                && result.status.is_failure()
                && verbosity >= 3
                && !result.traces.is_empty()
                && let Some((_, arena)) =
                    result.traces.iter().find(|(kind, _)| matches!(kind, TraceKind::Execution))
            {
                let builder = backtrace_builder.get_or_insert_with(|| {
                    BacktraceBuilder::new(
                        output,
                        config.root.clone(),
                        config.parsed_libraries().ok(),
                        config.via_ir,
                    )
                });
                let backtrace = builder.from_traces(arena);
                if !backtrace.is_empty() {
                    foundry_common::sh_println!("{}", backtrace)?;
                }
            }

            if let Some(gas_report) = &mut gas_report {
                gas_report.analyze(result.traces.iter().map(|(_, a)| &a.arena), &decoder).await;
                for trace in &result.gas_report_traces {
                    decoder.clear_addresses();
                    for (kind, arena) in &result.traces {
                        if !matches!(kind, TraceKind::Execution) {
                            decoder.identify(arena, &mut identifier);
                        }
                    }
                    for arena in trace {
                        decoder.identify(arena, &mut identifier);
                        gas_report.analyze([arena], &decoder).await;
                    }
                }
            }
            result.gas_report_traces = Default::default();

            for (group, new_snapshots) in &result.gas_snapshots {
                gas_snapshots.entry(group.clone()).or_default().extend(new_snapshots.clone());
            }
        }

        // Write gas snapshots to disk if any were collected.
        if !gas_snapshots.is_empty() {
            if args.gas_snapshot_check.unwrap_or(config.gas_snapshot_check) {
                let differences_found = gas_snapshots.clone().into_iter().fold(
                    false,
                    |mut found, (group, snapshots)| {
                        if !&config.snapshots.join(format!("{group}.json")).exists() {
                            return false;
                        }
                        let previous_snapshots: BTreeMap<String, String> =
                            fs::read_json_file(&config.snapshots.join(format!("{group}.json")))
                                .expect("Failed to read snapshots from disk");
                        let diff: BTreeMap<_, _> = snapshots
                            .iter()
                            .filter_map(|(k, v)| {
                                previous_snapshots.get(k).and_then(|prev| {
                                    if prev != v {
                                        Some((k.clone(), (prev.clone(), v.clone())))
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect();
                        if !diff.is_empty() {
                            let _ = foundry_common::sh_eprintln!(
                                "{}",
                                format!("\n[{group}] Failed to match snapshots:").red().bold()
                            );
                            for (key, (prev, snap)) in &diff {
                                let _ = foundry_common::sh_eprintln!(
                                    "{}",
                                    format!("- [{key}] {prev} → {snap}").red()
                                );
                            }
                            found = true;
                        }
                        found
                    },
                );
                if differences_found {
                    foundry_common::sh_eprintln!()?;
                    eyre::bail!("Snapshots differ from previous run");
                }
            }

            if args.gas_snapshot_emit.unwrap_or(config.gas_snapshot_emit) {
                fs::create_dir_all(&config.snapshots)?;
                gas_snapshots.clone().into_iter().for_each(|(group, snapshots)| {
                    fs::write_pretty_json_file(
                        &config.snapshots.join(format!("{group}.json")),
                        &snapshots,
                    )
                    .expect("Failed to write gas snapshots to disk");
                });
            }
        }

        if !silent && has_tests {
            foundry_common::sh_println!("{}", suite_result.summary())?;
        }

        outcome.results.insert(contract_name, suite_result);

        if args.fail_fast && any_test_failed {
            break;
        }
    }

    outcome.last_run_decoder = Some(decoder);
    let duration = timer.elapsed();

    if let Some(gas_report) = gas_report {
        let finalized = gas_report.finalize();
        foundry_common::sh_println!("{}", &finalized)?;
        outcome.gas_report = Some(finalized);
    }

    if !args.summary && !shell::is_json() {
        foundry_common::sh_println!("{}", outcome.summary(duration))?;
    }

    if args.summary && !outcome.results.is_empty() {
        let summary_report = TestSummaryReport::new(args.detailed, outcome.clone());
        foundry_common::sh_println!("{}", &summary_report)?;
    }

    // Reattach the task.
    match handle.await {
        Ok(result) => outcome.runner = Some(result?),
        Err(e) => match e.try_into_panic() {
            Ok(payload) => resume_unwind(payload),
            Err(e) => return Err(e.into()),
        },
    }

    persist_run_failures(&config, &outcome);

    Ok(outcome)
}
