use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::core::builder::{Builder, Kind};
use crate::core::config::DryRun;
use crate::core::session::Mode;
use crate::utils::exec::{BootstrapCommand, CommandFingerprint};

use sdk::{command_context, read_json};

#[allow(dead_code, reason = "shared with the compiler shim and installed source helper")]
#[path = "launcher.rs"]
mod launcher;

#[path = "bootstrap_config.rs"]
#[allow(dead_code, reason = "shared SDK capture configuration")]
mod config;

#[path = "sdk.rs"]
#[allow(dead_code, reason = "shared by the Python frontend helper and Rust bootstrap collector")]
mod sdk;

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
    frontend: Value,
    prepared: BTreeMap<String, (CommandFingerprint, Value)>,
}

impl Capture {
    pub(crate) fn prepare(&self, builder: &Builder<'_>) -> Result<(), String> {
        let Some(request_path) = &builder.config.ask_source_request else {
            return Ok(());
        };
        if builder.kind != Kind::Build {
            return Err("--ask-source-request requires a build".into());
        }
        if matches!(builder.config.get_dry_run(), DryRun::UserSelected) {
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
        let request_sha256 = launcher::digest(&bytes);
        let frontend = sdk::completed_frontend(&directory, &invocation, &request_path, &request_sha256)?;
        if builder.config.dry_run() {
            return Ok(());
        }
        *self.run.borrow_mut() = Some(Run {
            invocation,
            request_path,
            request_sha256,
            frontend,
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
        let frontend = sdk::completed_frontend(&run.directory, &run.invocation,
            &run.request_path, &run.request_sha256)?;
        if frontend != run.frontend {
            return Err("frontend receipt changed during SDK provisioning".into());
        }
        let frontend_context = frontend["context"].clone();
        let frontend_key = launcher::digest(&serde_json::to_vec(&frontend_context).map_err(|e| e.to_string())?);
        let mut builds = BTreeMap::from([(frontend_key.clone(), frontend)]);
        let mut covered = BTreeSet::new();
        for (key, (fingerprint, context)) in &run.prepared {
            let original = fingerprint.command();
            builder.info(&format!("Capturing SDK {} graph: stage{} {}",
                context["mode"], context["stage"], context["target"]));
            let descriptor = sdk::capture(&original, context, &run.request,
                &run.request_sha256, &run.directory)?;
            builds.insert(key.clone(), descriptor);
        }
        for descriptor in builds.values() {
            let document = read_json(Path::new(descriptor["path"].as_str().ok_or("missing SDK native graph")?))?;
            for (unit, _) in document["facts"].as_object().ok_or("missing SDK unit facts")? {
                let source = document["graph"]["units"][unit]["unit"]["target"]["src_path"]
                    .as_str().ok_or("missing SDK unit root")?;
                covered.insert(fs::canonicalize(source).map_err(|e| e.to_string())?);
            }
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
        let mut contexts: BTreeMap<_, _> =
            run.prepared.iter().map(|(key, (_, context))| (key.clone(), context.clone())).collect();
        contexts.insert(frontend_key, frontend_context);
        let document = json!({"complete": true, "invocation": run.invocation, "request": run.request, "request_path": run.request_path,
            "request_sha256": run.request_sha256, "contexts": contexts, "builds": builds});
        let partial = run.directory.join("builds.partial");
        fs::write(&partial, serde_json::to_vec(&document).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        fs::rename(partial, run.directory.join("builds.json")).map_err(|e| e.to_string())?;
        Ok(())
    }
}
