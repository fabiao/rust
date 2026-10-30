use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use self::cargo::compiler::{
    BuildContext, BuildRunner, CompileMode, CrateType, DefaultExecutor, Executor, Lto, Unit, UnitInterner,
};
use self::cargo::util::CargoResult;
use self::cargo::util::command_prelude::*;
use self::cargo::workspace::{PackageId, Target};
use self::cargo_util::ProcessBuilder;
use serde_json::{Value, json};


fn launcher_environment(command: &ProcessBuilder) -> Result<BTreeMap<String, Option<std::ffi::OsString>>, String> {
    command.build_command().get_envs().map(|(name, value)| {
        Ok((name.to_str().ok_or("launcher environment key is not UTF-8")?.to_owned(), value.map(ToOwned::to_owned)))
    }).collect()
}

fn build_cli() -> cargo::util::command_prelude::Command {
    subcommand("build")
        .about("Compile a local package and all of its dependencies")
        .arg_future_incompat_report()
        .arg_message_format()
        .arg_silent_suggestion()
        .arg_package_spec(
            "Package to build (see `cargo help pkgid`)",
            "Build all packages in the workspace",
            "Exclude packages from the build",
        )
        .arg_targets_all(
            "Build only this package's library",
            "Build only the specified binary",
            "Build all binaries",
            "Build only the specified example",
            "Build all examples",
            "Build only the specified test target",
            "Build all targets that have `test = true` set",
            "Build only the specified bench target",
            "Build all targets that have `bench = true` set",
            "Build all targets",
        )
        .arg_features()
        .arg_release("Build artifacts in release mode, with optimizations")
        .arg_redundant_default_mode("debug", "build", "release")
        .arg_profile("Build artifacts with the specified profile")
        .arg_parallel()
        .selected_target_argument()
        .arg_target_dir()
        .arg_artifact_dir()
        .arg_unit_graph()
        .arg_timings()
        .arg_compile_time_deps()
        .arg_manifest_path()
        .arg_ignore_rust_version()
        .after_help(color_print::cstr!(
            "Run `<bright-cyan,bold>cargo help build</>` for more detailed information.\n"
        ))
}

fn unit_descriptor(unit: &Unit) -> Value {
    json!({"package": unit.pkg.package_id().to_spec(), "manifest": unit.pkg.manifest_path(),
        "target": unit.target, "profile": unit.profile, "platform": unit.kind,
        "mode": unit.mode, "features": unit.features, "rustflags": &*unit.rustflags,
        "rustdocflags": &*unit.rustdocflags, "standard_library": unit.is_std,
        "artifact": unit.artifact.is_true(),
        "artifact_target": unit.artifact_target_for_features.as_ref().map(|target| target.rustc_target()),
        "skip_non_compile_time_dependency": unit.skip_non_compile_time_dep,
        "links_overrides_sha256": crate::digest(format!("{:?}", unit.links_overrides).as_bytes())})
}

/// LTO mode per unit, keyed by the digest of its `unit_descriptor`. Cargo
/// derives a dependency's mode from the roots that reach it rather than
/// from the unit itself, so one descriptor compiles to bitcode for one
/// recipe and to object code for another. The shared unit store therefore
/// needs the mode in the unit's identity.
type LtoPlan = HashMap<String, String>;

fn lto_plan<'a>(assigned: impl IntoIterator<Item = (&'a Unit, &'a Lto)>) -> Result<LtoPlan, String> {
    let mut plan = HashMap::new();
    for (unit, lto) in assigned {
        let mode = format!("{lto:?}");
        let key = crate::digest(unit_descriptor(unit).to_string().as_bytes());
        if plan.insert(key, mode.clone()).is_some_and(|previous| previous != mode) {
            return Err("Cargo units sharing a descriptor receive different LTO modes".into());
        }
    }
    Ok(plan)
}

/// Cargo's private `lto::generate`, which runs only inside `compile_with_exec`.
/// Units are planned before that, so the mode is predicted here, and
/// `init` checks every prediction against the `BuildRunner`'s actual map.
fn predicted_lto(bcx: &BuildContext<'_, '_>) -> HashMap<Unit, Lto> {
    use self::cargo::workspace::profiles::Lto as ProfileLto;
    fn needs_object(types: &[CrateType]) -> bool {
        types.iter().any(|kind| kind.can_lto() || kind.is_dynamic())
    }
    fn with_object(types: &[CrateType]) -> Lto {
        if types.iter().all(|kind| *kind == CrateType::Dylib) { Lto::OnlyObject } else { Lto::ObjectAndBitcode }
    }
    fn calculate(bcx: &BuildContext<'_, '_>, map: &mut HashMap<Unit, Lto>, unit: &Unit, parent: Lto) {
        let types = match unit.mode {
            CompileMode::Test | CompileMode::Doctest => vec![CrateType::Bin],
            _ => unit.target.rustc_crate_types(),
        };
        let lto = if unit.target.for_host() {
            Lto::OnlyObject
        } else if types.iter().all(CrateType::can_lto) {
            match unit.profile.lto {
                ProfileLto::Named(name) => Lto::Run(Some(name)),
                ProfileLto::Off => Lto::Off,
                ProfileLto::Bool(true) => Lto::Run(None),
                ProfileLto::Bool(false) => Lto::OnlyObject,
            }
        } else {
            match (parent, needs_object(&types)) {
                (Lto::Run(_), false) => Lto::OnlyBitcode,
                (Lto::Run(_), true) | (Lto::OnlyBitcode, true) => with_object(&types),
                (Lto::Off, _) => Lto::Off,
                (_, false) | (Lto::OnlyObject, true) | (Lto::ObjectAndBitcode, true) => parent,
            }
        };
        let merged = match map.get(unit).copied() {
            None => lto,
            Some(previous) => {
                let merged = match (lto, previous) {
                    (Lto::OnlyBitcode, Lto::OnlyBitcode) => Lto::OnlyBitcode,
                    (Lto::OnlyObject, Lto::OnlyObject) => Lto::OnlyObject,
                    (Lto::Run(name), _) | (_, Lto::Run(name)) => Lto::Run(name),
                    (Lto::Off, _) | (_, Lto::Off) => Lto::Off,
                    (Lto::ObjectAndBitcode, _) | (_, Lto::ObjectAndBitcode) => Lto::ObjectAndBitcode,
                    (Lto::OnlyObject, Lto::OnlyBitcode) | (Lto::OnlyBitcode, Lto::OnlyObject) => {
                        Lto::ObjectAndBitcode
                    }
                };
                if merged == previous { return; }
                merged
            }
        };
        map.insert(unit.clone(), merged);
        for dependency in &bcx.unit_graph[unit] {
            calculate(bcx, map, &dependency.unit, merged);
        }
    }
    let mut map = HashMap::new();
    for root in &bcx.roots {
        let lto = match root.profile.lto {
            ProfileLto::Bool(false) => Lto::OnlyObject,
            ProfileLto::Off => Lto::Off,
            _ => {
                let types = root.target.rustc_crate_types();
                if root.target.for_host() {
                    Lto::OnlyObject
                } else if needs_object(&types) {
                    with_object(&types)
                } else {
                    Lto::OnlyBitcode
                }
            }
        };
        calculate(bcx, &mut map, root, lto);
    }
    map
}

