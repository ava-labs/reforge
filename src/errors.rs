use std::{ops::ControlFlow, path::Path};

use foundry_compilers::artifacts::Error as SolcError;

use crate::MacroRules;

#[cfg(test)]
pub static TEST_COMPILER_OUTPUT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

const ARROW: &str = "-->";

/// Remaps compiler error line numbers in `e` from macro-expanded coordinates back to the
/// original source, or replaces the message with a macro-error notice if the error points
/// into macro-generated code.
///
/// `loc.file` is used directly as the lookup key because foundry-compilers calls
/// `strip_prefix(project_root)` on the compiler input *before* the preprocessor runs, so
/// the paths stored in `offset_adjustments` are in the same form (relative or absolute) as
/// the paths Solc reports in `source_location.file`.
pub fn correct_fmt_msg(macros: &MacroRules, e: &mut SolcError) {
    let Some(ref loc) = e.source_location else { return };
    let source = Path::new(&loc.file);
    match remap_fmt_msg(macros, source, e) {
        Some(s) => e.formatted_message = Some(s),
        None => {
            // The error points into macro-generated code: strip the location and update both
            // the short message (shown in the header) and formatted_message so all display
            // paths reflect the attribution.
            let msg = format!("error in macro-generated code: {}", e.message);
            e.message = msg.clone();
            e.formatted_message = Some(msg);
        }
    }
}

/// Remaps all line numbers in the formatted compiler error message from macro-expanded
/// coordinates back to the original source. Returns `None` if the error's primary location
/// falls within a macro-generated block.
fn remap_fmt_msg(macros: &MacroRules, source: &Path, e: &SolcError) -> Option<String> {
    let fmtd_msg = e.formatted_message.as_deref().unwrap_or("");
    let mut lines = fmtd_msg.lines();
    let mut modified = String::new();
    let Some(l) = lines.next() else { return Some(modified) };
    if l.bytes().filter(|&b| b == b':').count() >= 3
        && (l.contains(['/', '\\']) || l.contains(".sol"))
    {
        // Old style: "path/to/file:LINE:COL: ErrorType: message"
        modified.push_str(&remap_old_style_line(macros, source, l)?);
        modified.push('\n');
    } else {
        // New style: first line is the error description, followed by a --> location block.
        modified.push_str(l);
        modified.push('\n');
        // Find and remap the single --> location line; pass preceding lines through unchanged.
        for line in lines.by_ref() {
            if line.contains(ARROW) {
                match remap_arrow_line(macros, source, line) {
                    ControlFlow::Break(None) => return None,
                    ControlFlow::Break(Some(remapped)) => {
                        modified.push_str(&remapped);
                        modified.push('\n');
                    }
                    ControlFlow::Continue(()) => {
                        modified.push_str(line);
                        modified.push('\n');
                    }
                }
                break;
            }
            modified.push_str(line);
            modified.push('\n');
        }
    }
    // Append remaining lines, remapping line numbers in framed source lines.
    for line in lines {
        modified.push_str(&remap_framed_line(macros, source, line));
        modified.push('\n');
    }
    Some(modified)
}

/// Remaps the line number in a `-->` source-location line.
///
/// Returns `Break(Some(remapped))` on success, `Break(None)` if the line falls in
/// macro-generated code, or `Continue(())` if the line cannot be parsed (use original).
fn remap_arrow_line(macros: &MacroRules, source: &Path, line: &str) -> ControlFlow<Option<String>> {
    let Some((beginning, loc)) = line.split_once(ARROW) else {
        return ControlFlow::Continue(());
    };
    let loc = loc.trim();
    // loc is now "path/to/file.sol:LINE:COL:" — strip exactly the one structural
    // trailing colon before splitting, then restore it in the output.
    let (without_trailing, trailing) = match loc.strip_suffix(':') {
        Some(s) => (s, ":"),
        None => (loc, ""),
    };
    let Some((rest, col)) = without_trailing.rsplit_once(':') else {
        return ControlFlow::Continue(());
    };
    let Some((path, line_str)) = rest.rsplit_once(':') else {
        return ControlFlow::Continue(());
    };
    let Ok(line_num) = line_str.parse::<isize>() else {
        return ControlFlow::Continue(());
    };
    let adjustments = macros.offset_adjustments.lock().unwrap();
    match adjustments.get_original_line(source, line_num) {
        None => ControlFlow::Break(None),
        Some(remapped) => {
            let remapped_col = remap_col(&adjustments, source, remapped, col);
            ControlFlow::Break(Some(format!(
                "{beginning}{ARROW} {path}:{remapped}:{remapped_col}{trailing}"
            )))
        }
    }
}

