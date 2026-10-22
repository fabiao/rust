use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub mod control {
    // Private per-invocation transport, removed before bootstrap interprets Rustc arguments.
    pub const PREFIX: &str = "--ask-source-capture=";
}

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn environment_key(name: &str) -> String {
    if cfg!(windows) { name.to_uppercase() } else { name.to_owned() }
}

pub fn environment(overrides: &BTreeMap<String, Option<OsString>>) -> Result<Value, String> {
    let mut hashes = BTreeMap::new();
    for (name, value) in std::env::vars_os() {
        let name = environment_key(name.to_str().ok_or("launcher environment key is not UTF-8")?);
        hashes.insert(
            name,
            digest(value.to_str().ok_or("launcher environment value is not UTF-8")?.as_bytes()),
        );
    }
    for (name, value) in overrides {
        let name = environment_key(name);
        if let Some(value) = value {
            hashes.insert(
                name,
                digest(value.to_str().ok_or("launcher environment value is not UTF-8")?.as_bytes()),
            );
        } else {
            hashes.remove(&name);
        }
    }
    Ok(json!(hashes))
}

pub fn validate_request(request: &Value) -> Result<(), String> {
    let fields = request.as_object().ok_or("capture request must be an object")?;
    const REQUIRED: [&str; 5] = ["directory", "sources", "candidate_manifests", "graph", "compiler_shims"];
    if REQUIRED.iter().any(|key| !fields.contains_key(*key))
        || fields.keys().any(|key| !REQUIRED.contains(&key.as_str()) && key != "unit_store")
    {
        return Err(
            "capture request requires directory, sources, candidate_manifests, graph and compiler_shims".into()
        );
    }
    if request.get("unit_store").is_some_and(|store| store.as_str().is_none_or(|path| !Path::new(path).is_absolute())) {
        return Err("capture unit store must be an absolute path".into());
    }
    if request["directory"].as_str().is_none_or(|path| !Path::new(path).is_absolute())
        || ["sources", "candidate_manifests"].iter().any(|field| request[*field].as_array().is_none_or(|sources| {
            sources
                .iter()
                .any(|source| source.as_str().is_none_or(|path| !Path::new(path).is_absolute()))
        }))
        || request["graph"].as_object().is_none_or(|graph| graph.is_empty())
        || !request["compiler_shims"].is_object()
    {
        return Err(
            "capture requires absolute paths, an explicit graph and compiler_shims map".into()
        );
    }
    Ok(())
}

pub fn snapshot(
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    overrides: &BTreeMap<String, Option<OsString>>,
) -> Result<Value, String> {
    let program = fs::canonicalize(program).map_err(|e| e.to_string())?;
    let cwd = fs::canonicalize(cwd).map_err(|e| e.to_string())?;
    let mut arguments = Vec::new();
    let mut response_files = BTreeMap::new();
    for arg in args {
        let arg = arg.to_str().ok_or("launcher argument is not UTF-8")?;
        if let Some(path) = arg.strip_prefix('@') {
            let path = Path::new(path);
            let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
            arguments.extend(text.lines().map(str::to_owned));
            response_files.insert(path, digest(&bytes));
        } else {
            arguments.push(arg.to_owned());
        }
    }
    Ok(json!({"program": program, "sha256": digest(&fs::read(&program).map_err(|e| e.to_string())?),
        "arguments": arguments, "cwd": cwd, "environment": environment(overrides)?, "response_files": response_files}))
}

pub fn validate(receipt: &Value, request: &Value) -> Result<(), String> {
    let name = receipt["program"].as_str().ok_or("missing launcher program")?;
    let expected = request["compiler_shims"]
        .get(name)
        .and_then(Value::as_str)
        .ok_or("launcher is not an explicitly authorized compiler shim")?;
    if receipt["sha256"] != expected
        || digest(&fs::read(name).map_err(|e| e.to_string())?) != expected
    {
        return Err("compiler shim identity changed".into());
    }
    for (name, expected) in
        receipt["response_files"].as_object().ok_or("missing launcher response inputs")?
    {
        if digest(&fs::read(name).map_err(|e| e.to_string())?)
            != expected.as_str().ok_or("invalid launcher input hash")?
        {
            return Err(format!("launcher response input changed: {name}"));
        }
    }
    Ok(())
}
