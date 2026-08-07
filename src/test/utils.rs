// Copyright (C) 2026, Ava Labs, Inc. All rights reserved.
// See the file LICENSE for licensing terms.
//
// Portions of this file are derived from Foundry
// (https://github.com/foundry-rs/foundry), file
// `crates/forge/src/cmd/test/summary.rs`.
// Copyright (c) 2021 Georgios Konstantopoulos
// Licensed under the MIT License.

use std::{collections::BTreeMap, fmt, time::Duration};

use chrono::Utc;
use comfy_table::{
    Cell, Color, Row, Table, modifiers::UTF8_ROUND_CORNERS, presets::ASCII_MARKDOWN,
};
use forge::{
    MultiContractRunner,
    decode::decode_console_logs,
    result::{SuiteResult, TestOutcome, TestStatus},
};
use foundry_common::{TestFunctionExt, fs, shell};
use foundry_config::Config;
use itertools::Itertools;
use quick_junit::{NonSuccessKind, Report, TestCase, TestCaseStatus, TestSuite};
use regex::Regex;

use crate::test::filter::ProjectPathsAwareFilter;
// ---------------------------------------------------------------------------
// Summary helpers (duplicated from forge's private summary module)
// ---------------------------------------------------------------------------

/// Represents a test summary report.
pub struct TestSummaryReport {
    is_detailed: bool,
    outcome: TestOutcome,
}

impl TestSummaryReport {
    pub fn new(is_detailed: bool, outcome: TestOutcome) -> Self {
        Self { is_detailed, outcome }
    }
}

impl fmt::Display for TestSummaryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if shell::is_json() {
            writeln!(f, "{}", &self.format_json_output(&self.is_detailed, &self.outcome))?;
        } else {
            writeln!(f, "\n{}", &self.format_table_output(&self.is_detailed, &self.outcome))?;
        }
        Ok(())
    }
}

impl TestSummaryReport {
    fn format_json_output(&self, is_detailed: &bool, outcome: &TestOutcome) -> String {
        let output = serde_json::json!({
            "results": outcome.results.iter().map(|(contract, suite)| {
                let (suite_path, suite_name) = contract.split_once(':').unwrap();
                let passed = suite.successes().count();
                let failed = suite.failures().count();
                let skipped = suite.skips().count();
                let mut result = serde_json::json!({
                    "suite": suite_name,
                    "passed": passed,
                    "failed": failed,
                    "skipped": skipped,
                });
                if *is_detailed {
                    result["file_path"] = serde_json::Value::String(suite_path.to_string());
                    result["duration"] =
                        serde_json::Value::String(format!("{:.2?}", suite.duration));
                }
                result
            }).collect::<Vec<serde_json::Value>>(),
        });
        serde_json::to_string_pretty(&output).unwrap()
    }

    fn format_table_output(&self, is_detailed: &bool, outcome: &TestOutcome) -> Table {
        let mut table = Table::new();
        if shell::is_markdown() {
            table.load_preset(ASCII_MARKDOWN);
        } else {
            table.apply_modifier(UTF8_ROUND_CORNERS);
        }
        let mut row = Row::from(vec![
            Cell::new("Test Suite"),
            Cell::new("Passed").fg(Color::Green),
            Cell::new("Failed").fg(Color::Red),
            Cell::new("Skipped").fg(Color::Yellow),
        ]);
        if *is_detailed {
            row.add_cell(Cell::new("File Path").fg(Color::Cyan));
            row.add_cell(Cell::new("Duration").fg(Color::Cyan));
        }
        table.set_header(row);
        for (contract, suite) in &outcome.results {
            let mut row = Row::new();
            let (suite_path, suite_name) = contract.split_once(':').unwrap();
            let passed = suite.successes().count();
            let mut passed_cell = Cell::new(passed);
            let failed = suite.failures().count();
            let mut failed_cell = Cell::new(failed);
            let skipped = suite.skips().count();
            let mut skipped_cell = Cell::new(skipped);
            row.add_cell(Cell::new(suite_name));
            if passed > 0 {
                passed_cell = passed_cell.fg(Color::Green);
            }
            row.add_cell(passed_cell);
            if failed > 0 {
                failed_cell = failed_cell.fg(Color::Red);
            }
            row.add_cell(failed_cell);
            if skipped > 0 {
                skipped_cell = skipped_cell.fg(Color::Yellow);
            }
            row.add_cell(skipped_cell);
            if self.is_detailed {
                row.add_cell(Cell::new(suite_path));
                row.add_cell(Cell::new(format!("{:.2?}", suite.duration)));
            }
            table.add_row(row);
        }
        table
    }
}

pub fn format_invariant_metrics_table(
    test_metrics: &std::collections::HashMap<String, forge::executors::invariant::InvariantMetrics>,
) -> Table {
    let mut table = Table::new();
    if shell::is_markdown() {
        table.load_preset(ASCII_MARKDOWN);
    } else {
        table.apply_modifier(UTF8_ROUND_CORNERS);
    }
    table.set_header(vec![
        Cell::new("Contract"),
        Cell::new("Selector"),
        Cell::new("Calls").fg(Color::Green),
        Cell::new("Reverts").fg(Color::Red),
        Cell::new("Discards").fg(Color::Yellow),
    ]);
    for name in test_metrics.keys().sorted() {
        if let Some((contract, selector)) =
            name.split_once(':').map_or(name.as_str(), |(_, contract)| contract).split_once('.')
        {
            let mut row = Row::new();
            row.add_cell(Cell::new(contract));
            row.add_cell(Cell::new(selector));
            if let Some(metrics) = test_metrics.get(name) {
                let calls_cell = Cell::new(metrics.calls).fg(if metrics.calls > 0 {
                    Color::Green
                } else {
                    Color::White
                });
                let reverts_cell = Cell::new(metrics.reverts).fg(if metrics.reverts > 0 {
                    Color::Red
                } else {
                    Color::White
                });
                let discards_cell = Cell::new(metrics.discards).fg(if metrics.discards > 0 {
                    Color::Yellow
                } else {
                    Color::White
                });
                row.add_cell(calls_cell);
                row.add_cell(reverts_cell);
                row.add_cell(discards_cell);
            }
            table.add_row(row);
        }
    }
    table
}

