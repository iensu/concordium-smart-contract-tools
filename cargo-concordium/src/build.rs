use ansi_term::{Color, Style};
use anyhow::Context;
use base64::{engine::general_purpose, Engine as _};
use cargo_metadata::MetadataCommand;
use concordium_contracts_common::{
    schema::{
        ContractV0, ContractV1, ContractV2, ContractV3, FunctionV1, FunctionV2,
        VersionedModuleSchema,
    },
    *,
};
use concordium_smart_contract_engine::{
    utils::{self, WasmVersion},
    v0, v1, ExecResult,
};
use concordium_wasm::{
    output::{write_custom_section, Output},
    parse::parse_skeleton,
    types::{CustomSection, ExportDescription, Module},
    utils::strip,
    validate::validate_module,
};
use rand::{thread_rng, Rng};
use serde_json::Value;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Encode all base64 strings using the standard alphabet and no padding.
/// Padding is not useful since strings are just put as JSON strings.
const ENCODER: base64::engine::GeneralPurpose = general_purpose::STANDARD_NO_PAD;

fn to_snake_case(string: &str) -> String { string.to_lowercase().replace('-', "_") }

#[derive(Debug, Clone, Copy)]
pub enum SchemaBuildOptions {
    DoNotBuild,
    JustBuild,
    BuildAndEmbed,
}

impl SchemaBuildOptions {
    /// Return whether the schema should be built.
    pub fn build(self) -> bool {
        matches!(
            self,
            SchemaBuildOptions::JustBuild | SchemaBuildOptions::BuildAndEmbed
        )
    }

    /// Return whether the schema should be embedded.
    pub fn embed(self) -> bool { matches!(self, SchemaBuildOptions::BuildAndEmbed) }
}