fn descriptor(unit: &Unit, lto: &LtoPlan) -> Value {
    let mut value = unit_descriptor(unit);
    let key = crate::digest(value.to_string().as_bytes());
    value["lto"] = lto.get(&key).map_or(Value::Null, |mode| json!(mode));
    value
}

fn base_key(unit: &Unit, lto: &LtoPlan) -> String {
    crate::digest(descriptor(unit, lto).to_string().as_bytes())
}

struct GraphSnapshot {
    value: Value,
    keys: HashMap<Unit, String>,
}

fn graph_snapshot(
    graph: &cargo::compiler::unit_graph::UnitGraph,
    roots: &[Unit],
    lto: &LtoPlan,
) -> Result<GraphSnapshot, String> {
    fn visit(
        unit: &Unit,
        graph: &cargo::compiler::unit_graph::UnitGraph,
        lto: &LtoPlan,
        keys: &mut HashMap<Unit, String>,
        active: &mut HashSet<Unit>,
        units: &mut BTreeMap<String, Value>,
    ) -> Result<String, String> {
        if let Some(key) = keys.get(unit) {
            return Ok(key.clone());
        }
        if !active.insert(unit.clone()) {
            return Err(format!("cycle in selected Cargo unit graph: {}", unit.pkg.package_id()));
        }
        let dependencies = graph.get(unit).ok_or("missing selected Cargo dependency unit")?;
        let mut edges = Vec::new();
        for edge in dependencies {
            let key = visit(&edge.unit, graph, lto, keys, active, units)?;
            edges.push(json!({"unit": key, "extern_name": edge.extern_crate_name,
                "dependency_name": edge.dep_name, "public": edge.public,
                "noprelude": edge.noprelude, "nounused": edge.nounused}));
        }
        edges.sort_by_cached_key(Value::to_string);
        let value = json!({"unit": descriptor(unit, lto), "dependencies": edges});
        let key = crate::digest(value.to_string().as_bytes());
        if let Some(previous) = units.insert(key.clone(), value.clone()) {
            if previous != value {
                return Err("Cargo unit identity collision".into());
            }
        }
        keys.insert(unit.clone(), key.clone());
        active.remove(unit);
        Ok(key)
    }
    let mut keys = HashMap::new();
    let mut active = HashSet::new();
    let mut units = BTreeMap::new();
    for unit in graph.keys() {
        visit(unit, graph, lto, &mut keys, &mut active, &mut units)?;
    }
    let roots: BTreeSet<_> = roots
        .iter()
        .map(|unit| keys.get(unit).cloned().ok_or("missing selected Cargo root unit"))
        .collect::<Result<_, _>>()?;
    Ok(GraphSnapshot { value: json!({"units": units, "roots": roots}), keys })
}

fn configuration_files(value: &cargo::context::ConfigValue, paths: &mut BTreeSet<PathBuf>) {
    use self::cargo::context::{ConfigValue, Definition};
    match value.definition() {
        Definition::Path(path) | Definition::Cli(Some(path)) => {
            paths.insert(path.clone());
        }
        _ => {}
    }
    match value {
        ConfigValue::Table(table, _) => {
            for value in table.values() {
                configuration_files(value, paths);
            }
        }
        ConfigValue::List(list, _) => {
            for value in list {
                configuration_files(value, paths);
            }
        }
        _ => {}
    }
}

fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn publish(path: &Path, value: &Value) -> Result<(), String> {
    let partial = path.with_extension("partial");
    fs::write(&partial, serde_json::to_vec(value).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    fs::rename(partial, path).map_err(|e| e.to_string())
}

/// Facts for shared library and build-script units, keyed by their semantic
/// unit identity. They bind to this store's fixed request instead of one
/// recipe's graph, so a later build of the same unit in a shared Cargo target
/// directory reuses them rather than forcing Cargo to compile it again.
struct UnitStore {
    request: Value,
    request_path: PathBuf,
    directory: PathBuf,
    keys: BTreeSet<String>,
}

struct CaptureExecutor {
    request: Value,
    request_path: PathBuf,
    directory: PathBuf,
    units: Option<UnitStore>,
    graph: Value,
    graph_sha256: String,
    configuration_sha256: String,
    cwd: PathBuf,
    graph_inputs: BTreeMap<PathBuf, String>,
    producer: Value,
    selected: BTreeSet<String>,
    selected_by_base: BTreeMap<String, BTreeSet<String>>,
    lto: LtoPlan,
    dispatch: Mutex<BTreeMap<String, String>>,
    artifact_publications: Mutex<BTreeMap<String, Vec<(String, PathBuf)>>>,
    errors: Mutex<Vec<String>>,
    // Cargo joins compiler jobs before the completion marker is read.
    compiled_build_script: AtomicBool,
}

impl CaptureExecutor {
    fn entry_request(&self, key: &str, command: &ProcessBuilder) -> Result<Option<(PathBuf, String)>, String> {
        let policy = match command.get_envs().get("ASK_ENTRY_LINK_POLICY") {
            Some(value) => value.clone(),
            None => std::env::var_os("ASK_ENTRY_LINK_POLICY"),
        };
        let Some(policy) = policy else { return Ok(None) };
        let policy = fs::canonicalize(policy).map_err(|error| error.to_string())?;
        let bytes = fs::read(&policy).map_err(|error| error.to_string())?;
        let args = command.get_args().map(|arg| arg.to_str().ok_or("non-UTF8 controlled compiler argument"))
            .collect::<Result<Vec<_>, _>>()?;
        let target = args.windows(2).find_map(|pair| (pair[0] == "--target").then_some(pair[1]))
            .or_else(|| args.iter().find_map(|arg| arg.strip_prefix("--target=")));
        let ask_target = target.is_some_and(|target| target == "x86_64-unknown-ask"
            || fs::read(target).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|spec| spec["os"] == "ask"));
        if !ask_target { return Ok(None); }
        let kinds = args.windows(2).find_map(|pair| (pair[0] == "--crate-type").then_some(pair[1]))
            .or_else(|| args.iter().find_map(|arg| arg.strip_prefix("--crate-type=")));
        let final_kind = kinds.is_some_and(|kinds| kinds.split(',').any(|kind|
            matches!(kind, "bin" | "cdylib" | "dylib")));
        if !final_kind { return Ok(None); }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?.as_nanos();
        let identity = crate::digest(format!("{key}:{}:{now}:{}", std::process::id(), crate::digest(&bytes)).as_bytes());
        let path = self.directory.join(format!("{key}.entry-request.json"));
        let request = json!({"request_id": identity,
            "plan_path": self.directory.join(format!("{key}.entry-plan.json")),
            "receipt_path": self.directory.join(format!("{key}.entry-receipt.json")),
            "facts_path": self.fact_path(key), "policy_path": policy, "policy_sha256": crate::digest(&bytes)});
        publish(&path, &request)?;
        let hash = crate::digest(&fs::read(&path).map_err(|error| error.to_string())?);
        Ok(Some((path, hash)))
    }

    fn shared(&self, key: &str) -> Option<&UnitStore> {
        self.units.as_ref().filter(|store| store.keys.contains(key))
    }
    fn request_for(&self, key: &str) -> (&Value, &Path) {
        match self.shared(key) {
            Some(store) => (&store.request, &store.request_path),
            None => (&self.request, &self.request_path),
        }
    }
    /// A shared unit's key already digests its descriptor and dependency keys.
    fn binding(&self, key: &str) -> String {
        if self.shared(key).is_some() { format!("unit:{key}") } else { self.graph_sha256.clone() }
    }
    fn record_path(&self, key: &str) -> PathBuf {
        self.shared(key).map_or(&self.directory, |store| &store.directory).join(format!("{key}.unit.json"))
    }
    fn fact_path(&self, key: &str) -> PathBuf {
        self.shared(key).map_or(&self.directory, |store| &store.directory).join(format!("{key}.facts.json"))
    }

    fn valid(&self, key: &str, command: Option<&ProcessBuilder>) -> bool {
        let (request, request_path) = self.request_for(key);
        let validate = || -> Option<()> {
            if read_json(request_path).ok()? != *request { return None; }
            if self.shared(key).is_none() {
                for (path, hash) in &self.graph_inputs {
                    if crate::digest(&fs::read(path).ok()?) != *hash {
                        return None;
                    }
                }
            }
            let record = read_json(&self.record_path(key)).ok()?;
            for publication in record["publication_declarations"].as_array()? {
                let kind = publication["kind"].as_str()?;
                let path = Path::new(publication["path"].as_str()?);
                if !matches!(kind, "bin" | "dylib" | "cdylib" | "rlib" | "staticlib") || !path.is_absolute() {
                    return None;
                }
            }
            let facts = read_json(&self.fact_path(key)).ok()?;
            crate::records::validate(&facts, &self.fact_path(key)).ok()?;
            if let Some(plan) = facts["entry_input_plan_path"].as_str() {
                let receipt = read_json(Path::new(facts["entry_input_receipt_path"].as_str()?)).ok()?;
                if receipt["state"] != "complete"
                    || receipt["facts_sha256"].as_str()? != crate::digest(&fs::read(self.fact_path(key)).ok()?)
                    || receipt["plan_sha256"] != facts["entry_input_plan_sha256"]
                    || receipt["plan_sha256"].as_str()? != crate::digest(&fs::read(plan).ok()?) {
                    return None;
                }
                for field in ["premerge_artifacts", "compiler_output_artifacts", "output_artifacts"] {
                    for (path, hash) in receipt[field].as_object()? {
                        if hash.as_str()? != crate::digest(&fs::read(path).ok()?) { return None; }
                    }
                }
            } else if std::env::var_os("ASK_ENTRY_LINK_POLICY").is_some() {
                let arguments = facts["arguments"].as_array()?;
                let ask_target = arguments.windows(2).any(|pair| pair[0] == "--target"
                    && pair[1].as_str().is_some_and(|target| target == "x86_64-unknown-ask"
                        || fs::read(target).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                            .is_some_and(|spec| spec["os"] == "ask")));
                if ask_target && arguments.windows(2).any(|pair| pair[0] == "--crate-type"
                    && pair[1].as_str().is_some_and(|kind| matches!(kind, "bin" | "dylib" | "cdylib"))) {
                    return None;
                }
            }
            if record.get("request")? != request
                || record.get("binding")?.as_str()? != self.binding(key)
                || record.get("producer")? != &self.producer
                || facts.get("complete")? != true
                || facts.get("capture_request")? != request
            {
                return None;
            }
            if record.get("facts_sha256")?.as_str()?
                != crate::digest(&fs::read(self.fact_path(key)).ok()?)
            {
                return None;
            }
            let cwd = Path::new(facts.get("cwd")?.as_str()?);
            let compiler = Path::new(facts.get("arguments")?.as_array()?.first()?.as_str()?);
            let compiler = if compiler.is_absolute() { compiler.to_path_buf() } else { cwd.join(compiler) };
            if fs::canonicalize(compiler).ok()?.to_str()? != facts.get("compiler_realpath")?.as_str()? { return None; }
            for field in [
                "loaded_inputs",
                "dependency_artifacts",
                "file_inputs",
                "tool_inputs",
                "output_artifacts",
            ] {
                let inputs = facts.get(field)?.as_object()?;
                if field == "output_artifacts" && inputs.is_empty() {
                    return None;
                }
                for (name, hash) in inputs {
                    let path = Path::new(name);
                    let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
                    if hash.as_str()? != crate::digest(&fs::read(path).ok()?) {
                        return None;
                    }
                }
            }
            for (name, exists) in facts.get("input_existence")?.as_object()? {
                let path = Path::new(name);
                if (if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }).exists()
                    != exists.as_bool()?
                {
                    return None;
                }
            }
            if !facts.get("launcher")?.is_null() {
                let launcher = &facts["launcher"];
                crate::launcher::validate(launcher, &self.request).ok()?;
                if let Some(command) = command {
                    let args = command.get_args().cloned().collect::<Vec<_>>();
                    if crate::launcher::snapshot(Path::new(command.get_program()), &args,
                        command.get_cwd().unwrap_or(&self.cwd), &launcher_environment(command).ok()?).ok()? != *launcher { return None; }
                } else {
                    let mut current = crate::launcher::environment(&Default::default()).ok()?;
                    let current = current.as_object_mut()?;
                    let original = launcher["environment"].as_object()?;
                    for name in record["launcher_overrides"].as_array()? {
                        let name = name.as_str()?;
                        if let Some(value) = original.get(name) { current.insert(name.to_owned(), value.clone()); }
                        else { current.remove(name); }
                    }
                    if current != original { return None; }
                }
                let effective = facts["effective_environment"].as_object()?;
                let tracked = facts["environment_inputs"].as_object()?;
                if record["environment_inputs"] != facts["environment_inputs"] { return None; }
                for (name, expected) in tracked {
                    if effective.get(&crate::launcher::environment_key(name)).unwrap_or(&Value::Null) != expected { return None; }
                }
                return Some(());
            }
            let overrides = record.get("environment_overrides")?.as_object()?;
            let tracked = facts.get("environment_inputs")?.as_object()?;
            if record.get("environment_inputs")? != &facts["environment_inputs"] {
                return None;
            }
            if overrides.iter().any(|(name, hash)| tracked.get(name) != Some(hash)) {
                return None;
            }
            for (name, expected) in tracked {
                if command.is_none() && overrides.contains_key(name) {
                    // Cargo invalidates generated command environments through its normal fingerprints.
                    continue;
                }
                let value = match command {
                    Some(cmd) => cmd.get_env(name),
                    None => std::env::var_os(name),
                };
                let actual = match value {
                    Some(value) => json!(crate::digest(value.to_str()?.as_bytes())),
                    None => Value::Null,
                };
                if &actual != expected {
                    return None;
                }
            }
            Some(())
        };
        validate().is_some()
    }

    fn note_error(&self, error: String) {
        if let Ok(mut errors) = self.errors.lock() {
            errors.push(error);
        }
    }
}

