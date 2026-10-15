use std::fs;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};

struct Request {
    directory: PathBuf,
    sources: Vec<PathBuf>,
    graph: serde_json::Value,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let compiler = args.first().ok_or("missing Cargo compiler argument")?;
    let request_path = std::env::var_os("ASK_SOURCE_CAPTURE_REQUEST")
        .ok_or("Cargo capture requires ASK_SOURCE_CAPTURE_REQUEST")?;
    let request_bytes = fs::read(&request_path).map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_slice(&request_bytes).map_err(|e| e.to_string())?;
    let fields = value.as_object().ok_or("capture request must be an object")?;
    if fields.len() != 3 || fields.keys().any(|key| !matches!(key.as_str(), "directory" | "sources" | "graph")) {
        return Err("capture request requires only directory, sources and graph".into());
    }
    let request = Request {
        directory: serde_json::from_value(value["directory"].clone()).map_err(|e| e.to_string())?,
        sources: serde_json::from_value(value["sources"].clone()).map_err(|e| e.to_string())?,
        graph: value["graph"].clone(),
    };
    if !request.directory.is_absolute() || request.sources.iter().any(|path| !path.is_absolute()) {
        return Err("capture directory and owned source paths must be absolute".into());
    }
    if !request.graph.is_object() || request.graph.as_object().is_none_or(|graph| graph.is_empty()) {
        return Err("capture requires an explicit selected graph context".into());
    }
    let mut owned = false;
    for argument in args.iter().skip(1).filter(|argument| argument.ends_with(".rs")) {
        let Ok(path) = fs::canonicalize(argument) else { continue };
        for source in &request.sources {
            let source = fs::canonicalize(source)
                .map_err(|e| format!("owned source {}: {e}", source.display()))?;
            owned |= path == source;
        }
    }
    if !owned {
        let status = Command::new(compiler).args(&args[1..]).status().map_err(|e| e.to_string())?;
        std::process::exit(status.code().unwrap_or(1));
    }
    let compiler = fs::canonicalize(compiler).map_err(|e| e.to_string())?;
    let helper = compiler.with_file_name(if cfg!(windows) { "asksource.exe" } else { "asksource" });
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut key = Sha256::new();
    key.update(&request_bytes);
    key.update(serde_json::to_vec(&(args, cwd)).map_err(|e| e.to_string())?);
    fs::create_dir_all(&request.directory).map_err(|e| e.to_string())?;
    let destination = request.directory.join(format!("{}.json", hex::encode(key.finalize())));
    let status = Command::new(&helper).arg("capture").arg(&compiler).args(&args[1..])
        .env("ASK_SOURCE_CAPTURE", destination)
        .env("ASK_SOURCE_CAPTURE_REQUEST_SHA256", crate::digest(&request_bytes))
        .status().map_err(|e| format!("matching compiler helper {}: {e}", helper.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}