/// Build a contract and its schema.
/// If build_schema is set then the return value will contain the schema of the
/// version specified.
pub fn build_contract(
    version: WasmVersion,
    build_schema: SchemaBuildOptions,
    out: Option<PathBuf>,
    cargo_args: &[String],
) -> anyhow::Result<(usize, Option<schema::VersionedModuleSchema>)> {
    #[allow(unused_assignments)]
    // This assignment is not actually unused. It is used via the custom_section which retains a
    // reference to this vector, which is why it has to be here. This is a bit ugly, but not as
    // ugly as alternatives.
    let mut schema_bytes = Vec::new();
    // if none do not build. If Some(true) then embed, otherwise
    // just build and return
    let schema = match version {
        WasmVersion::V0 => {
            if build_schema.build() {
                let schema = build_contract_schema(cargo_args, utils::generate_contract_schema_v0)
                    .context("Could not build module schema.")?;
                if build_schema.embed() {
                    schema_bytes = to_bytes(&schema);
                    let custom_section = CustomSection {
                        name:     "concordium-schema".into(),
                        contents: &schema_bytes,
                    };
                    Some((Some(custom_section), schema))
                } else {
                    Some((None, schema))
                }
            } else {
                None
            }
        }
        WasmVersion::V1 => {
            if build_schema.build() {
                let schema = build_contract_schema(cargo_args, utils::generate_contract_schema_v3)
                    .context("Could not build module schema.")?;
                if build_schema.embed() {
                    schema_bytes = to_bytes(&schema);
                    let custom_section = CustomSection {
                        name:     "concordium-schema".into(),
                        contents: &schema_bytes,
                    };
                    Some((Some(custom_section), schema))
                } else {
                    Some((None, schema))
                }
            } else {
                None
            }
        }
    };

    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("Could not access cargo metadata.")?;

    let package = metadata
        .root_package()
        .context("Unable to determine package.")?;

    let target_dir = format!("{}/concordium", metadata.target_directory);

    let result = Command::new("cargo")
        .arg("build")
        .args(&["--target", "wasm32-unknown-unknown"])
        .args(&["--release"])
        .args(&["--target-dir", target_dir.as_str()])
        .args(cargo_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context("Could not use cargo build.")?;

    if !result.status.success() {
        anyhow::bail!("Compilation failed.")
    }

    let filename = format!(
        "{}/wasm32-unknown-unknown/release/{}.wasm",
        target_dir,
        to_snake_case(package.name.as_str())
    );

    let wasm = fs::read(&filename).context("Could not read cargo build Wasm output.")?;

    let mut skeleton =
        parse_skeleton(&wasm).context("Could not parse the skeleton of the module.")?;

    // Remove all custom sections to reduce the size of the module
    strip(&mut skeleton);
    match version {
        WasmVersion::V0 => {
            let module = validate_module(&v0::ConcordiumAllowedImports, &skeleton)
                .context("Could not validate resulting smart contract module as a V0 contract.")?;
            check_exports(&module, WasmVersion::V0)
                .context("Contract and entrypoint validation failed for a V0 contract.")?;
            module
        }
        WasmVersion::V1 => {
            let module = validate_module(
                &v1::ConcordiumAllowedImports {
                    support_upgrade: true,
                },
                &skeleton,
            )
            .context("Could not validate resulting smart contract module as a V1 contract.")?;
            check_exports(&module, WasmVersion::V1)
                .context("Contract and entrypoint validation failed for a V1 contract.")?;
            module
        }
    };

    // We output a versioned module that can be directly deployed to the chain,
    // i.e., the exact data that needs to go into the transaction. This starts with
    // the version number in big endian. The remaining 4 bytes are a placeholder for
    // length.
    let mut output_bytes = match version {
        WasmVersion::V0 => vec![0, 0, 0, 0, 0, 0, 0, 0],
        WasmVersion::V1 => vec![0, 0, 0, 1, 0, 0, 0, 0],
    };
    // Embed schema custom section
    skeleton.output(&mut output_bytes)?;
    let return_schema = if let Some((custom_section, schema)) = schema {
        if let Some(custom_section) = custom_section {
            write_custom_section(&mut output_bytes, &custom_section)?;
        }
        Some(schema)
    } else {
        None
    };
    // write the size of the actual module to conform to serialization expected on
    // the chain
    let data_size = (output_bytes.len() - 8) as u32;
    (&mut output_bytes[4..8]).copy_from_slice(&data_size.to_be_bytes());

    let out_filename = match out {
        Some(out) => {
            // A path and a filename need to be provided when using the `--out` flag.
            if out.file_name().is_none() || out.is_dir() {
                anyhow::bail!(
                    "The `--out` flag requires a path and a filename (expected input: \
                     `./my/path/my_smart_contract.wasm.v1`)"
                );
            }
            out
        }
        None => {
            let extension = match version {
                WasmVersion::V0 => "v0",
                WasmVersion::V1 => "v1",
            };
            PathBuf::from(format!("{}.{}", filename, extension))
        }
    };

    let total_module_len = output_bytes.len();
    if let Some(out_dir) = out_filename.parent() {
        fs::create_dir_all(out_dir)
            .context("Unable to create directory for the resulting smart contract module.")?;
    }
    fs::write(out_filename, output_bytes)?;
    Ok((total_module_len, return_schema))
}

/// Check that exports of module conform to the specification so that they will
/// be accepted by the chain.
fn check_exports(module: &Module, version: WasmVersion) -> anyhow::Result<()> {
    // collect contracts in the module.
    let mut contracts = BTreeSet::new();
    let mut methods = BTreeMap::<_, BTreeSet<OwnedEntrypointName>>::new();
    for export in &module.export.exports {
        if let ExportDescription::Func { .. } = export.description {
            if let Ok(cn) = ContractName::new(export.name.as_ref()) {
                contracts.insert(cn.contract_name());
            } else if let Ok(rn) = ReceiveName::new(export.name.as_ref()) {
                methods
                    .entry(rn.contract_name())
                    .or_insert_with(BTreeSet::new)
                    .insert(rn.entrypoint_name().into());
            } else {
                // for V0 contracts we do not allow any other functions.
                match version {
                    WasmVersion::V0 => anyhow::bail!(
                        "The module has '{}' as an exposed function, which is neither a valid \
                         init or receive method.\nV0 contracts do not allow any exported \
                         functions that are neither init or receive methods.\n",
                        export.name.as_ref()
                    ),
                    WasmVersion::V1 => (),
                }
            }
        }
    }
    for (cn, _ens) in methods {
        if let Some(closest) = find_closest(contracts.iter().copied(), cn) {
            if closest.is_empty() {
                anyhow::bail!(
                    "An entrypoint is declared for a contract '{}', but no contracts exist in the \
                     module.",
                    cn
                );
            } else if closest.len() == 1 {
                anyhow::bail!(
                    "An entrypoint is declared for a contract '{}', but such a contract does not \
                     exist in the module.\nPerhaps you meant '{}'?",
                    cn,
                    closest[0]
                );
            } else {
                let list = closest
                    .into_iter()
                    .map(|x| format!("'{}'", x))
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "An entrypoint is declared for a contract '{}', but such a contract does not \
                     exist in the module.\nPerhaps you meant one of [{}].",
                    cn,
                    list
                );
            }
        }
    }
    Ok(())
}

