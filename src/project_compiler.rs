// Copyright (C) 2026, Ava Labs, Inc. All rights reserved.
// See the file LICENSE for licensing terms.

use std::{collections::BTreeMap, path::PathBuf, time::Instant};

use foundry_common::{
    TestFunctionExt,
    compile::{ContractInfo, SizeReport, with_compilation_reporter},
    sh_println, shell,
};
use foundry_compilers::{
    Artifact, Compiler, Project, ProjectCompileOutput,
    artifacts::{BytecodeObject, Contract, Source},
    multi::{MultiCompiler, MultiCompilerError},
};

use crate::MacroRules;

/// https://eips.ethereum.org/EIPS/eip-170
const CONTRACT_RUNTIME_SIZE_LIMIT: usize = 24576;

/// https://eips.ethereum.org/EIPS/eip-3860
const CONTRACT_INITCODE_SIZE_LIMIT: usize = 49152;

/// Keeps track of the project being compiled. It is responsible
/// for pre-processing, organizing files, printing out diagnostics/info,
/// and passing sources to the compiler. It is nearly identical to the
/// `ProjectCompiler` found in `foundry-common::compile`.
pub(crate) struct ProjectCompiler {
    pub project_root: PathBuf,
    /// Whether to also print contract names.
    pub print_names: bool,
    /// Whether to also print contract sizes.
    pub print_sizes: bool,
    /// Whether to bail on compiler errors.
    pub bail: bool,
    /// Whether to ignore the contract initcode size limit introduced by EIP-3860.
    pub ignore_eip_3860: bool,
    /// Extra files to include, that are not necessarily in the project's source directory.
    pub files: Vec<PathBuf>,
}

impl ProjectCompiler {
    pub fn compile(
        mut self,
        project: &Project<MultiCompiler>,
        preprocessor: MacroRules,
    ) -> eyre::Result<ProjectCompileOutput<MultiCompiler>> {
        if !project.paths.has_input_files() && self.files.is_empty() {
            sh_println!("Nothing to compile")?;
            std::process::exit(0);
        }

        // Taking is fine since we don't need these in `compile_with`.
        let files = std::mem::take(&mut self.files);
        // Clone the Arc before moving preprocessor into the closure so we can remap
        // error line numbers after compilation.
        let macros = preprocessor.clone();
        self.compile_with(|| {
            let sources = if !files.is_empty() {
                Source::read_all(files)?
            } else {
                project.paths.read_input_files()?
            };

            let mut compiler =
                foundry_compilers::project::ProjectCompiler::with_sources(project, sources)?;

            compiler = compiler.with_preprocessor(preprocessor);
            let mut output: ProjectCompileOutput<MultiCompiler> =
                compiler.compile().map_err(eyre::Error::from)?;

            // Remap solc error line numbers from expanded source back to original source.
            for error in output.output_mut().errors.iter_mut() {
                if let MultiCompilerError::Solc(e) = error {
                    crate::errors::correct_fmt_msg(&macros, e);
                }
            }

            Ok(output)
        })
    }

    // Compiles the project with the given closure
    fn compile_with<C: Compiler<CompilerContract = Contract>, F>(
        self,
        f: F,
    ) -> eyre::Result<ProjectCompileOutput<C>>
    where
        F: FnOnce() -> eyre::Result<ProjectCompileOutput<C>>,
    {
        let bail = self.bail;
        let output = with_compilation_reporter(false, Some(self.project_root.clone()), || {
            tracing::debug!("compiling project");

            let timer = Instant::now();
            let r = f();
            let elapsed = timer.elapsed();

            tracing::debug!("finished compiling in {:.3}s", elapsed.as_secs_f64());
            r
        })?;

        if bail && output.has_compiler_errors() {
            #[cfg(test)]
            crate::errors::TEST_COMPILER_OUTPUT.lock().unwrap().push(format!("{output}"));
            eyre::bail!("{output}")
        }

        if !shell::is_json() {
            if output.is_unchanged() {
                sh_println!("No files changed, compilation skipped")?;
            } else {
                // print the compiler output / warnings
                sh_println!("{output}")?;
            }
            self.handle_output(&output)?;
        }

        Ok(output)
    }

