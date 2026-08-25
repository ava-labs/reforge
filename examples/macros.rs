// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.

use std::collections::HashMap;

use foundry_compilers::error::SolcError;
use reforge::{MacroRules, PreprocessingData, file_offset, get_comment};
use solar::sema::{Gcx, hir::ContractKind};

fn main() -> eyre::Result<()> {
    let mut macros = MacroRules::default();
    macros.rules.push(do_nothing);
    macros.rules.push(print_name);
    macros.rules.push(get_id_or_revert);
    macros.rules.push(make_libraries_contracts);
    macros.rules.push(make_func_public);
    macros.run()
}

fn do_nothing(_: &Gcx, _: &mut PreprocessingData<'_>) -> foundry_compilers::error::Result<()> {
    Ok(())
}

/// A macro that adds a function for each struct definition that prints the struct name.
/// The function is injected into the pre-existing `{name}Library` library.
fn print_name(ctx: &Gcx, data: &mut PreprocessingData<'_>) -> foundry_compilers::error::Result<()> {
    // Collect (offset, injected_text) per file path.
    let mut insertions: HashMap<std::path::PathBuf, Vec<(usize, String)>> = HashMap::new();

    for struct_def in ctx.hir.structs() {
        let Some(source) = ctx.sources.get(struct_def.source) else {
            continue;
        };
        let Some(path) = source.file.name.as_real() else {
            continue;
        };

        if !data.input.contains_key(path) {
            continue;
        }

        let name = struct_def.name.name;
        let library_name = format!("{name}Library");

        // Find the pre-existing library named `Library{name}` in the same source file.
        let Some(library) = ctx.hir.contracts().find(|c| {
            c.source == struct_def.source
                && c.kind == ContractKind::Library
                && c.name.name.as_str() == library_name
        }) else {
            tracing::debug!("No library named {library_name} found, skipping struct {name}");
            continue;
        };

        // Insert just before the closing `}` of the library body.
        let close_brace_offset =
            file_offset(library.span.hi().0, source.file.start_pos.0).saturating_sub(1);
        let func = format!(
            "\n    function print{name}() public pure returns (string memory) {{ return \"{name}\"; }}\n"
        );
        tracing::debug!("Injecting function for struct {name} into {library_name}");
        insertions.entry(path.to_path_buf()).or_default().push((close_brace_offset, func));
    }

    for (path, inserts) in insertions {
        for (offset, text) in inserts {
            data.entry(&path, &text).insert(offset);
        }
    }
    Ok(())
}

/// A macro that adds a function to every struct that returns its ID if it has the field and reverts
/// otherwise.
fn get_id_or_revert(
    ctx: &Gcx,
    data: &mut PreprocessingData<'_>,
) -> foundry_compilers::error::Result<()> {
    // Collect (offset, injected_text) per file path.
    let mut insertions: HashMap<std::path::PathBuf, Vec<(usize, String)>> = HashMap::new();
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"#\[derive\(get_id_or_revert\(contract=(\w+)\)\)]").unwrap()
    });

    for struct_def in ctx.hir.structs() {
        let Some(source) = ctx.sources.get(struct_def.source) else {
            continue;
        };
        let Some(path) = source.file.name.as_real() else {
            continue;
        };
        if !data.input.contains_key(path) {
            continue;
        }

        let name = struct_def.name.name;

        let contract_name = if let Some(comment_block) =
            get_comment(ctx, struct_def.source, struct_def.span, data)
        {
            if let Some(caps) = re.captures(&comment_block) {
                caps[1].to_string()
            } else {
                continue;
            }
        } else {
            continue;
        };
        let has_id_field = struct_def
            .fields
            .iter()
            .any(|&var_id| ctx.hir.variable(var_id).name.is_some_and(|n| n.name.as_str() == "ID"));

        // Find the pre-existing contract named `contract_name` in the same source file.
        let Some(library) = ctx
            .hir
            .contracts()
            .find(|c| c.source == struct_def.source && c.name.name.as_str() == contract_name)
        else {
            return Err(SolcError::msg(format!(
                "No library named {contract_name} found, macro expansion failed."
            )));
        };

        // Insert just before the closing `}` of the library body.
        let close_brace_offset =
            file_offset(library.span.hi().0, source.file.start_pos.0).saturating_sub(1);
        let func = if has_id_field {
            format!(
                "\n    function getId{name}({name} memory obj) public pure returns (uint32) {{ return obj.ID; }}\n",
            )
        } else {
            format!(
                "\n    function getId{name}({name} memory) public pure returns (uint32) {{ revert(\"{name} has no field ID\"); }}\n",
            )
        };
        insertions.entry(path.to_path_buf()).or_default().push((close_brace_offset, func));
    }

    // In this example, we use the provided helper methods to insert the text and handle the
    // offset adjustments automatically.
    for (path, inserts) in insertions {
        for (offset, text) in &inserts {
            data.entry(&path, text).insert(*offset);
        }
    }

    Ok(())
}

/// A macro that changes all libraries into contracts if their doc comment contains
/// #[derive(promote)].
fn make_libraries_contracts(
    ctx: &Gcx,
    data: &mut PreprocessingData<'_>,
) -> foundry_compilers::error::Result<()> {
    for lib in ctx.hir.contracts().filter(|c| c.kind == ContractKind::Library) {
        let Some(source) = ctx.sources.get(lib.source) else {
            continue;
        };
        let Some(path) = source.file.name.as_real() else {
            continue;
        };
        let Some(comment_block) = get_comment(ctx, lib.source, lib.span, data) else {
            continue;
        };

        if comment_block.contains("#[derive(promote)]") {
            let lib_offset = file_offset(lib.span.lo().0, source.file.start_pos.0);
            data.entry(path, "contract")
                .replace(lib_offset..lib_offset.saturating_add("library".len()));
        }
    }
    Ok(())
}

/// A macro that changes the visibility of functions to public if their doc comment contains
/// #[derive(public)].
fn make_func_public(
    ctx: &Gcx,
    data: &mut PreprocessingData<'_>,
) -> foundry_compilers::error::Result<()> {
    for func in ctx.hir.functions() {
        let Some(source) = ctx.sources.get(func.source) else {
            continue;
        };
        let Some(path) = source.file.name.as_real() else {
            continue;
        };
        let Some(comment_block) = get_comment(ctx, func.source, func.span, data) else {
            continue;
        };

        // Use convenience methods to compute offset adjustments automatically.
        if comment_block.contains("#[derive(public)]") {
            let original_offset = file_offset(func.span.lo().0, source.file.start_pos.0);
            let func_offset = data.adjusted_offset(path, original_offset);
            let Some(src) = data.input.get(path) else {
                continue;
            };
            let visibility_keyword = func.visibility.to_str();
            // NOTE: `find` may match a false positive if the visibility keyword appears in a
            // parameter name or string literal before the actual modifier. If this becomes an
            // issue, narrow the search to the signature only (i.e. the text before the `{`).
            let modifier_local_offset = src.content[func_offset..]
                .find(visibility_keyword)
                .ok_or_else(|| SolcError::msg(
                    format!("could not find visibility modifier '{visibility_keyword}' in function at offset {func_offset}")
                ))?;
            let original_modifier_start = original_offset.saturating_add(modifier_local_offset);
            data.entry(path, "public").replace(
                original_modifier_start
                    ..original_modifier_start.saturating_add(visibility_keyword.len()),
            );
        }
    }
    Ok(())
}