/// Find the string closest to the list of strings. If an exact match is found
/// return `None`, otherwise return `Some` with a list of strings that are
/// closest according to the [optimal string alignment metric](https://en.wikipedia.org/wiki/Damerau%E2%80%93Levenshtein_distance distance).
fn find_closest<'a>(
    list: impl IntoIterator<Item = &'a str>,
    goal: &'a str,
) -> Option<Vec<&'a str>> {
    let mut out = Vec::new();
    let mut least = usize::MAX;
    for cn in list.into_iter() {
        let dist = strsim::osa_distance(cn, goal);
        if dist == 0 {
            return None;
        }
        match dist.cmp(&least) {
            Ordering::Less => {
                out.clear();
                out.push(cn);
                least = dist;
            }
            Ordering::Equal => {
                out.push(cn);
            }
            Ordering::Greater => {
                // do nothing since this candidate is not useful
            }
        }
    }
    Some(out)
}

/// Generates the contract schema by compiling with the 'build-schema' feature
/// Then extracts the schema from the schema build
pub fn build_contract_schema<A>(
    cargo_args: &[String],
    generate_schema: impl FnOnce(&[u8]) -> ExecResult<A>,
) -> anyhow::Result<A> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("Could not access cargo metadata.")?;

    let package = metadata
        .root_package()
        .context("Unable to determine package.")?;

    let target_dir = format!("{}/concordium", metadata.target_directory);

    let result = Command::new("cargo")
        .arg("build")
        .args(&["--target", "wasm32-unknown-unknown"])
        .arg("--release")
        .args(&["--features", "concordium-std/build-schema"])
        .args(&["--target-dir", target_dir.as_str()])
        .args(cargo_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context("Could not run cargo build.")?;

    if !result.status.success() {
        anyhow::bail!("Compilation failed.");
    }

    let filename = format!(
        "{}/wasm32-unknown-unknown/release/{}.wasm",
        target_dir,
        to_snake_case(package.name.as_str())
    );

    let wasm =
        std::fs::read(filename).context("Could not read cargo build contract schema output.")?;
    let schema =
        generate_schema(&wasm).context("Could not generate module schema from Wasm module.")?;
    Ok(schema)
}

/// Create a new Concordium smart contract project from a template, or there
/// are runtime exceptions that are not expected then this function returns
/// `Err(...)`.
pub fn init_concordium_project(path: impl AsRef<Path>) -> anyhow::Result<()> {
    let path = path.as_ref();

    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };

    if let Err(which::Error::CannotFindBinaryPath) = which::which("cargo-generate") {
        anyhow::bail!(
            "`cargo concordium init` requires `cargo-generate` which does not appear to be \
             installed. You can install it by running `cargo install --locked cargo-generate`"
        )
    }

    let result = Command::new("cargo")
        .arg("generate")
        .args(&[
            "--git",
            "https://github.com/Concordium/concordium-rust-smart-contracts",
            "templates",
        ])
        .args(&["--destination", absolute_path.to_str().unwrap()])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .context("Could not obtain the template.")?;

    anyhow::ensure!(
        result.status.success(),
        "Could not use the template to initialize the project."
    );

    println!("Created the smart contract template.");
    Ok(())
}