// ---------------------------------------------------------------------------
// Stderr suppression (Unix-only fd 2 redirect around Solar-noisy build call)
// ---------------------------------------------------------------------------

/// Temporarily redirect fd 2 to /dev/null for the duration of `f`, then restore it.
/// On non-Unix platforms this is a no-op.
#[cfg(unix)]
pub fn suppress_stderr<F, T>(f: F) -> T
where
    F: FnOnce() -> T,
{
    unsafe {
        let saved = libc::dup(2);
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        libc::dup2(devnull, 2);
        libc::close(devnull);
        let result = f();
        libc::dup2(saved, 2);
        libc::close(saved);
        result
    }
}

#[cfg(not(unix))]
pub fn suppress_stderr<F, T>(f: F) -> T
where
    F: FnOnce() -> T,
{
    f()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Creates a new Solar compiler instance with a silent (buffer) emitter and
/// loads the given sources into it. Used to replace the runner's Solar analysis
/// that was built on stale on-disk (pre-macro) sources.
pub fn create_silent_solar_analysis(
    sources: &foundry_compilers::artifacts::Sources,
) -> eyre::Result<solar::sema::Compiler> {
    let session = solar::interface::Session::builder().with_silent_emitter(None).build();
    let mut analysis = solar::sema::Compiler::new(session);
    analysis
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
            let _ = compiler.lower_asts();
            Ok(())
        })
        .map_err(|e| eyre::eyre!("{e}"))?;
    Ok(analysis)
}

/// Lists all matching tests.
pub fn list_tests(
    runner: MultiContractRunner,
    filter: &ProjectPathsAwareFilter,
) -> eyre::Result<TestOutcome> {
    let results = runner.list(filter);
    if shell::is_json() {
        foundry_common::sh_println!("{}", serde_json::to_string(&results)?)?;
    } else {
        for (file, contracts) in &results {
            foundry_common::sh_println!("{file}")?;
            for (contract, tests) in contracts {
                foundry_common::sh_println!("  {contract}")?;
                foundry_common::sh_println!("    {}\n", tests.join("\n    "))?;
            }
        }
    }
    Ok(TestOutcome::empty(Some(runner), false))
}

/// Load persisted filter (last test run failures) from file.
pub fn last_run_failures(config: &Config) -> Option<Regex> {
    match fs::read_to_string(&config.test_failures_file) {
        Ok(filter) => Regex::new(&filter)
            .inspect_err(|e| {
                let _ = foundry_common::sh_warn!(
                    "failed to parse test filter from {:?}: {e}",
                    config.test_failures_file
                );
            })
            .ok(),
        Err(_) => None,
    }
}

/// Persist filter with last test run failures (only if there's any failure).
pub fn persist_run_failures(config: &Config, outcome: &TestOutcome) {
    if outcome.failed() > 0 && fs::create_file(&config.test_failures_file).is_ok() {
        let mut filter = String::new();
        let mut failures = outcome.failures().peekable();
        while let Some((test_name, _)) = failures.next() {
            if test_name.is_any_test()
                && let Some(test_match) = test_name.split("(").next()
            {
                filter.push_str(test_match);
                if failures.peek().is_some() {
                    filter.push('|');
                }
            }
        }
        let _ = fs::write(&config.test_failures_file, filter);
    }
}

/// Generate test report in JUnit XML report format.
pub fn junit_xml_report(results: &BTreeMap<String, SuiteResult>, verbosity: u8) -> Report {
    let mut total_duration = Duration::default();
    let mut junit_report = Report::new("Test run");
    junit_report.set_timestamp(Utc::now());
    for (suite_name, suite_result) in results {
        let mut test_suite = TestSuite::new(suite_name);
        total_duration += suite_result.duration;
        test_suite.set_time(suite_result.duration);
        test_suite.set_system_out(suite_result.summary());
        for (test_name, test_result) in &suite_result.test_results {
            let mut test_status = match test_result.status {
                TestStatus::Success => TestCaseStatus::success(),
                TestStatus::Failure => TestCaseStatus::non_success(NonSuccessKind::Failure),
                TestStatus::Skipped => TestCaseStatus::skipped(),
            };
            if let Some(reason) = &test_result.reason {
                test_status.set_message(reason);
            }
            let mut test_case = TestCase::new(test_name, test_status);
            test_case.set_time(test_result.duration);
            let mut sys_out = String::new();
            let result_report = test_result.kind.report();
            use std::fmt::Write;
            write!(sys_out, "{test_result} {test_name} {result_report}").unwrap();
            if verbosity >= 2 && !test_result.logs.is_empty() {
                write!(sys_out, "\\nLogs:\\n").unwrap();
                let console_logs = decode_console_logs(&test_result.logs);
                for log in console_logs {
                    write!(sys_out, "  {log}\\n").unwrap();
                }
            }
            test_case.set_system_out(sys_out);
            test_suite.add_test_case(test_case);
        }
        junit_report.add_test_suite(test_suite);
    }
    junit_report.set_time(total_duration);
    junit_report
}
