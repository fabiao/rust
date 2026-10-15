use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use cargo::compiler::{BuildRunner, CompileMode, DefaultExecutor, Executor, Unit, UnitInterner};
use cargo::util::CargoResult;
use cargo::util::command_prelude::*;
use cargo::workspace::{PackageId, Target};
use cargo_util::ProcessBuilder;
use serde_json::{Value, json};

#[path = "../../cargo/src/bin/cargo/commands/build.rs"]
#[allow(dead_code)]
mod build_command;

fn descriptor(unit: &Unit) -> Value {
    json!({"package": unit.pkg.package_id().to_spec(), "manifest": unit.pkg.manifest_path(),
        "target": unit.target, "profile": unit.profile, "platform": unit.kind,
        "mode": unit.mode, "features": unit.features, "rustflags": &*unit.rustflags,
        "rustdocflags": &*unit.rustdocflags, "standard_library": unit.is_std,
        "artifact": unit.artifact.is_true(),
        "artifact_target": unit.artifact_target_for_features.as_ref().map(|target| target.rustc_target()),
        "skip_non_compile_time_dependency": unit.skip_non_compile_time_dep,
        "links_overrides_sha256": crate::digest(format!("{:?}", unit.links_overrides).as_bytes())})
}

fn base_key(unit: &Unit) -> String {
    crate::digest(descriptor(unit).to_string().as_bytes())
}

struct GraphSnapshot {
    value: Value,
    keys: HashMap<Unit, String>,
}

fn graph_snapshot(
    graph: &cargo::compiler::unit_graph::UnitGraph,
    roots: &[Unit],
) -> Result<GraphSnapshot, String> {
    fn visit(
        unit: &Unit,
        graph: &cargo::compiler::unit_graph::UnitGraph,
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
            let key = visit(&edge.unit, graph, keys, active, units)?;
            edges.push(json!({"unit": key, "extern_name": edge.extern_crate_name,
                "dependency_name": edge.dep_name, "public": edge.public,
                "noprelude": edge.noprelude, "nounused": edge.nounused}));
        }
        edges.sort_by_cached_key(Value::to_string);
        let value = json!({"unit": descriptor(unit), "dependencies": edges});
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
        visit(unit, graph, &mut keys, &mut active, &mut units)?;
    }
    let roots: BTreeSet<_> = roots
        .iter()
        .map(|unit| keys.get(unit).cloned().ok_or("missing selected Cargo root unit"))
        .collect::<Result<_, _>>()?;
    Ok(GraphSnapshot { value: json!({"units": units, "roots": roots}), keys })
}