    /// If configured, this will print sizes or names
    fn handle_output<C: Compiler<CompilerContract = Contract>>(
        &self,
        output: &ProjectCompileOutput<C>,
    ) -> eyre::Result<()> {
        let print_names = self.print_names;
        let print_sizes = self.print_sizes;

        // print any sizes or names
        if print_names {
            let mut artifacts: BTreeMap<_, Vec<_>> = BTreeMap::new();
            for (name, (_, version)) in output.versioned_artifacts() {
                artifacts.entry(version).or_default().push(name);
            }

            if shell::is_json() {
                sh_println!("{}", serde_json::to_string(&artifacts).unwrap())?;
            } else {
                for (version, names) in artifacts {
                    sh_println!(
                        "  compiler version: {}.{}.{}",
                        version.major,
                        version.minor,
                        version.patch
                    )?;
                    for name in names {
                        sh_println!("    - {name}")?;
                    }
                }
            }
        }

        if print_sizes {
            // add extra newline if names were already printed
            if print_names && !shell::is_json() {
                sh_println!()?;
            }

            let mut size_report = SizeReport { contracts: BTreeMap::new() };

            let mut artifacts: BTreeMap<String, Vec<_>> = BTreeMap::new();
            for (id, artifact) in output.artifact_ids().filter(|(id, _)| {
                // filter out forge-std specific contracts
                !id.source.to_string_lossy().contains("/forge-std/src/")
            }) {
                artifacts.entry(id.name.clone()).or_default().push((id.source.clone(), artifact));
            }

            for (name, artifact_list) in artifacts {
                for (path, artifact) in &artifact_list {
                    let runtime_size = contract_size(*artifact, false).unwrap_or_default();
                    let init_size = contract_size(*artifact, true).unwrap_or_default();

                    let is_dev_contract = artifact
                        .abi
                        .as_ref()
                        .map(|abi| {
                            abi.functions().any(|f| {
                                f.test_function_kind().is_known()
                                    || matches!(f.name.as_str(), "IS_TEST" | "IS_SCRIPT")
                            })
                        })
                        .unwrap_or(false);

                    let unique_name = if artifact_list.len() > 1 {
                        format!(
                            "{} ({})",
                            name,
                            path.strip_prefix(&self.project_root).unwrap_or(path).display()
                        )
                    } else {
                        name.clone()
                    };

                    size_report.contracts.insert(
                        unique_name,
                        ContractInfo { runtime_size, init_size, is_dev_contract },
                    );
                }
            }

            sh_println!("{size_report}")?;

            eyre::ensure!(
                !size_report.exceeds_runtime_size_limit(),
                "some contracts exceed the runtime size limit \
                 (EIP-170: {CONTRACT_RUNTIME_SIZE_LIMIT} bytes)"
            );
            // Check size limits only if not ignoring EIP-3860
            eyre::ensure!(
                self.ignore_eip_3860 || !size_report.exceeds_initcode_size_limit(),
                "some contracts exceed the initcode size limit \
                 (EIP-3860: {CONTRACT_INITCODE_SIZE_LIMIT} bytes)"
            );
        }

        Ok(())
    }
}

/// Returns the deployed or init size of the contract.
fn contract_size<T: Artifact>(artifact: &T, initcode: bool) -> Option<usize> {
    let bytecode = if initcode {
        artifact.get_bytecode_object()?
    } else {
        artifact.get_deployed_bytecode_object()?
    };

    let size = match bytecode.as_ref() {
        BytecodeObject::Bytecode(bytes) => bytes.len(),
        BytecodeObject::Unlinked(unlinked) => {
            // we don't need to account for placeholders here, because library placeholders take up
            // 40 characters: `__$<library hash>$__` which is the same as a 20byte address in hex.
            let mut size = unlinked.len();
            if unlinked.starts_with("0x") {
                size -= 2;
            }
            // hex -> bytes
            size / 2
        }
    };

    Some(size)
}
