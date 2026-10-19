use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::{config, launcher};

pub(super) fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub(super) fn command_context(command: &Command) -> Result<Value, String> {
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


pub(super) fn capture(original: &Command, context: &Value, request: &Value,
    request_sha256: &str, directory: &Path) -> Result<Value, String> {
    let key = launcher::digest(&serde_json::to_vec(context).map_err(|e| e.to_string())?);
    if command_context(&original)? != context["invocation"] {
        return Err("prepared SDK Cargo inputs changed during provisioning".into());
    }
    let arguments: Vec<_> = original.get_args().collect();
    if arguments.first().is_none_or(|arg| arg.to_str() != Some("build")) {
        return Err("SDK capture requires the original Cargo build command".into());
    }
    let frontend = context["mode"] == config::build_mode::FRONTEND;
    let compiler = PathBuf::from(effective_env(&original, if frontend { "RUSTC" } else { "RUSTC_REAL" })
        .ok_or("missing SDK compiler selection")?);
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
    let compiler_shims = if frontend {
        if effective_env(original, "RUSTC_WRAPPER").is_some_and(|value| !value.is_empty())
            || effective_env(original, "RUSTC_WORKSPACE_WRAPPER").is_some_and(|value| !value.is_empty()) {
            return Err("frontend capture requires direct compiler ownership; remove nested wrappers".into());
        }
        json!({})
    } else {
        let shim = fs::canonicalize(PathBuf::from(effective_env(original, "RUSTC_WRAPPER")
            .ok_or("missing bootstrap shim")?)).map_err(|e| e.to_string())?;
        let hash = launcher::digest(&fs::read(&shim).map_err(|e| e.to_string())?);
        json!({(shim.to_str().ok_or("non-UTF-8 shim path")?): hash})
    };
    let directory = directory.join(&key);
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let request_path = directory.join("request.json");
    let request = json!({"directory": directory, "sources": request["sources"],
        "candidate_manifests": request["candidate_manifests"],
        "graph": {"selected": request["graph"], "bootstrap": context, "seed_sha256": request_sha256},
        "compiler_shims": compiler_shims});
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
    let identity = original;
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
    if !capture.status().map_err(|e| e.to_string())?.success() {
        return Err(format!("SDK capture failed for {}", context["target"]));
    }
    let path = directory.join("build.json");
    let document = read_json(&path)?;
    if document["complete"] != true || document["request"] != request {
        return Err("SDK capture omitted its complete native graph".into());
    }
    Ok(json!({"path": path, "sha256": launcher::digest(&fs::read(&path).map_err(|e| e.to_string())?),
        "request_path": request_path, "request_sha256": launcher::digest(&request_bytes), "context": context,
        "compiler": compiler, "helper": helper, "helper_sha256": helper_sha256}))
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

fn frontend_receipt(directory: &Path, invocation: &str) -> PathBuf {
    directory.join(format!("frontend-{}.json", launcher::digest(invocation.as_bytes())))
}

pub(super) fn frontend(arguments: &[String]) -> Result<(), String> {
    let [request_path, invocation, host, separator, program, rest @ ..] = arguments else {
        return Err("sdk-frontend requires seed, invocation, host and -- <prepared Cargo command>".into());
    };
    if invocation.is_empty() || host.is_empty() || separator != "--" {
        return Err("frontend capture requires explicit invocation, host and Cargo command".into());
    }
    let request_path = fs::canonicalize(request_path).map_err(|e| e.to_string())?;
    let bytes = fs::read(&request_path).map_err(|e| e.to_string())?;
    let request: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    launcher::validate_request(&request)?;
    if request["compiler_shims"].as_object().is_none_or(|map| !map.is_empty()) {
        return Err("frontend seed must leave compiler shim authority to bootstrap".into());
    }
    let directory = PathBuf::from(request["directory"].as_str().ok_or("missing SDK directory")?);
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let destination = frontend_receipt(&directory, invocation);
    if destination.exists() {
        fs::remove_file(&destination).map_err(|e| e.to_string())?;
    }
    let request_sha256 = launcher::digest(&bytes);
    let mut original = Command::new(program);
    original.args(rest).current_dir(std::env::current_dir().map_err(|e| e.to_string())?);
    let context = json!({"invocation": command_context(&original)?,
        "stage": 0, "compiler_host": host, "target": host, "mode": config::build_mode::FRONTEND});
    let descriptor = capture(&original, &context, &request, &request_sha256, &directory)?;
    if fs::read(&request_path).map_err(|e| e.to_string())? != bytes {
        return Err("SDK seed changed during frontend capture".into());
    }
    let receipt = json!({"invocation": invocation, "request_path": request_path,
        "request_sha256": request_sha256, "descriptor": descriptor});
    let partial = destination.with_extension("partial");
    fs::write(&partial, serde_json::to_vec(&receipt).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    fs::rename(partial, destination).map_err(|e| e.to_string())?;
    Ok(())
}

pub(super) fn completed_frontend(directory: &Path, invocation: &str,
    request_path: &Path, request_sha256: &str) -> Result<Value, String> {
    let receipt = read_json(&frontend_receipt(directory, invocation))
        .map_err(|e| format!("frontend capture receipt unavailable; invoke SDK capture through x.py: {e}"))?;
    if receipt["invocation"] != invocation || receipt["request_path"] != json!(request_path)
        || receipt["request_sha256"] != request_sha256 {
        return Err("frontend receipt does not match this SDK invocation and seed".into());
    }
    let descriptor = &receipt["descriptor"];
    let context = &descriptor["context"];
    let key = launcher::digest(&serde_json::to_vec(context).map_err(|e| e.to_string())?);
    if context["mode"] != config::build_mode::FRONTEND || context["stage"] != 0
        || context["target"].as_str().is_none_or(str::is_empty)
        || context["target"] != context["compiler_host"] {
        return Err("invalid prepared frontend command identity".into());
    }
    for (field, filename, hash) in [("path", "build.json", "sha256"),
        ("request_path", "request.json", "request_sha256")] {
        let path = directory.join(&key).join(filename);
        if descriptor[field] != json!(path)
            || descriptor[hash] != launcher::digest(&fs::read(&path).map_err(|e| e.to_string())?) {
            return Err("frontend native graph or request changed before SDK collection".into());
        }
    }
    Ok(descriptor.clone())
}
