use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::core::builder::{Builder, Kind};
use crate::core::session::Mode;
use crate::utils::exec::{BootstrapCommand, CommandFingerprint};

#[allow(dead_code, reason = "shared with the compiler shim and installed source helper")]
#[path = "launcher.rs"]
mod launcher;

#[path = "bootstrap_config.rs"]
mod config;

#[derive(Default)]
pub(crate) struct Capture {
    run: RefCell<Option<Run>>,
}

struct Run {
    invocation: String,
    request_path: PathBuf,
    request_sha256: String,
    request: Value,
    directory: PathBuf,
    _lock: File,
    prepared: BTreeMap<String, (CommandFingerprint, Value)>,
}

fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn command_context(command: &Command) -> Result<Value, String> {
    let overlays = command
        .get_envs()
        .map(|(name, value)| {
            Ok((
                name.to_str().ok_or("non-UTF-8 Cargo environment key")?.to_owned(),
                value.map(ToOwned::to_owned),
            ))
        })
        .collect::<Result<_, String>>()?;
    launcher::snapshot(
        Path::new(command.get_program()),
        &command.get_args().map(ToOwned::to_owned).collect::<Vec<_>>(),
        command.get_current_dir().unwrap_or(&std::env::current_dir().map_err(|e| e.to_string())?),
        &overlays,
    )
}

fn effective_env(command: &Command, name: &str) -> Option<OsString> {
    command
        .get_envs()
        .find(|(key, _)| {
            launcher::environment_key(&key.to_string_lossy()) == launcher::environment_key(name)
        })
        .map(|(_, value)| value.map(ToOwned::to_owned))
        .unwrap_or_else(|| std::env::var_os(name))
}

