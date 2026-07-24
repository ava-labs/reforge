// Copyright (C) 2026, Ava Labs, Inc. All rights reserved.
// See the file LICENSE for licensing terms.

use forge::cmd::{install, test::TestArgs};
use foundry_cli::utils::LoadConfig;
use foundry_common::shell;

use crate::project_compiler::ProjectCompiler;

pub async fn test(mut test_args: TestArgs, macros: crate::MacroRules) -> eyre::Result<()> {
    let silent = test_args.junit || shell::is_json();
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

    let output = compiler.compile(&project, macros)?;

    let outcome =
        test_args.run_tests(&project.paths.root, config, evm_opts, &output, &filter, false).await?;

    outcome.ensure_ok(silent)
}