fn discriminator(source: &Path, suffix: &str) -> String {
    format!("{}:{suffix}", source.display())
}

impl Executor for CaptureExecutor {
    fn init(&self, runner: &BuildRunner<'_, '_>, unit: &Unit) {
        let selected = self.selected_by_base.contains_key(&base_key(unit, &self.lto));
        let publication = unit.target.is_lib()
            || unit.target.is_bin() || unit.mode.is_any_test();
        if !selected && !publication { return; }
        // Cargo's own LTO assignment: a wrong prediction changes the snapshot.
        let snapshot = match lto_plan(&runner.lto)
            .and_then(|lto| graph_snapshot(&runner.bcx.unit_graph, &runner.bcx.roots, &lto))
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.note_error(error);
                return;
            }
        };
        if snapshot.value != self.graph {
            self.note_error("Cargo unit graph changed during capture preparation".into());
            return;
        }
        let Some(key) = snapshot.keys.get(unit) else {
            self.note_error("selected unit is absent from Cargo graph".into());
            return;
        };
        if publication {
            let publication = (|| -> Result<Vec<(String, PathBuf)>, String> {
                let info = runner.bcx.target_data.info(unit.kind);
                let target = runner.bcx.target_data.short_name(&unit.kind);
                let (types, _) = info.rustc_outputs(unit.mode, unit.target.kind(), target)
                    .map_err(|error| error.to_string())?;
                let outputs = runner.outputs(unit).map_err(|error| error.to_string())?;
                let metadata = runner.files().metadata(unit).c_extra_filename().map(|hash| hash.to_string());
                let mut result = Vec::new();
                for file_type in types {
                    if !matches!(file_type.flavor, cargo::compiler::FileFlavor::Normal | cargo::compiler::FileFlavor::Linkable) {
                        continue;
                    }
                    let kind = match file_type.crate_type {
                        Some(cargo::compiler::CrateType::Bin) => "bin",
                        Some(cargo::compiler::CrateType::Dylib) => "dylib",
                        Some(cargo::compiler::CrateType::Cdylib) => "cdylib",
                        Some(cargo::compiler::CrateType::Lib | cargo::compiler::CrateType::Rlib) => "rlib",
                        Some(cargo::compiler::CrateType::Staticlib) => "staticlib",
                        _ => continue,
                    };
                    let source = runner.files().output_dir(unit)
                        .join(file_type.output_filename(&unit.target, metadata.as_deref()));
                    let output = outputs.iter().find(|output| output.path == source)
                        .ok_or("typed Cargo artifact output has no publication record")?;
                    result.push((kind.to_owned(), output.bin_dst().clone()));
                }
                Ok(result)
            })();
            match publication {
                Ok(outputs) => match self.artifact_publications.lock() {
                    Ok(mut declarations) => {
                        if declarations.insert(key.clone(), outputs.clone()).is_some_and(|old| old != outputs) {
                            self.note_error("Cargo artifact publication changed during capture".into());
                            return;
                        }
                    }
                    Err(_) => { self.note_error("Cargo artifact publication lock poisoned".into()); return; }
                },
                Err(error) => { self.note_error(error); return; }
            }
        }
        if !selected { return; }
        let Some(source) = unit.target.src_path().path() else {
            self.note_error("selected unit has no source path".into());
            return;
        };
        let suffix = runner
            .files()
            .metadata(unit)
            .c_extra_filename()
            .map(|hash| format!("-{hash}"))
            .unwrap_or_default();
        match self.dispatch.lock() {
            Ok(mut dispatch) => {
                if dispatch.insert(discriminator(source, &suffix), key.clone()).is_some() {
                    self.note_error("ambiguous Cargo compiler invocation identity".into());
                }
            }
            Err(_) => self.note_error("Cargo capture dispatch poisoned".into()),
        }
    }

    fn force_rebuild(&self, unit: &Unit) -> bool {
        self.selected_by_base
            .get(&base_key(unit, &self.lto))
            .is_some_and(|keys| keys.iter().any(|key| !self.valid(key, None)))
    }

    fn exec(
        &self,
        cmd: &ProcessBuilder,
        id: PackageId,
        target: &Target,
        mode: CompileMode,
        stdout: &mut dyn FnMut(&str) -> CargoResult<()>,
        stderr: &mut dyn FnMut(&str) -> CargoResult<()>,
    ) -> CargoResult<()> {
        let args: Vec<_> = cmd.get_args().collect();
        let suffix =
            args.iter().find_map(|arg| arg.to_str()?.strip_prefix("extra-filename=")).unwrap_or("");
        let key = target.src_path().path().and_then(|source| {
            self.dispatch.lock().ok()?.get(&discriminator(source, suffix)).cloned()
        });
        let Some(key) = key else {
            return DefaultExecutor.exec(cmd, id, target, mode, stdout, stderr);
        };
        let mut captured = cmd.clone();
        let entry_request = self.entry_request(&key, cmd).map_err(cargo::util::errors::internal)?;
        let compiler = fs::canonicalize(Path::new(cmd.get_program()))?;
        let helper = compiler.with_file_name(if cfg!(windows) {
            "cargo-asksource.exe"
        } else {
            "cargo-asksource"
        });
        let (request, request_path) = self.request_for(&key);
        let shim = self.request["compiler_shims"].get(compiler.to_string_lossy().as_ref());
        if shim.is_some() {
            let launcher = crate::launcher::snapshot(&compiler, &args.iter().map(|arg| (*arg).clone()).collect::<Vec<_>>(),
                cmd.get_cwd().unwrap_or(&self.cwd), &launcher_environment(cmd).map_err(cargo::util::errors::internal)?).map_err(cargo::util::errors::internal)?;
            crate::launcher::validate(&launcher, &self.request).map_err(cargo::util::errors::internal)?;
            let capture = json!({"output": self.fact_path(&key), "request": request_path,
                "entry_input_request": entry_request.as_ref().map(|(path, _)| path),
                "entry_input_request_sha256": entry_request.as_ref().map(|(_, hash)| hash),
                "request_sha256": crate::digest(&fs::read(request_path)?), "launcher": launcher});
            captured.arg(format!("{}{}", crate::launcher::control::PREFIX, capture));
        } else {
            let mut compiler_args = vec![
                std::ffi::OsString::from("capture"), "--output".into(), self.fact_path(&key).into_os_string(),
                "--request".into(), request_path.as_os_str().to_owned(),
                "--request-sha256".into(), crate::digest(&fs::read(request_path)?).into(),
            ];
            if let Some((path, hash)) = &entry_request {
                compiler_args.extend(["--entry-input-request".into(), path.clone().into_os_string(),
                    "--entry-input-request-sha256".into(), hash.into()]);
            }
            compiler_args.extend(["--".into(), cmd.get_program().clone()]);
            compiler_args.extend(args.into_iter().cloned());
            captured.program(helper).args_replace(&compiler_args);
        }
        let record_path = self.record_path(&key);
        if record_path.exists() {
            fs::remove_file(&record_path)?;
        }
        DefaultExecutor.exec(&captured, id, target, mode, stdout, stderr)?;
        let facts = read_json(&self.fact_path(&key)).map_err(cargo::util::errors::internal)?;
        let overrides: BTreeMap<_, _> = facts["environment_inputs"]
            .as_object()
            .ok_or_else(|| cargo::util::errors::internal("missing tracked compiler environment"))?
            .iter()
            .filter(|(name, _)| cmd.get_envs().contains_key(*name))
            .map(|(name, hash)| (name.clone(), hash.clone()))
            .collect();
        let publications = self.artifact_publications.lock()
            .map_err(|_| cargo::util::errors::internal("Cargo artifact publication lock poisoned"))?
            .get(&key).cloned().unwrap_or_default();
        let publications: Result<Vec<_>, _> = publications.into_iter().map(|(kind, path)|
            std::path::absolute(path).map(|path| json!({"kind": kind, "path": path}))).collect();
        let record = json!({"request": request, "binding": self.binding(&key), "producer": self.producer,
            "publication_declarations": publications?,
            "facts_sha256": crate::digest(&fs::read(self.fact_path(&key))?),
            "environment_inputs": facts["environment_inputs"],
            "environment_overrides": overrides,
            "launcher_overrides": launcher_environment(cmd).map_err(cargo::util::errors::internal)?.keys().map(|name| crate::launcher::environment_key(name)).collect::<Vec<_>>()});
        publish(&record_path, &record).map_err(cargo::util::errors::internal)?;
        if !self.valid(&key, Some(cmd)) {
            fs::remove_file(&record_path)?;
            return Err(cargo::util::errors::internal(
                "compiler capture inputs changed during build",
            ));
        }
        if target.is_custom_build() {
            self.compiled_build_script.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

pub fn run(arguments: &[String]) -> Result<(), String> {
    if arguments.get(1).is_some_and(|argument| argument == "--discovery-context") {
        return restore_discovery(arguments);
    }
    let mut cli = build_cli()
        .arg(
            Arg::new("capture-request")
                .long("capture-request")
                .required(true)
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(Arg::new("verbose").short('v').long("verbose").action(ArgAction::Count))
        .arg(Arg::new("quiet").short('q').long("quiet").action(ArgAction::SetTrue))
        .arg(Arg::new("color").long("color").value_parser(["auto", "always", "never"]))
        .arg(Arg::new("frozen").long("frozen").action(ArgAction::SetTrue))
        .arg(Arg::new("locked").long("locked").action(ArgAction::SetTrue))
        .arg(Arg::new("offline").long("offline").action(ArgAction::SetTrue))
        .arg(Arg::new("unstable-features").short('Z').action(ArgAction::Append))
        .arg(Arg::new("config").long("config").action(ArgAction::Append));
    let args = cli.try_get_matches_from_mut(arguments).map_err(|e| e.to_string())?;
    let request_path = args
        .get_one::<PathBuf>("capture-request")
        .cloned()
        .ok_or("build capture requires --capture-request")?;
    let request = read_json(&request_path)?;
    crate::launcher::validate_request(&request)?;
    let directory =
        PathBuf::from(request["directory"].as_str().ok_or("missing capture directory")?);
    let sources: BTreeSet<PathBuf> =
        serde_json::from_value(request["sources"].clone()).map_err(|e| e.to_string())?;
    let candidate_manifests: BTreeSet<PathBuf> =
        serde_json::from_value(request["candidate_manifests"].clone()).map_err(|e| e.to_string())?;
    if !directory.is_absolute()
        || sources.iter().any(|path| !path.is_absolute())
        || request["graph"].as_object().is_none_or(|graph| graph.is_empty())
    {
        return Err("capture requires absolute paths and an explicit selected graph context".into());
    }
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let capture_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("capture.lock"))
        .map_err(|e| e.to_string())?;
    capture_lock.lock().map_err(|e| format!("cannot lock selected capture directory: {e}"))?;
    let snapshot = directory.join("build.json");
    if snapshot.exists() {
        fs::remove_file(&snapshot).map_err(|e| e.to_string())?;
    }
    let shims = request["compiler_shims"].as_object().ok_or("missing explicit compiler_shims map")?;
    for (name, hash) in shims {
        let path = Path::new(name);
        if !path.is_absolute() || fs::canonicalize(path).map_err(|e| e.to_string())? != path
            || hash.as_str() != Some(crate::digest(&fs::read(path).map_err(|e| e.to_string())?).as_str()) {
            return Err(format!("invalid or stale compiler shim identity: {name}"));
        }
    }
    let canonical_sources: BTreeSet<_> = sources
        .iter()
        .map(|source| {
            fs::canonicalize(source).map_err(|e| format!("owned source {}: {e}", source.display()))
        })
        .collect::<Result<_, _>>()?;
    let canonical_manifests: BTreeSet<_> = candidate_manifests.iter()
        .map(|manifest| fs::canonicalize(manifest)
            .map_err(|e| format!("candidate manifest {}: {e}", manifest.display())))
        .collect::<Result<_, _>>()?;
    let cargo_exe =
        PathBuf::from(std::env::var_os("CARGO").ok_or(
            "invoke through cargo asksource build so Cargo supplies its actual executable",
        )?);
    let version = Command::new(&cargo_exe).arg("-Vv").output().map_err(|e| e.to_string())?;
    let embedded_version = cargo::version();
    let embedded = embedded_version.commit_info.ok_or("embedded Cargo has no source identity")?;
    if !version.status.success()
        || !String::from_utf8_lossy(&version.stdout)
            .lines()
            .any(|line| line == format!("commit-hash: {}", embedded.commit_hash))
        || !String::from_utf8_lossy(&version.stdout)
            .lines()
            .any(|line| line == format!("release: {}", embedded_version.version))
    {
        return Err("selected Cargo does not match embedded Cargo; rebuild asksource for the selected producer".into());
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let producer = json!({"path": cargo_exe,
        "executor": executable, "executor_sha256": crate::digest(&fs::read(&executable).map_err(|e| e.to_string())?),
        "sha256": crate::digest(&fs::read(&cargo_exe).map_err(|e| e.to_string())?),
        "commit": embedded.commit_hash, "release": embedded_version.version,
        "channel": embedded_version.release_channel});
    let mut gctx = GlobalContext::default().map_err(|e| e.to_string())?;
    let strings = |name: &str| -> Vec<String> {
        args.get_many::<String>(name).map(|values| values.cloned().collect()).unwrap_or_default()
    };
    gctx.configure(
        args.get_count("verbose").into(),
        args.get_flag("quiet"),
        args.get_one::<String>("color").map(String::as_str),
        args.get_flag("frozen"),
        args.get_flag("locked"),
        args.get_flag("offline"),
        &args.value_of_path("target-dir", &gctx),
        &strings("unstable-features"),
        &strings("config"),
    )
    .map_err(|e| e.to_string())?;
    let ws = args.workspace(&gctx).map_err(|e| e.to_string())?;
    let mut options = args
        .compile_options(&gctx, UserIntent::Build, Some(&ws), ProfileChecking::Custom)
        .map_err(|e| e.to_string())?;
    options.build_config.export_dir = args.value_of_path("artifact-dir", &gctx).or_else(|| {
        gctx.build_config().ok()?.artifact_dir.as_ref().map(|dir| dir.resolve_path(&gctx))
    });
    if options.build_config.export_dir.is_some() {
        gctx.cli_unstable()
            .fail_if_stable_opt("--artifact-dir", 6790)
            .map_err(|e| e.to_string())?;
    }
    if options.build_config.unit_graph {
        return Err(
            "capture requires a completed build; --unit-graph does not compile units".into()
        );
    }
    let interner = UnitInterner::new();
    let bcx = cargo::ops::create_bcx(&ws, &options, &interner, None).map_err(|e| e.to_string())?;
    let lto = lto_plan(&predicted_lto(&bcx))?;
    let graph_snapshot = graph_snapshot(&bcx.unit_graph, &bcx.roots, &lto)?;
    let graph = graph_snapshot.value;
    let mut graph_paths: BTreeSet<_> =
        bcx.unit_graph.keys().map(|unit| unit.pkg.manifest_path().to_path_buf()).collect();
    graph_paths.insert(ws.root_manifest().to_path_buf());
    graph_paths.insert(
        ws.requested_lockfile_path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| ws.lock_root().as_path_unlocked().join("Cargo.lock")),
    );
    for value in gctx.values().map_err(|e| e.to_string())?.values() {
        configuration_files(value, &mut graph_paths);
    }
    let graph_inputs: BTreeMap<_, _> = graph_paths
        .into_iter()
        .map(|path| {
            fs::read(&path)
                .map(|bytes| (path.clone(), crate::digest(&bytes)))
                .map_err(|e| format!("Cargo graph input {}: {e}", path.display()))
        })
        .collect::<Result<_, _>>()?;
    let configuration_sha256 =
        crate::digest(&serde_json::to_vec(&strings("config")).map_err(|e| e.to_string())?);
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let graph_sha256 = crate::digest(
        json!({"graph": graph, "inputs": graph_inputs,
        "configuration_sha256": configuration_sha256, "cwd": cwd})
        .to_string()
        .as_bytes(),
    );
    let mut selected = BTreeSet::new();
    let mut selected_by_base: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut primitive_units = HashSet::new();
    if let Some(policy) = std::env::var_os("ASK_ENTRY_LINK_POLICY") {
        let policy = read_json(Path::new(&policy))?;
        let manifests: Vec<PathBuf> = serde_json::from_value(policy["primitive_manifests"].clone())
            .map_err(|error| format!("invalid primitive capture manifests: {error}"))?;
        let manifests: BTreeSet<_> = manifests.into_iter().map(fs::canonicalize)
            .collect::<Result<_, _>>().map_err(|error| error.to_string())?;
        let mut pending: Vec<_> = bcx.unit_graph.keys().filter(|unit|
            fs::canonicalize(unit.pkg.manifest_path()).is_ok_and(|path| manifests.contains(&path)))
            .cloned().collect();
        pending.extend(bcx.unit_graph.keys().filter(|unit| {
            matches!(unit.kind, self::cargo::compiler::CompileKind::Target(target)
                if target.rustc_target() == "x86_64-unknown-ask")
                && unit.target.is_executable()
                && !unit.mode.is_run_custom_build()
        }).cloned());
        while let Some(unit) = pending.pop() {
            if !primitive_units.insert(unit.clone()) { continue; }
            for edge in bcx.unit_graph.get(&unit).ok_or("missing primitive dependency unit")? {
                pending.push(edge.unit.clone());
            }
        }
    }
    // Only a request without owned-source projections or launcher shims yields
    // recipe-independent facts; final executables keep their recipe's policy.
    let (mut units, _unit_lock) = match request.get("unit_store").and_then(Value::as_str) {
        Some(store) if sources.is_empty() && shims.is_empty() => {
            let directory = PathBuf::from(store);
            fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
                .open(directory.join("capture.lock")).map_err(|e| e.to_string())?;
            lock.lock().map_err(|e| format!("cannot lock shared unit store: {e}"))?;
            let unit_request = json!({"directory": directory, "sources": [], "candidate_manifests": [],
                "graph": {"owner": "shared-unit-facts"}, "compiler_shims": {}});
            let request_path = directory.join("request.json");
            if read_json(&request_path).ok().as_ref() != Some(&unit_request) {
                publish(&request_path, &unit_request)?;
            }
            (Some(UnitStore { request: unit_request, request_path, directory, keys: BTreeSet::new() }), Some(lock))
        }
        _ => (None, None),
    };
    for unit in
        bcx.unit_graph.keys().filter(|unit| !unit.mode.is_run_custom_build()).filter(|unit| {
            primitive_units.contains(*unit) || unit.target.src_path().path().is_some_and(|path| {
                fs::canonicalize(path).is_ok_and(|path| canonical_sources.contains(&path))
            }) || fs::canonicalize(unit.pkg.manifest_path())
                .is_ok_and(|path| canonical_manifests.contains(&path))
        })
    {
        let key =
            graph_snapshot.keys.get(unit).ok_or("selected unit has no semantic identity")?.clone();
        if let Some(store) = units.as_mut()
            && unit.mode == CompileMode::Build
            && (unit.target.is_lib() || unit.target.is_custom_build())
        {
            store.keys.insert(key.clone());
        }
        selected.insert(key.clone());
        selected_by_base.entry(base_key(unit, &lto)).or_default().insert(key);
    }
    let executor = Arc::new(CaptureExecutor {
        request,
        request_path,
        directory,
        units,
        graph,
        graph_sha256,
        configuration_sha256,
        cwd,
        graph_inputs,
        producer,
        selected,
        selected_by_base,
        lto,
        dispatch: Mutex::new(BTreeMap::new()),
        artifact_publications: Mutex::new(BTreeMap::new()),
        errors: Mutex::new(Vec::new()),
        compiled_build_script: AtomicBool::new(false),
    });
    drop(bcx);
    let exec: Arc<dyn Executor> = executor.clone();
    let mut compilation = cargo::ops::compile_with_exec(&ws, &options, &exec).map_err(|e| e.to_string())?;
    if executor.compiled_build_script.swap(false, Ordering::Relaxed) {
        // A forced script compile does not transitively invalidate its run unit.
        // Let Cargo settle script runs and dependents before publishing facts.
        executor.dispatch.lock().map_err(|_| "Cargo capture dispatch poisoned")?.clear();
        compilation = cargo::ops::compile_with_exec(&ws, &options, &exec).map_err(|e| e.to_string())?;
    }
    let errors = executor.errors.lock().map_err(|_| "Cargo capture errors poisoned")?;
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    if executor.producer["sha256"]
        != crate::digest(&fs::read(&cargo_exe).map_err(|e| e.to_string())?)
    {
        return Err("selected Cargo executable changed during capture".into());
    }
    if read_json(&executor.request_path)? != executor.request {
        return Err("capture request changed during build".into());
    }
    for (path, hash) in &executor.graph_inputs {
        if crate::digest(&fs::read(path).map_err(|e| e.to_string())?) != *hash {
            return Err(format!("Cargo graph input changed during build: {}", path.display()));
        }
    }
    let mut facts = BTreeMap::new();
    for key in &executor.selected {
        if !executor.valid(key, None) {
            return Err(format!("missing or stale compiler facts for selected unit {key}"));
        }
        facts.insert(key, json!({"path": executor.fact_path(key),
            "sha256": crate::digest(&fs::read(executor.fact_path(key)).map_err(|e| e.to_string())?),
            "record": executor.record_path(key),
            "record_sha256": crate::digest(&fs::read(executor.record_path(key)).map_err(|e| e.to_string())?)}));
    }
    let mut published_artifacts: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut published_paths = BTreeMap::new();
    let declarations = executor.artifact_publications.lock()
        .map_err(|_| "Cargo artifact publication lock poisoned")?;
    let mut publications = Vec::new();
    for output in &compilation.binaries {
        let path = std::path::absolute(&output.path).map_err(|error| error.to_string())?;
        let mut identity = None;
        for (key, outputs) in declarations.iter() {
            for (kind, declared) in outputs {
                if kind != "bin" || std::path::absolute(declared).map_err(|error| error.to_string())? != path {
                    continue;
                }
                if executor.graph["units"][key]["unit"] != descriptor(&output.unit, &executor.lto)
                    || identity.replace(key.clone()).is_some()
                {
                    return Err("published Cargo binary has an ambiguous unit identity".into());
                }
            }
        }
        // Cargo calls `Executor::init` only for dirty units, so a fresh binary has
        // no declaration; its graph unit is then the one sharing its descriptor.
        let key = match identity {
            Some(key) => key,
            None => {
                let unit = descriptor(&output.unit, &executor.lto);
                let mut matches = executor.graph["units"].as_object()
                    .ok_or("capture graph omits units")?
                    .iter()
                    .filter(|(_, node)| node["unit"] == unit)
                    .map(|(key, _)| key.clone());
                let key = matches.next().ok_or("published artifact has no captured Cargo unit identity")?;
                if matches.next().is_some() {
                    return Err("published Cargo binary has an ambiguous unit identity".into());
                }
                key
            }
        };
        publications.push((key, "bin".to_owned(), path));
    }
    for (key, outputs) in declarations.iter() {
        for (kind, path) in outputs {
            if kind != "bin" { publications.push((key.clone(), kind.clone(), path.clone())); }
        }
    }
    for key in &executor.selected {
        if declarations.contains_key(key) { continue; }
        let record = read_json(&executor.record_path(key))?;
        for declaration in record["publication_declarations"].as_array()
            .ok_or("captured unit omits Cargo publication declarations; recapture selected build")? {
            let kind = declaration["kind"].as_str().ok_or("captured publication omits kind")?;
            if kind == "bin" { continue; }
            let path = declaration["path"].as_str().ok_or("captured publication omits path")?;
            publications.push((key.clone(), kind.to_owned(), PathBuf::from(path)));
        }
    }
    for (key, kind, path) in publications {
        let path = std::path::absolute(path).map_err(|error| error.to_string())?;
        let hash = crate::digest(&fs::read(&path)
            .map_err(|error| format!("published artifact {}: {error}", path.display()))?);
        if let Some(previous) = published_paths.insert(path.clone(), (key.clone(), kind.clone(), hash.clone())) {
            if previous != (key.clone(), kind.clone(), hash.clone()) {
                return Err(format!("Cargo published conflicting unit identities at {}", path.display()));
            }
            continue;
        }
        published_artifacts.entry(key).or_default()
            .push(json!({"kind": kind, "path": path, "sha256": hash}));
    }
    for outputs in published_artifacts.values_mut() {
        outputs.sort_by_cached_key(Value::to_string);
    }
    publish(
        &snapshot,
        &json!({"complete": true, "request": executor.request,
        "graph": executor.graph, "graph_inputs": executor.graph_inputs, "graph_sha256": executor.graph_sha256,
        "configuration_sha256": executor.configuration_sha256, "cwd": executor.cwd,
        "producer": executor.producer, "facts": facts, "published_artifacts": published_artifacts, "cargo": cargo_exe,
        "cargo_sha256": crate::digest(&fs::read(&cargo_exe).map_err(|e| e.to_string())?)}),
    )
}

fn restore_discovery(arguments: &[String]) -> Result<(), String> {
    let context: Value = serde_json::from_str(arguments.get(2).ok_or("missing discovery context")?)
        .map_err(|e| e.to_string())?;
    let original = context["original"].as_str().ok_or("missing invocation PATH")?;
    let injected = context["injected"].as_str().ok_or("missing discovery directory")?;
    let separator = if cfg!(windows) { ';' } else { ':' };
    let expected = format!("{original}{separator}{injected}");
    let actual = std::env::var("PATH").map_err(|e| e.to_string())?;
    let positions: Vec<_> = actual
        .match_indices(&expected)
        .filter(|(start, matched)| {
            (*start == 0 || actual.as_bytes().get(start - 1) == Some(&(separator as u8)))
                && (*start + matched.len() == actual.len()
                    || actual.as_bytes().get(*start + matched.len()) == Some(&(separator as u8)))
        })
        .collect();
    let [(start, matched)] = positions.as_slice() else {
        return Err("cannot restore invocation PATH after Cargo discovery; expose the matching helper on the ordinary command path".into());
    };
    let restored = format!("{}{original}{}", &actual[..*start], &actual[*start + matched.len()..]);
    let status = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg("build")
        .args(&arguments[3..])
        .env("PATH", restored)
        .status()
        .map_err(|e| e.to_string())?;
    std::process::exit(status.code().unwrap_or(1));
}