/// Write the provided JSON value to the file inside the `root` directory.
/// The file is named after contract_name, except if contract_name contains
/// unsuitable characters. Then the counter is used to name the file.
fn write_schema_json(
    root: &Path,
    contract_name: &str,
    counter: usize,
    mut schema_json: Value,
) -> anyhow::Result<()> {
    schema_json["contractName"] = contract_name.into();
    // save the schema JSON representation into the file
    let mut out_path = root.to_path_buf();

    // make sure the path is valid on all platforms
    let file_name = if contract_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_[]{}".contains(c))
    {
        contract_name.to_owned() + "_schema.json"
    } else {
        format!("contract-schema_{}.json", counter)
    };

    out_path.push(file_name);

    println!(
        "   Writing JSON schema for {} to {}.",
        contract_name,
        out_path.display()
    );
    if let Some(out_dir) = out_path.parent() {
        fs::create_dir_all(out_dir)
            .context("Unable to create directory for the resulting JSON schemas.")?;
    }
    std::fs::write(out_path, serde_json::to_string_pretty(&schema_json)?)
        .context("Unable to write schema output.")?;
    Ok(())
}

/// Write the provided schema in its base64 representation to a file or print it
/// to the console if `out` is None.
pub fn write_schema_base64(
    out: Option<PathBuf>,
    schema: &VersionedModuleSchema,
) -> anyhow::Result<()> {
    let schema_base64 = ENCODER.encode(to_bytes(schema));

    match out {
        // writing base64 schema to file
        Some(out) => {
            println!("   Writing base64 schema to {}.", out.display());
            if let Some(out_dir) = out.parent() {
                fs::create_dir_all(out_dir)
                    .context("Unable to create directory for the resulting base64 schema.")?;
            }
            // save the schema base64 representation to the file
            std::fs::write(out, schema_base64).context("Unable to write schema output.")?;
        }
        // printing base64 schema to console
        None => {
            println!(
                "   The base64 conversion of the schema is:\n{}",
                schema_base64
            )
        }
    }

    Ok(())
}

/// Converts the ContractV0 schema of the given contract_name to JSON and writes
/// it to a file named after the smart contract name at the specified location.
pub fn write_json_schema_to_file_v0(
    path_of_out: &Path,
    contract_name: &str,
    contract_counter: usize,
    contract_schema: &ContractV0,
) -> anyhow::Result<()> {
    // create empty schema_json
    let mut schema_json: Value = Value::Object(serde_json::Map::new());

    // add init schema
    if let Some(init_schema) = &contract_schema.init {
        schema_json["init"] = type_to_json(init_schema);
    }

    // add state schema
    if let Some(state_schema) = &contract_schema.state {
        schema_json["state"] = type_to_json(state_schema);
    }

    // add receive entrypoints
    if !contract_schema.receive.is_empty() {
        // create empty entrypoints
        let mut entrypoints: Value = Value::Object(serde_json::Map::new());

        // iterate through the entrypoints and add their schemas
        for (method_name, receive_schema) in contract_schema.receive.iter() {
            // add `method_name` entrypoint
            entrypoints[method_name] = type_to_json(receive_schema);
        }

        // add all receive entrypoints
        schema_json["entrypoints"] = entrypoints;
    }

    write_schema_json(path_of_out, contract_name, contract_counter, schema_json)
}

fn function_v1_schema(schema: &FunctionV1) -> Value {
    // create empty function object
    let mut function_object: Value = Value::Object(serde_json::Map::new());

    // add parameter schema to function object
    if let Some(parameter_schema) = &schema.parameter() {
        function_object["parameter"] = type_to_json(*parameter_schema);
    }

    // add return_value schema to function object
    if let Some(return_value_schema) = &schema.return_value() {
        function_object["returnValue"] = type_to_json(*return_value_schema);
    }
    function_object
}

