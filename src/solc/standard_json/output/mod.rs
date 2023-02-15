//!
//! The `solc --standard-json` output representation.
//!

pub mod contract;
pub mod error;
pub mod source;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::evmla::assembly::instruction::Instruction;
use crate::evmla::assembly::Assembly;
use crate::project::contract::source::Source as ProjectContractSource;
use crate::project::contract::Contract as ProjectContract;
use crate::project::Project;
use crate::solc::pipeline::Pipeline as SolcPipeline;
use crate::yul::lexer::Lexer;
use crate::yul::parser::statement::object::Object;

use self::contract::Contract;
use self::error::Error as SolcStandardJsonOutputError;
use self::source::Source;

///
/// The `solc --standard-json` output representation.
///
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Output {
    /// The file-contract hashmap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contracts: Option<BTreeMap<String, BTreeMap<String, Contract>>>,
    /// The source code mapping data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<BTreeMap<String, Source>>,
    /// The compilation errors and warnings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<SolcStandardJsonOutputError>>,
    /// The `solc` compiler version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The `solc` compiler long version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_version: Option<String>,
    /// The `zksolc` compiler version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zk_version: Option<String>,
}

impl Output {
    ///
    /// Converts the `solc` JSON output into a convenient project representation.
    ///
    pub fn try_to_project(
        &mut self,
        libraries: BTreeMap<String, BTreeMap<String, String>>,
        pipeline: SolcPipeline,
        version: &semver::Version,
        debug_config: Option<&compiler_llvm_context::DebugConfig>,
    ) -> anyhow::Result<Project> {
        if let SolcPipeline::EVMLA = pipeline {
            self.preprocess_dependencies()?;
        }

        let files = match self.contracts.as_mut() {
            Some(files) => files,
            None => {
                anyhow::bail!(
                    "{}",
                    self.errors
                        .as_ref()
                        .map(|errors| serde_json::to_string_pretty(errors).expect("Always valid"))
                        .unwrap_or_else(|| "Unknown project assembling error".to_owned())
                );
            }
        };
        let mut project_contracts = BTreeMap::new();

        for (path, contracts) in files.iter_mut() {
            for (name, contract) in contracts.iter_mut() {
                let full_path = format!("{path}:{name}");

                let source = match pipeline {
                    SolcPipeline::Yul => {
                        let ir_optimized = match contract.ir_optimized.take() {
                            Some(ir_optimized) => ir_optimized,
                            None => continue,
                        };
                        if ir_optimized.is_empty() {
                            continue;
                        }

                        if let Some(debug_config) = debug_config {
                            debug_config.dump_yul(full_path.as_str(), ir_optimized.as_str())?;
                        }

                        let mut lexer = Lexer::new(ir_optimized.clone());
                        let object = Object::parse(&mut lexer, None).map_err(|error| {
                            anyhow::anyhow!("Contract `{}` parsing error: {:?}", full_path, error)
                        })?;

                        ProjectContractSource::new_yul(ir_optimized, object)
                    }
                    SolcPipeline::EVMLA => {
                        let assembly =
                            match contract.evm.as_ref().and_then(|evm| evm.assembly.as_ref()) {
                                Some(assembly) => assembly.to_owned(),
                                None => continue,
                            };

                        ProjectContractSource::new_evmla(assembly)
                    }
                };

                let project_contract =
                    ProjectContract::new(full_path.clone(), source, Some(contract));
                project_contracts.insert(full_path, project_contract);
            }
        }

        Ok(Project::new(
            version.to_owned(),
            project_contracts,
            libraries,
        ))
    }

    ///
    /// Traverses the AST and returns the list of additional errors and warnings.
    ///
    pub fn preprocess_ast(&mut self) -> anyhow::Result<()> {
        let sources = match self.sources.as_ref() {
            Some(sources) => sources,
            None => return Ok(()),
        };

        let mut messages = Vec::new();
        for (path, source) in sources.iter() {
            if let Some(ast) = source.ast.as_ref() {
                let mut zkevm_messages = ast.get_messages()?;
                for message in zkevm_messages.iter_mut() {
                    message.push_contract_path(path.as_str());
                }
                messages.extend(zkevm_messages);
            }
        }

        self.errors = match self.errors.take() {
            Some(mut errors) => {
                errors.extend(messages);
                Some(errors)
            }
            None => Some(messages),
        };

        Ok(())
    }

    ///
    /// The pass, which replaces with dependency indexes with actual data.
    ///
    fn preprocess_dependencies(&mut self) -> anyhow::Result<()> {
        let files = match self.contracts.as_mut() {
            Some(files) => files,
            None => return Ok(()),
        };
        let mut hash_path_mapping = BTreeMap::new();

        for (path, contracts) in files.iter() {
            for (name, contract) in contracts.iter() {
                let full_path = format!("{path}:{name}");
                let hash = match contract
                    .evm
                    .as_ref()
                    .and_then(|evm| evm.assembly.as_ref())
                    .map(|assembly| assembly.keccak256())
                {
                    Some(hash) => hash,
                    None => continue,
                };

                hash_path_mapping.insert(hash, full_path);
            }
        }

        for (path, contracts) in files.iter_mut() {
            for (name, contract) in contracts.iter_mut() {
                let assembly = match contract.evm.as_mut().and_then(|evm| evm.assembly.as_mut()) {
                    Some(assembly) => assembly,
                    None => continue,
                };

                let full_path = format!("{path}:{name}");
                Self::preprocess_dependency_level(
                    full_path.as_str(),
                    assembly,
                    &hash_path_mapping,
                )?;
            }
        }

        Ok(())
    }

    ///
    /// Preprocesses an assembly JSON structure dependency data map.
    ///
    fn preprocess_dependency_level(
        full_path: &str,
        assembly: &mut Assembly,
        hash_path_mapping: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        assembly.set_full_path(full_path.to_owned());

        let deploy_code_index_path_mapping =
            assembly.deploy_dependencies_pass(full_path, hash_path_mapping)?;
        if let Some(deploy_code_instructions) = assembly.code.as_deref_mut() {
            Instruction::replace_data_aliases(
                deploy_code_instructions,
                &deploy_code_index_path_mapping,
            )?;
        };

        let runtime_code_index_path_mapping =
            assembly.runtime_dependencies_pass(full_path, hash_path_mapping)?;
        if let Some(runtime_code_instructions) = assembly
            .data
            .as_mut()
            .and_then(|data_map| data_map.get_mut("0"))
            .and_then(|data| data.get_assembly_mut())
            .and_then(|assembly| assembly.code.as_deref_mut())
        {
            Instruction::replace_data_aliases(
                runtime_code_instructions,
                &runtime_code_index_path_mapping,
            )?;
        }

        Ok(())
    }
}