impl Capture {
    pub(crate) fn prepare(&self, builder: &Builder<'_>) -> Result<(), String> {
        let Some(request_path) = &builder.config.ask_source_request else {
            return Ok(());
        };
        if builder.kind != Kind::Build {
            return Err("--ask-source-request requires a build".into());
        }
        if builder.config.dry_run() {
            return Ok(());
        }
        let invocation = builder
            .config
            .ask_source_run
            .as_ref()
            .filter(|token| !token.is_empty())
            .ok_or("SDK capture requires a nonempty invocation token")?
            .clone();
        let request_path = fs::canonicalize(request_path).map_err(|e| e.to_string())?;
        let bytes = fs::read(&request_path).map_err(|e| e.to_string())?;
        let request: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        launcher::validate_request(&request)?;
        if request["compiler_shims"].as_object().is_none_or(|map| !map.is_empty()) {
            return Err(
                "SDK seed requires an empty shim map; bootstrap owns the actual shim identities"
                    .into(),
            );
        }
        let directory =
            PathBuf::from(request["directory"].as_str().ok_or("missing capture directory")?);
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("sdk.lock"))
            .map_err(|e| e.to_string())?;
        lock.lock().map_err(|e| e.to_string())?;
        let complete = directory.join("builds.json");
        if complete.exists() {
            fs::remove_file(complete).map_err(|e| e.to_string())?;
        }
        *self.run.borrow_mut() = Some(Run {
            invocation,
            request_path,
            request_sha256: launcher::digest(&bytes),
            request,
            directory,
            _lock: lock,
            prepared: BTreeMap::new(),
        });
        Ok(())
    }

    pub(crate) fn record(
        &self,
        command: &BootstrapCommand,
        stage: u32,
        host: &str,
        target: &str,
        mode: Mode,
    ) -> Result<(), String> {
        let mut run = self.run.borrow_mut();
        let Some(run) = run.as_mut() else {
            return Ok(());
        };
        let fingerprint = command.fingerprint();
        let mode = match mode {
            Mode::Std => config::build_mode::STD,
            Mode::Rustc => config::build_mode::RUSTC,
            Mode::Codegen => config::build_mode::CODEGEN,
            Mode::ToolBootstrap => config::build_mode::TOOL_BOOTSTRAP,
            Mode::ToolTarget => config::build_mode::TOOL_TARGET,
            Mode::ToolStd => config::build_mode::TOOL_STD,
            Mode::ToolRustcPrivate => config::build_mode::TOOL_RUSTC_PRIVATE,
        };
        let context = json!({"invocation": command_context(&fingerprint.command())?,
            "stage": stage, "compiler_host": host, "target": target, "mode": mode});
        let key = launcher::digest(&serde_json::to_vec(&context).map_err(|e| e.to_string())?);
        run.prepared.insert(key, (fingerprint, context));
        Ok(())
    }

    pub(crate) fn finish(&self, builder: &Builder<'_>) -> Result<(), String> {
        let mut state = self.run.borrow_mut();
        let Some(run) = state.take() else {
            return Ok(());
        };
        if run.prepared.is_empty() {
            return Err("no SDK Cargo build commands were selected".into());
        }
        let mut builds = BTreeMap::new();
        let mut covered = BTreeSet::new();
        for (key, (fingerprint, context)) in &run.prepared {
            let original = fingerprint.command();
            if command_context(&original)? != context["invocation"] {
                return Err("prepared SDK Cargo inputs changed during provisioning".into());
            }
            let arguments: Vec<_> = original.get_args().collect();
            if arguments.first().is_none_or(|arg| arg.to_str() != Some("build")) {
                return Err("SDK capture requires the original Cargo build command".into());
            }
            let compiler = PathBuf::from(
                effective_env(&original, "RUSTC_REAL").ok_or("missing SDK compiler selection")?,
            );
            let compiler = fs::canonicalize(&compiler).map_err(|e| e.to_string())?;
            let helper = compiler.with_file_name(if cfg!(windows) {
                "cargo-asksource.exe"
            } else {
                "cargo-asksource"
            });
            let helper_sha256 = launcher::digest(
                &fs::read(&helper)
                    .map_err(|e| format!("provision matching helper {}: {e}", helper.display()))?,
            );
            let shim = fs::canonicalize(PathBuf::from(
                effective_env(&original, "RUSTC_WRAPPER").ok_or("missing bootstrap shim")?,
            ))
            .map_err(|e| e.to_string())?;
            let shim_sha256 = launcher::digest(&fs::read(&shim).map_err(|e| e.to_string())?);
            let directory = run.directory.join(key);
            fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let request_path = directory.join("request.json");
            let request = json!({"directory": directory, "sources": run.request["sources"],
                "graph": {"selected": run.request["graph"], "bootstrap": context, "seed_sha256": run.request_sha256},
                "compiler_shims": {(shim.to_str().ok_or("non-UTF-8 shim path")?): shim_sha256}});
            let request_bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
            fs::write(&request_path, &request_bytes).map_err(|e| e.to_string())?;
            let original_path =
                effective_env(&original, "PATH").ok_or("Cargo discovery requires PATH")?;
            let original_path =
                original_path.to_str().ok_or("Cargo discovery PATH is not UTF-8")?;
            let helper_directory = helper.parent().ok_or("helper has no parent")?;
            let mut paths: Vec<_> = std::env::split_paths(original_path).collect();
            paths.push(helper_directory.to_path_buf());
            let injected_path = std::env::join_paths(paths).map_err(|e| e.to_string())?;
            let identity = fingerprint.command();
            // Identity uses the same producer and context, replacing only its Cargo subcommand.
            let mut identity_args = Command::new(identity.get_program());
            identity_args.args(["asksource", "identity"]);
            copy_context(&identity, &mut identity_args);
            identity_args.env("PATH", &injected_path);
            let identity = identity_args.output().map_err(|e| e.to_string())?;
            if !identity.status.success() {
                return Err(format!(
                    "cannot discover SDK source helper: {}",
                    String::from_utf8_lossy(&identity.stderr)
                ));
            }
            let identity: Value =
                serde_json::from_slice(&identity.stdout).map_err(|e| e.to_string())?;
            if identity["sha256"] != helper_sha256
                || identity["path"].as_str().is_none_or(|path| Path::new(path) != helper)
            {
                return Err("Cargo discovered a different SDK source helper".into());
            }
            let mut capture = Command::new(original.get_program());
            capture
                .args(["asksource", "build", "--discovery-context"])
                .arg(json!({"original": original_path, "injected": helper_directory}).to_string())
                .arg("--capture-request")
                .arg(&request_path)
                .args(arguments.iter().skip(1));
            copy_context(&original, &mut capture);
            capture.env("PATH", injected_path);
            builder.info(&format!(
                "Capturing SDK {} graph: stage{} {}",
                context["mode"], context["stage"], context["target"]
            ));
            if !BootstrapCommand::from(capture).allow_failure().run(&builder.config.exec_ctx) {
                return Err(format!("SDK capture failed for {}", context["target"]));
            }
            let path = directory.join("build.json");
            let document = read_json(&path)?;
            if document["complete"] != true || document["request"] != request {
                return Err("SDK capture omitted its complete native graph".into());
            }
            for (unit, _) in document["facts"].as_object().ok_or("missing SDK unit facts")? {
                let source = document["graph"]["units"][unit]["unit"]["target"]["src_path"]
                    .as_str()
                    .ok_or("missing SDK unit root")?;
                covered.insert(fs::canonicalize(source).map_err(|e| e.to_string())?);
            }
            builds.insert(key.clone(), json!({"path": path, "sha256": launcher::digest(&fs::read(&path).map_err(|e| e.to_string())?),
                "request_path": request_path, "request_sha256": launcher::digest(&request_bytes), "context": context,
                "compiler": compiler, "helper": helper, "helper_sha256": helper_sha256}));
        }
        for source in run.request["sources"].as_array().ok_or("missing SDK source roots")? {
            let source = source.as_str().ok_or("invalid SDK source root")?;
            if !covered.contains(&fs::canonicalize(source).map_err(|e| e.to_string())?) {
                return Err(format!(
                    "requested owned root is absent from selected SDK Cargo graphs: {source}"
                ));
            }
        }
        if launcher::digest(&fs::read(&run.request_path).map_err(|e| e.to_string())?)
            != run.request_sha256
        {
            return Err("SDK seed changed during capture".into());
        }
        let contexts: BTreeMap<_, _> =
            run.prepared.iter().map(|(key, (_, context))| (key, context)).collect();
        let document = json!({"complete": true, "invocation": run.invocation, "request": run.request, "request_path": run.request_path,
            "request_sha256": run.request_sha256, "contexts": contexts, "builds": builds});
        let partial = run.directory.join("builds.partial");
        fs::write(&partial, serde_json::to_vec(&document).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        fs::rename(partial, run.directory.join("builds.json")).map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn copy_context(source: &Command, destination: &mut Command) {
    for (name, value) in source.get_envs() {
        if let Some(value) = value {
            destination.env(name, value);
        } else {
            destination.env_remove(name);
        }
    }
    if let Some(cwd) = source.get_current_dir() {
        destination.current_dir(cwd);
    }
}