/// Converts the ContractV1 schema of the given contract_name to JSON and writes
/// it to a file named after the smart contract name at the specified location.
pub fn write_json_schema_to_file_v1(
    path_of_out: &Path,
    contract_name: &str,
    contract_counter: usize,
    contract_schema: &ContractV1,
) -> anyhow::Result<()> {
    // create empty schema_json
    let mut schema_json: Value = Value::Object(serde_json::Map::new());

    // add init schema
    if let Some(init_schema) = &contract_schema.init {
        schema_json["init"] = function_v1_schema(init_schema);
    }

    // add receive entrypoints
    if !contract_schema.receive.is_empty() {
        // create empty entrypoints
        let mut entrypoints: Value = Value::Object(serde_json::Map::new());

        // iterate through the entrypoints and add their schemas
        for (method_name, receive_schema) in contract_schema.receive.iter() {
            // add `method_name` entrypoint
            entrypoints[method_name] = function_v1_schema(receive_schema);
        }

        // add all receive entrypoints
        schema_json["entrypoints"] = entrypoints;
    }

    write_schema_json(path_of_out, contract_name, contract_counter, schema_json)
}

/// Convert a [schema type](schema::Type) to a base64 string.
fn type_to_json(ty: &schema::Type) -> Value { ENCODER.encode(to_bytes(ty)).into() }

/// Convert a [`FunctionV2`] schema to a JSON representation.
fn function_v2_schema(schema: &FunctionV2) -> Value {
    // create empty object
    let mut function_object: Value = Value::Object(serde_json::Map::new());

    // add parameter schema
    if let Some(parameter_schema) = &schema.parameter {
        function_object["parameter"] = type_to_json(parameter_schema);
    }

    // add return_value schema
    if let Some(return_value_schema) = &schema.return_value {
        function_object["returnValue"] = type_to_json(return_value_schema);
    }

    // add error schema
    if let Some(error_schema) = &schema.error {
        function_object["error"] = type_to_json(error_schema);
    }
    function_object
}

/// Converts the ContractV2 schema of the given contract_name to JSON and writes
/// it to a file named after the smart contract name at the specified location.
pub fn write_json_schema_to_file_v2(
    path_of_out: &Path,
    contract_name: &str,
    contract_counter: usize,
    contract_schema: &ContractV2,
) -> anyhow::Result<()> {
    // create empty schema_json
    let mut schema_json: Value = Value::Object(serde_json::Map::new());

    // add init schema
    if let Some(init_schema) = &contract_schema.init {
        schema_json["init"] = function_v2_schema(init_schema);
    }

    // add receive entrypoints
    if !contract_schema.receive.is_empty() {
        // create empty entrypoints
        let mut entrypoints: Value = Value::Object(serde_json::Map::new());

        // iterate through the entrypoints and add their schemas
        for (method_name, receive_schema) in contract_schema.receive.iter() {
            // add `method_name` entrypoint
            entrypoints[method_name] = function_v2_schema(receive_schema)
        }

        // add all receive entrypoints
        schema_json["entrypoints"] = entrypoints;
    }

    write_schema_json(path_of_out, contract_name, contract_counter, schema_json)
}

/// Converts the ContractV3 schema of the given contract_name to JSON and writes
/// it to a file named after the smart contract name at the specified location.
pub fn write_json_schema_to_file_v3(
    path_of_out: &Path,
    contract_name: &str,
    contract_counter: usize,
    contract_schema: &ContractV3,
) -> anyhow::Result<()> {
    // create empty schema_json
    let mut schema_json: Value = Value::Object(serde_json::Map::new());

    // add init schema
    if let Some(init_schema) = &contract_schema.init {
        schema_json["init"] = function_v2_schema(init_schema)
    }

    // add event schema
    if let Some(event_schema) = &contract_schema.event {
        schema_json["event"] = type_to_json(event_schema);
    }

    // add receive entrypoints
    if !contract_schema.receive.is_empty() {
        // create empty entrypoints
        let mut entrypoints: Value = Value::Object(serde_json::Map::new());

        // iterate through the entrypoints and add their schemas
        for (method_name, receive_schema) in contract_schema.receive.iter() {
            // add `method_name` entrypoint
            entrypoints[method_name] = function_v2_schema(receive_schema)
        }

        // add all receive entrypoints
        schema_json["entrypoints"] = entrypoints;
    }

    write_schema_json(path_of_out, contract_name, contract_counter, schema_json)
}