/// Remaps a column string against `original_line`, returning the remapped column as a string.
/// Falls back to the original text when it does not parse as a number.
fn remap_col(
    adjustments: &crate::offsets::OffsetAdjustment,
    source: &Path,
    original_line: usize,
    col: &str,
) -> String {
    match col.trim().parse::<isize>() {
        Ok(c) => adjustments.get_original_col(source, original_line, c).to_string(),
        Err(_) => col.to_string(),
    }
}

/// Remaps the line and column numbers in an old-style source-location line.
///
/// Returns `None` if the line falls in macro-generated code. Returns `Some` of the remapped
/// line on success, or `Some` of the original line if it cannot be parsed.
fn remap_old_style_line(macros: &MacroRules, source: &Path, line: &str) -> Option<String> {
    let source_str = source.to_string_lossy();
    let Some(rest) = line.strip_prefix(source_str.as_ref()) else { return Some(line.to_string()) };
    let Some(rest) = rest.strip_prefix(':') else { return Some(line.to_string()) };
    let Some((line_str, after_line)) = rest.split_once(':') else { return Some(line.to_string()) };
    let Ok(line_num) = line_str.parse::<isize>() else { return Some(line.to_string()) };
    let adjustments = macros.offset_adjustments.lock().unwrap();
    let remapped = adjustments.get_original_line(source, line_num)?;
    // `after_line` is "COL: ErrorType: message"; remap the leading column, keep the rest.
    let after_line = match after_line.split_once(':') {
        Some((col, tail)) => format!("{}:{tail}", remap_col(&adjustments, source, remapped, col)),
        None => after_line.to_string(),
    };
    Some(format!("{source_str}:{remapped}:{after_line}"))
}

/// Extracts the line number from a framed source line.
///
/// Expects a line of the form `LINE | ...` or `    | ...` (the separator/caret
/// lines have only whitespace before `|`). Returns `Some(LINE)` only when the
/// prefix before `|` is a non-empty decimal number.
fn framed_line(line: &str) -> Option<usize> {
    let (prefix, _rest) = line.split_once('|')?;
    let trimmed = prefix.trim();
    if trimmed.is_empty() { None } else { trimmed.parse().ok() }
}

/// Remaps the line number in a framed source line, if one is present.
///
/// For lines of the form `LINE | source code`, replaces `LINE` with the remapped number
/// while preserving the field width. Returns the line unchanged if it has no line number
/// (separator and caret lines like `   |` and `   | ^^^`), cannot be parsed, or falls in
/// macro-generated code.
fn remap_framed_line(macros: &MacroRules, source: &Path, line: &str) -> String {
    let Some(line_num) = framed_line(line) else { return line.to_string() };
    let Some(remapped) =
        macros.offset_adjustments.lock().unwrap().get_original_line(source, line_num as isize)
    else {
        return line.to_string();
    };
    let (prefix, rest) = line.split_once('|').unwrap();
    let field_width = prefix.trim_end().len();
    format!("{remapped:>field_width$} |{rest}")
}

#[cfg(test)]
mod tests {
    use solar::sema::{Gcx, hir::ContractKind};

    use crate::{Macro, PreprocessingData};

    const TEST_SOURCE_VALID: &str = r#"
    pragma solidity ^0.8.30;
    library Foo {
        function original() internal pure returns (uint256) { return 42; }
    }
    "#;

    const TEST_SOURCE_INVALID: &str = r#"
    pragma solidity ^0.8.30;
    library Foo {
        function original() internal pure returns (uint256) { return "42"; }
    }
    "#;

    fn make_macro(fail: bool) -> Macro {
        if fail { insert_foo_fail } else { insert_foo }
    }

    fn insert_foo(
        ctx: &Gcx,
        data: &mut PreprocessingData<'_>,
    ) -> foundry_compilers::error::Result<()> {
        insert_in_foo(ctx, data, false)
    }