fn configuration_files(value: &cargo::context::ConfigValue, paths: &mut BTreeSet<PathBuf>) {
    use cargo::context::{ConfigValue, Definition};
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

struct CaptureExecutor {
    request: Value,
    request_path: PathBuf,
    directory: PathBuf,
    graph: Value,
    graph_sha256: String,
    configuration_sha256: String,
    cwd: PathBuf,
    graph_inputs: BTreeMap<PathBuf, String>,
    producer: Value,
    selected: BTreeSet<String>,
    selected_by_base: BTreeMap<String, BTreeSet<String>>,
    dispatch: Mutex<BTreeMap<String, String>>,
    errors: Mutex<Vec<String>>,
}

impl CaptureExecutor {
    fn record_path(&self, key: &str) -> PathBuf {
        self.directory.join(format!("{key}.unit.json"))
    }
    fn fact_path(&self, key: &str) -> PathBuf {
        self.directory.join(format!("{key}.facts.json"))
    }

    fn valid(&self, key: &str, command: Option<&ProcessBuilder>) -> bool {
        let validate = || -> Option<()> {
            for (path, hash) in &self.graph_inputs {
                if crate::digest(&fs::read(path).ok()?) != *hash {
                    return None;
                }
            }
            let record = read_json(&self.record_path(key)).ok()?;
            let facts = read_json(&self.fact_path(key)).ok()?;
            if record.get("request")? != &self.request
                || record.get("graph_sha256")?.as_str()? != self.graph_sha256
                || record.get("producer")? != &self.producer
                || facts.get("complete")? != true
                || facts.get("capture_request")? != &self.request
            {
                return None;
            }
            if record.get("facts_sha256")?.as_str()?
                != crate::digest(&fs::read(self.fact_path(key)).ok()?)
            {
                return None;
            }
            let cwd = Path::new(facts.get("cwd")?.as_str()?);
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
        if !self.selected_by_base.contains_key(&base_key(unit)) {
            return;
        }
        let snapshot = match graph_snapshot(&runner.bcx.unit_graph, &runner.bcx.roots) {
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
            .get(&base_key(unit))
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
        let compiler = fs::canonicalize(Path::new(cmd.get_program()))?;
        let helper = compiler.with_file_name(if cfg!(windows) {
            "cargo-asksource.exe"
        } else {
            "cargo-asksource"
        });
        let mut compiler_args = vec![
            std::ffi::OsString::from("capture"),
            "--output".into(),
            self.fact_path(&key).into_os_string(),
            "--request".into(),
            self.request_path.clone().into_os_string(),
            "--request-sha256".into(),
            crate::digest(&fs::read(&self.request_path)?).into(),
            "--".into(),
            compiler.into_os_string(),
        ];
        compiler_args.extend(args.into_iter().cloned());
        captured.program(helper).args_replace(&compiler_args);
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
        let record = json!({"request": self.request, "graph_sha256": self.graph_sha256, "producer": self.producer,
            "facts_sha256": crate::digest(&fs::read(self.fact_path(&key))?),
            "environment_inputs": facts["environment_inputs"],
            "environment_overrides": overrides});
        publish(&record_path, &record).map_err(cargo::util::errors::internal)?;
        if !self.valid(&key, Some(cmd)) {
            fs::remove_file(&record_path)?;
            return Err(cargo::util::errors::internal(
                "compiler capture inputs changed during build",
            ));
        }
        Ok(())
    }
}

pub fn run(arguments: &[String]) -> Result<(), String> {
    if arguments.get(1).is_some_and(|argument| argument == "--discovery-context") {
        return restore_discovery(arguments);
    }
    let mut cli = build_command::cli()
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
    let fields = request.as_object().ok_or("capture request must be an object")?;
    if fields.len() != 3
        || fields.keys().any(|key| !matches!(key.as_str(), "directory" | "sources" | "graph"))
    {
        return Err("capture request requires only directory, sources and graph".into());
    }
    let directory =
        PathBuf::from(request["directory"].as_str().ok_or("missing capture directory")?);
    let sources: BTreeSet<PathBuf> =
        serde_json::from_value(request["sources"].clone()).map_err(|e| e.to_string())?;
    if !directory.is_absolute()
        || sources.iter().any(|path| !path.is_absolute())
        || request["graph"].as_object().is_none_or(|graph| graph.is_empty())
    {
        return Err("capture requires absolute paths and an explicit selected graph context".into());
    }
    let canonical_sources: BTreeSet<_> = sources
        .iter()
        .map(|source| {
            fs::canonicalize(source).map_err(|e| format!("owned source {}: {e}", source.display()))
        })
        .collect::<Result<_, _>>()?;
    let cargo_exe =
        PathBuf::from(std::env::var_os("CARGO").ok_or(
            "invoke through cargo asksource build so Cargo supplies its actual executable",
        )?);
    let version = Command::new(&cargo_exe).arg("-Vv").output().map_err(|e| e.to_string())?;
    let embedded = cargo::version().commit_info.ok_or("embedded Cargo has no source identity")?;
    if !version.status.success()
        || !String::from_utf8_lossy(&version.stdout)
            .lines()
            .any(|line| line == format!("commit-hash: {}", embedded.commit_hash))
    {
        return Err("selected Cargo does not match embedded Cargo; rebuild asksource for the selected producer".into());
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let producer = json!({"path": cargo_exe,
        "executor": executable, "executor_sha256": crate::digest(&fs::read(&executable).map_err(|e| e.to_string())?),
        "sha256": crate::digest(&fs::read(&cargo_exe).map_err(|e| e.to_string())?),
        "commit": embedded.commit_hash});
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
    let graph_snapshot = graph_snapshot(&bcx.unit_graph, &bcx.roots)?;
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
    for unit in
        bcx.unit_graph.keys().filter(|unit| !unit.mode.is_run_custom_build()).filter(|unit| {
            unit.target.src_path().path().is_some_and(|path| {
                fs::canonicalize(path).is_ok_and(|path| canonical_sources.contains(&path))
            })
        })
    {
        let key =
            graph_snapshot.keys.get(unit).ok_or("selected unit has no semantic identity")?.clone();
        selected.insert(key.clone());
        selected_by_base.entry(base_key(unit)).or_default().insert(key);
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
    let executor = Arc::new(CaptureExecutor {
        request,
        request_path,
        directory,
        graph,
        graph_sha256,
        configuration_sha256,
        cwd,
        graph_inputs,
        producer,
        selected,
        selected_by_base,
        dispatch: Mutex::new(BTreeMap::new()),
        errors: Mutex::new(Vec::new()),
    });
    drop(bcx);
    let exec: Arc<dyn Executor> = executor.clone();
    cargo::ops::compile_with_exec(&ws, &options, &exec).map_err(|e| e.to_string())?;
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
    publish(
        &snapshot,
        &json!({"complete": true, "request": executor.request,
        "graph": executor.graph, "graph_inputs": executor.graph_inputs, "graph_sha256": executor.graph_sha256,
        "configuration_sha256": executor.configuration_sha256, "cwd": executor.cwd,
        "producer": executor.producer, "facts": facts, "cargo": cargo_exe,
        "cargo_sha256": crate::digest(&fs::read(&cargo_exe).map_err(|e| e.to_string())?)}),
    )
}

pub fn identity() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    println!(
        "{}",
        json!({"path": executable,
        "sha256": crate::digest(&fs::read(&executable).map_err(|e| e.to_string())?)})
    );
    Ok(())
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