/// Build tests and run them. If errors occur in building the tests, or there
/// are runtime exceptions that are not expected then this function returns
/// Err(...).
///
/// Otherwise a boolean is returned, signifying whether the tests succeeded or
/// failed.
///
/// The `seed` argument allows for providing the seed to instantiate a random
/// number generator. If `None` is given, a random seed will be sampled.
pub fn build_and_run_wasm_test(extra_args: &[String], seed: Option<u64>) -> anyhow::Result<bool> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("Could not access cargo metadata.")?;

    let package = metadata
        .root_package()
        .context("Unable to determine package.")?;

    let target_dir = format!("{}/concordium", metadata.target_directory);

    let cargo_args = [
        "build",
        "--release",
        "--target",
        "wasm32-unknown-unknown",
        "--features",
        "concordium-std/wasm-test",
        "--target-dir",
        target_dir.as_str(),
    ];

    // Output what we are doing so that it is easier to debug if the user
    // has their own features or options.
    eprint!(
        "{} cargo {}",
        Color::Green.bold().paint("Running"),
        cargo_args.join(" ")
    );
    if extra_args.is_empty() {
        // This branch is just to avoid the extra trailing space in the case when
        // there are no extra arguments.
        eprintln!()
    } else {
        eprintln!(" {}", extra_args.join(" "));
    }
    let result = Command::new("cargo")
        .args(cargo_args)
        .args(extra_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context("Failed building contract tests.")?;
    // Make sure that compilation succeeded before proceeding.
    anyhow::ensure!(
        result.status.success(),
        Color::Red.bold().paint("Could not build contract tests.")
    );

    // If we compiled successfully the artifact is in the place listed below.
    // So we load it, and try to run it.s
    let filename = format!(
        "{}/wasm32-unknown-unknown/release/{}.wasm",
        target_dir,
        to_snake_case(package.name.as_str())
    );

    let wasm = std::fs::read(filename).context("Failed reading contract test output artifact.")?;

    eprintln!("\n{}", Color::Green.bold().paint("Running tests ..."));

    let seed_u64 = match seed {
        Some(s) => s,
        None => {
            // Since the seed was not provided, we use system randomness to sample a random
            // one and use is to seed a deterministic RNG. We store the seed so
            // we may report it to the user in case of test failure.
            thread_rng().gen()
        }
    };

    let results = utils::run_module_tests(&wasm, seed_u64)?;
    let mut num_failed = 0;
    for result in results {
        let test_name = result.0;
        match result.1 {
            Some((err, is_randomized)) => {
                num_failed += 1;
                eprintln!(
                    "  - {} ... {}",
                    test_name,
                    Color::Red.bold().paint("FAILED")
                );
                eprintln!(
                    "    {} ... {}",
                    Color::Red.bold().paint("Error"),
                    Style::new().italic().paint(err.to_string())
                );
                if is_randomized {
                    eprintln!(
                        "    {}: {}",
                        Style::new().bold().paint("Seed"),
                        Style::new().bold().paint(seed_u64.to_string())
                    )
                };
            }
            None => {
                eprintln!("  - {} ... {}", test_name, Color::Green.bold().paint("ok"));
            }
        }
    }

    if num_failed == 0 {
        eprintln!("Test result: {}", Color::Green.bold().paint("ok"));
        Ok(true)
    } else {
        eprintln!("Test result: {}", Color::Red.bold().paint("FAILED"));
        Ok(false)
    }
}