    fn insert_foo_fail(
        ctx: &Gcx,
        data: &mut PreprocessingData<'_>,
    ) -> foundry_compilers::error::Result<()> {
        insert_in_foo(ctx, data, true)
    }

    fn insert_in_foo(
        ctx: &Gcx,
        data: &mut PreprocessingData<'_>,
        fail: bool,
    ) -> foundry_compilers::error::Result<()> {
        for contract in ctx.hir.contracts() {
            if contract.kind != ContractKind::Library || contract.name.name.as_str() != "Foo" {
                continue;
            }
            let Some(source) = ctx.sources.get(contract.source) else { continue };
            let Some(path) = source.file.name.as_real() else { continue };
            if !data.input.contains_key(path) {
                continue;
            }

            let start = (contract.span.lo().0 - source.file.start_pos.0) as usize;
            let after_open_brace = {
                let content = data.input.get(path).unwrap().content.as_str();
                let Some(rel) = content[start..].find('{') else { continue };
                start + rel + 1
            };

            let func = if fail {
                "\n    function inserted() internal pure returns (uint256) { return \"not a number\"; }\n"
            } else {
                "\n    function inserted() internal pure returns (uint256) { return 42; }\n"
            };
            data.insert(path, after_open_brace, func);
        }
        Ok(())
    }

    fn compile_with_macro(source: &str, fail: bool) -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let src_dir = dir.path().join("src");
        std::fs::create_dir(&src_dir)?;
        let sol_path = src_dir.join("Foo.sol");
        std::fs::write(&sol_path, source)?;

        let config = foundry_config::Config::with_root(dir.path());
        let project = config.project()?;

        let mut macros = crate::MacroRules::default();
        macros.rules.push(make_macro(fail));

        crate::project_compiler::ProjectCompiler {
            project_root: project.root().to_path_buf(),
            print_names: false,
            print_sizes: false,
            bail: true,
            ignore_eip_3860: false,
            files: vec![sol_path],
        }
        .compile(&project, macros)
        .map(|_| ())
    }

    #[test]
    fn test_macro_error_attributed_to_macro() {
        let err = compile_with_macro(TEST_SOURCE_VALID, true).unwrap_err().to_string();
        assert!(
            err.contains("error in macro-generated code"),
            "expected macro attribution, got:\n{err}"
        );
    }

    #[test]
    fn test_source_error_line_is_remapped() {
        let err = compile_with_macro(TEST_SOURCE_INVALID, false).unwrap_err().to_string();
        assert!(
            !err.contains("error in macro-generated code"),
            "error was incorrectly attributed to macro:\n{err}"
        );
        assert!(
            err.contains("Foo.sol:4:"),
            "expected error remapped to original line 4, got:\n{err}"
        );
    }

    #[test]
    fn test_arrow_column_is_remapped() {
        use std::path::Path;

        // A single-line "library" -> "contract" replacement on line 3 shifts every column after
        // it right by one. An error reported at expanded column 10 must map back to column 9.
        let source = Path::new("Foo.sol");
        let macros = crate::MacroRules::default();
        {
            let mut adj = macros.offset_adjustments.lock().unwrap();
            let src = "a\nb\nlibrary Foo {\n}";
            adj.record(source, 4, src, "contract", "library");
        }

        let line = " --> Foo.sol:3:10:";
        match super::remap_arrow_line(&macros, source, line) {
            std::ops::ControlFlow::Break(Some(remapped)) => {
                assert_eq!(remapped, " --> Foo.sol:3:9:");
            }
            other => panic!("expected remapped arrow line, got: {other:?}"),
        }
    }

    #[test]
    fn test_old_style_column_is_remapped() {
        use std::path::Path;

        let source = Path::new("Foo.sol");
        let macros = crate::MacroRules::default();
        {
            let mut adj = macros.offset_adjustments.lock().unwrap();
            let src = "a\nb\nlibrary Foo {\n}";
            adj.record(source, 4, src, "contract", "library");
        }

        let line = "Foo.sol:3:10: Error: something";
        let remapped = super::remap_old_style_line(&macros, source, line).expect("should remap");
        assert_eq!(remapped, "Foo.sol:3:9: Error: something");
    }
}
