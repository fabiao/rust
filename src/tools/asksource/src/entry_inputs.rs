use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rustc_ast::expand::allocator::AllocatorKind;
use rustc_hir::def_id::{CrateNum, LOCAL_CRATE};
use rustc_interface::interface;
use rustc_middle::mono::CodegenUnitNameBuilder;
use rustc_middle::ty::TyCtxt;
use rustc_session::config::{ExternEntry, ExternLocation, Externs, Lto, OutputType};
use rustc_session::search_paths::{PathKind, SearchPath};
use rustc_session::utils::CanonicalizedPath;
use rustc_structures::CrateType;
use serde_json::{Value, json};

pub const BRIDGE: &str = "--ask-entry-input-plan=";

fn default_allocator(tcx: TyCtxt<'_>, krate: CrateNum) -> Result<bool, String> {
    #[cfg(not(feature = "snapshot-driver"))]
    {
        Ok(tcx.has_default_lib_allocator(krate))
    }
    #[cfg(feature = "snapshot-driver")]
    {
        let _ = (tcx, krate);
        Err("entry capture requires the compiler's authoritative default allocator query".into())
    }
}

pub fn codegen_modules(tcx: TyCtxt<'_>, facts: &Value) -> Result<Vec<Value>, String> {
    let mut modules = Vec::new();
    for unit in tcx.collect_and_partition_mono_items(()).codegen_units {
            let mut symbols = Vec::new();
            for (item, _) in unit.items_in_deterministic_order(tcx) {
                if matches!(item, rustc_middle::mono::MonoItem::GlobalAsm(_)) {
                    continue;
                }
                let definition = item.def_id();
                let mut artifacts = BTreeMap::new();
                if !definition.is_local() {
                    let source = tcx.used_crate_source(definition.krate);
                    for path in source.paths().chain(source.sdylib_interface.iter()) {
                        let path = fs::canonicalize(path).map_err(|error| error.to_string())?;
                        let hash = facts["dependency_artifacts"]
                            .get(path.to_str().ok_or("non-UTF8 monomorphization artifact")?)
                            .and_then(Value::as_str)
                            .ok_or("monomorphization artifact was not captured")?;
                        artifacts.insert(path, hash.to_owned());
                    }
                    if artifacts.is_empty() {
                        return Err(
                            "monomorphization definition has no native origin artifacts".into()
                        );
                    }
                }
                symbols.push(json!({"symbol": item.symbol_name(tcx).name,
                    "local": definition.is_local(), "defining_artifacts": artifacts}));
            }
            modules.push(json!({"name": unit.name().to_string(), "codegen_symbols": symbols}));
        }
    Ok(modules)
}

fn rust_entry_wrapper(tcx: TyCtxt<'_>, facts: &Value) -> Result<Value, String> {
    let (main, _) = tcx.entry_fn(())
        .ok_or("participating Rust final entry requires the compiler-generated main wrapper; remove no_main/custom main")?;
    let instance = rustc_middle::ty::Instance::mono(tcx, main);
    let units = tcx.collect_and_partition_mono_items(()).codegen_units;
    let mut selected = units.iter().filter(|unit| {
        if main.is_local() {
            unit.contains_item(&rustc_middle::mono::MonoItem::Fn(instance))
        } else {
            unit.is_primary()
        }
    });
    let unit = selected.next().ok_or("compiler Rust entry wrapper has no codegen unit")?;
    if selected.next().is_some() {
        return Err("compiler Rust entry wrapper has multiple codegen units".into());
    }
    let symbol = tcx.sess.target.entry_name.as_ref();
    if symbol != "main" {
        return Err("participating ASK Rust adapter requires compiler target entry main".into());
    }
    let start = tcx.lang_items().start_fn()
        .ok_or("participating Rust entry requires the std start language item")?;
    if start.is_local() || tcx.crate_name(start.krate).as_str() != "std" {
        return Err("participating Rust entry cannot call an application start language item before GO".into());
    }
    let mut artifacts = BTreeMap::new();
    let libraries = PathBuf::from(facts["sysroot"].as_str().ok_or("missing compiler sysroot")?)
        .join("lib/rustlib").join(tcx.sess.opts.target_triple.tuple()).join("lib");
    for source in tcx.used_crate_source(start.krate).paths() {
        let path = fs::canonicalize(source).map_err(|error| error.to_string())?;
        if !path.starts_with(&libraries) {
            return Err("Rust start language item must come from the admitted std sysroot".into());
        }
        let hash = facts["dependency_artifacts"].get(path.to_str().ok_or("non-UTF8 std artifact")?)
            .and_then(Value::as_str).ok_or("std start artifact was not captured")?;
        artifacts.insert(path, hash.to_owned());
    }
    if artifacts.is_empty() {
        return Err("std start language item has no captured native artifact".into());
    }
    let module = std::path::absolute(tcx.output_filenames(()).temp_path_ext_for_cgu("no-opt.bc", &unit.name().to_string()))
        .map_err(|error| error.to_string())?;
    Ok(json!({"symbol": symbol, "module": module, "std_start_artifacts": artifacts}))
}

pub fn resolved_mir(tcx: TyCtxt<'_>) -> Result<String, String> {
    let mut bytes = Vec::new();
    rustc_middle::mir::pretty::write_mir_pretty(tcx, &mut bytes)
        .map_err(|error| format!("cannot capture actual compiler MIR: {error}"))?;
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

pub struct Capture {
    request: Value,
    request_path: PathBuf,
    request_sha256: String,
    policy: Value,
    realization: Value,
    plan: Option<Value>,
    plan_sha256: Option<String>,
    module: bool,
}

fn absolute(value: &Value, name: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value[name].as_str().ok_or_else(|| format!("missing {name}"))?);
    if !path.is_absolute() {
        return Err(format!("{name} must be absolute"));
    }
    Ok(path)
}

fn remove(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn publish(path: &Path, value: &Value) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    let partial = path.with_extension("partial");
    fs::write(&partial, &bytes).map_err(|error| error.to_string())?;
    fs::rename(partial, path).map_err(|error| error.to_string())?;
    Ok(crate::digest(&bytes))
}

impl Capture {
    pub fn tracked_inputs(&self) -> Result<BTreeMap<PathBuf, String>, String> {
        Ok(BTreeMap::from([
            (self.request_path.clone(), self.request_sha256.clone()),
            (
                absolute(&self.request, "policy_path")?,
                self.request["policy_sha256"].as_str().ok_or("missing policy hash")?.to_owned(),
            ),
        ]))
    }

    pub fn load(
        path: &Path,
        hash: &str,
        facts: &Path,
        arguments: &[String],
    ) -> Result<Self, String> {
        let mut kinds = Vec::new();
        for (index, argument) in arguments.iter().enumerate() {
            if let Some(value) = argument.strip_prefix("--crate-type=") {
                kinds.extend(value.split(','));
            } else if argument == "--crate-type" {
                kinds.extend(
                    arguments.get(index + 1).ok_or("missing explicit crate type")?.split(','),
                );
            }
        }
        let module =
            !kinds.is_empty() && kinds.iter().all(|kind| matches!(*kind, "dylib" | "cdylib"));
        if !module && kinds.as_slice() != ["bin"] {
            return Err("entry capture requires explicit bin or dynamic module crate types".into());
        }
        let bytes = fs::read(path).map_err(|error| error.to_string())?;
        if crate::digest(&bytes) != hash {
            return Err("entry input request changed".into());
        }
        let request: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let fields = request.as_object().ok_or("entry input request must be an object")?;
        if fields.len() != 6
            || fields.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "request_id"
                        | "plan_path"
                        | "receipt_path"
                        | "facts_path"
                        | "policy_path"
                        | "policy_sha256"
                )
            })
        {
            return Err("invalid entry input request fields".into());
        }
        for name in ["request_id", "policy_sha256"] {
            let hash = request[name].as_str().ok_or("missing entry request hash")?;
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("invalid {name}"));
            }
        }
        let plan = absolute(&request, "plan_path")?;
        let receipt = absolute(&request, "receipt_path")?;
        let destination = absolute(&request, "facts_path")?;
        let policy_path = absolute(&request, "policy_path")?;
        if destination != facts
            || [
                plan.clone(),
                receipt.clone(),
                destination.clone(),
                policy_path.clone(),
                path.to_path_buf(),
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .len()
                != 5
        {
            return Err(
                "entry input capture paths must be distinct and match capture output".into()
            );
        }
        let policy_bytes = fs::read(&policy_path).map_err(|error| error.to_string())?;
        if crate::digest(&policy_bytes)
            != request["policy_sha256"].as_str().ok_or("missing policy hash")?
        {
            return Err("entry link policy changed".into());
        }
        let policy = serde_json::from_slice(&policy_bytes).map_err(|error| error.to_string())?;
        for output in [plan, receipt, destination] {
            remove(&output)?;
        }
        Ok(Self {
            request,
            request_path: path.to_path_buf(),
            request_sha256: hash.to_owned(),
            policy,
            realization: Value::Null,
            plan: None,
            plan_sha256: None,
            module,
        })
    }

    pub fn configure(&mut self, config: &mut interface::Config) -> Result<(), String> {
        if config.opts.cg.link_args.iter().any(|arg| arg.starts_with(BRIDGE)) {
            return Err("entry input bridge may only be added by SourceCapture".into());
        }
        config.opts.cg.save_temps = true;
        config.opts.incremental = None;
        config.opts.cg.incremental = None;
        config
            .opts
            .cg
            .link_args
            .push(format!("{BRIDGE}{}", absolute(&self.request, "plan_path")?.display()));
        let mut externs: BTreeMap<_, _> =
            config.opts.externs.iter().map(|(name, entry)| (name.clone(), entry.clone())).collect();
        let mut requested: BTreeMap<String, PathBuf> =
            serde_json::from_value(self.policy["compiler_externs"].clone())
                .map_err(|error| format!("invalid policy compiler_externs: {error}"))?;
        let module = self.module;
        if module {
            requested.clear();
        }
        let client_name = self.policy["client_extern_name"].as_str()
            .ok_or("missing controlled client extern name")?;
        if client_name == "askfdowner" || client_name.is_empty()
            || !client_name.bytes().enumerate().all(|(index, byte)|
                byte == b'_' || byte.is_ascii_alphabetic() || (index != 0 && byte.is_ascii_digit())) {
            return Err("invalid controlled client extern name".into());
        }
        let mut realized = BTreeMap::new();
        for (name, path) in requested {
            if (name != "askfdowner" && name != client_name) || !path.is_absolute() {
                return Err("invalid controlled compiler extern".into());
            }
            let path = fs::canonicalize(path).map_err(|error| error.to_string())?;
            if let Some(existing) = externs.get_mut(&name) {
                let files = existing
                    .files()
                    .ok_or("controlled extern must name an exact archive")?
                    .map(|path| path.canonicalized().to_path_buf())
                    .collect::<Vec<_>>();
                if files.len() != 1 || (name == "askfdowner" && files[0] != path) {
                    return Err("controlled owner extern differs from approved archive".into());
                }
                realized.insert(name.clone(), files);
                existing.force = true;
            } else {
                externs.insert(
                    name.clone(),
                    ExternEntry {
                        location: ExternLocation::ExactPaths(BTreeSet::from([
                            CanonicalizedPath::new(path.clone()),
                        ])),
                        is_private_dep: false,
                        add_prelude: true,
                        nounused_dep: true,
                        force: true,
                    },
                );
                realized.insert(name, vec![path]);
            }
        }
        config.opts.externs = Externs::new(externs);
        let paths: Vec<PathBuf> =
            serde_json::from_value(self.policy["compiler_search_paths"].clone())
                .map_err(|error| format!("invalid compiler_search_paths: {error}"))?;
        for path in &paths {
            if !path.is_absolute() {
                return Err("compiler search path must be absolute".into());
            }
            config
                .opts
                .search_paths
                .push(SearchPath { kind: PathKind::Dependency, dir: path.clone().into() });
        }
        self.realization = json!({"save_temps": true, "incremental": null, "module": module,
            "link_args": config.opts.cg.link_args, "externs": realized,
            "dependency_search_paths": paths});
        Ok(())
    }

    pub fn pending(&mut self, tcx: TyCtxt<'_>, facts: &mut Value) -> Result<(), String> {
        let module = !tcx.crate_types().is_empty()
            && tcx
                .crate_types()
                .iter()
                .all(|kind| matches!(kind, CrateType::Dylib | CrateType::Cdylib));
        if module != self.module {
            return Err("actual crate type differs from entry capture intent".into());
        }
        if tcx.crate_types().iter().any(|kind| {
            !matches!(kind, CrateType::Executable | CrateType::Dylib | CrateType::Cdylib)
        }) {
            return Err(
                "entry input capture requires final executable or dynamic module outputs".into()
            );
        }
        let requires_primitive_admission = !module && self.policy["owner_input"].is_string();
        let require_rust_entry_wrapper = self.policy["require_rust_entry_wrapper"].as_bool()
            .ok_or("entry link policy omits Rust wrapper requirement")?;
        let rust_entry_wrapper = if require_rust_entry_wrapper && !module {
            rust_entry_wrapper(tcx, facts)?
        } else {
            Value::Null
        };
        let allocator_kind = match tcx.allocator_kind(()) {
            Some(AllocatorKind::Global) => "global",
            Some(AllocatorKind::Default) => "default",
            None => "none",
        };
        let mut allocator_owners = Vec::new();
        for krate in std::iter::once(LOCAL_CRATE).chain(tcx.crates(()).iter().copied()) {
            let global = tcx.has_global_allocator(krate);
            let default = default_allocator(tcx, krate)?;
            if !global && !default {
                continue;
            }
            let local = krate == LOCAL_CRATE;
            if local && requires_primitive_admission {
                return Err(
                    "participating final entry cannot execute an application allocator before GO"
                        .into(),
                );
            }
            let mut artifacts = BTreeMap::new();
            if !local {
                let source = tcx.used_crate_source(krate);
                for path in source.paths().chain(source.sdylib_interface.iter()) {
                    let path = fs::canonicalize(path).map_err(|error| error.to_string())?;
                    artifacts.insert(
                        path.clone(),
                        crate::digest(&fs::read(path).map_err(|error| error.to_string())?),
                    );
                }
                if artifacts.is_empty() {
                    return Err("allocator owner has no captured native archive".into());
                }
            }
            if global {
                allocator_owners
                    .push(json!({"kind": if local { "local_global" } else { "dependency_global" },
                    "crate_name": tcx.crate_name(krate).to_string(), "artifacts": artifacts}));
            }
            if default {
                allocator_owners.push(json!({"kind": "default", "local": local,
                    "crate_name": tcx.crate_name(krate).to_string(), "sysroot": facts["sysroot"], "artifacts": artifacts}));
            }
        }
        let outputs = tcx.output_filenames(());
        let mut modules = Vec::new();
        for module in codegen_modules(tcx, facts)? {
            modules.push((module["name"].as_str().ok_or("missing codegen module name")?.to_owned(),
                "regular", module["codegen_symbols"].as_array().ok_or("missing codegen symbols")?.clone()));
        }
        if rustc_codegen_ssa::base::allocator_kind_for_codegen(tcx).is_some() {
            let name = CodegenUnitNameBuilder::new(tcx).build_cgu_name(
                LOCAL_CRATE,
                &["crate"],
                Some("allocator"),
            );
            modules.push((name.to_string(), "allocator", Vec::new()));
        }
        let mut expected = Vec::new();
        let mut candidates = BTreeMap::new();
        for (name, kind, symbols) in modules {
            let path = std::path::absolute(outputs.temp_path_ext_for_cgu("no-opt.bc", &name))
                .map_err(|error| error.to_string())?;
            remove(&path)?;
            expected.push(
                json!({"name": name, "kind": kind, "path": path, "codegen_symbols": symbols}),
            );
            if kind != "allocator" || tcx.sess.lto() != Lto::Fat {
                let output =
                    std::path::absolute(outputs.temp_path_for_cgu(OutputType::Object, &name))
                        .map_err(|error| error.to_string())?;
                candidates.insert(output, json!({"name": name, "kind": kind}));
            }
        }
        if tcx.sess.lto() == Lto::Thin {
            for (archive, _) in
                facts["dependency_artifacts"].as_object().ok_or("missing dependency artifacts")?
            {
                let archive = Path::new(archive);
                if archive.extension().is_none_or(|extension| extension != "rlib") {
                    continue;
                }
                let bytes = fs::read(archive).map_err(|error| error.to_string())?;
                let metadata = rustc_codegen_ssa::back::rmeta_link::read_from_data(&bytes, archive)
                    .ok_or_else(|| {
                        format!("cannot read exact Rust module list: {}", archive.display())
                    })?;
                for name in metadata.rust_object_files {
                    let output =
                        std::path::absolute(outputs.temp_path_for_cgu(OutputType::Object, &name))
                            .map_err(|error| error.to_string())?;
                    if candidates
                        .insert(
                            output,
                            json!({"name": name, "kind": "upstream", "archive": archive}),
                        )
                        .is_some()
                    {
                        return Err("ambiguous compiler output module identity".into());
                    }
                }
            }
        }
        let mut native_candidates = Vec::new();
        for (path, mut identity) in candidates {
            remove(&path)?;
            identity["path"] = json!(path);
            native_candidates.push(identity);
        }
        if expected.is_empty() {
            return Err("entry input capture has no codegen modules".into());
        }
        let mut final_outputs = Vec::new();
        if !outputs.outputs.contains_key(&OutputType::Exe) {
            return Err("entry capture requires linked output".into());
        }
        for &kind in tcx.crate_types() {
            let output = rustc_session::output::out_filename(
                tcx.sess,
                kind,
                outputs,
                tcx.crate_name(LOCAL_CRATE),
            );
            if output.is_stdout() {
                return Err("entry capture output must be a file".into());
            }
            final_outputs
                .push(std::path::absolute(output.as_path()).map_err(|error| error.to_string())?);
        }
        let plan = json!({"state": "pending", "request": self.request,
            "requires_primitive_admission": requires_primitive_admission,
            "rust_entry_wrapper": rust_entry_wrapper,
            "allocator_kind": allocator_kind, "allocator_owners": allocator_owners,
            "target": tcx.sess.opts.target_triple.tuple(),
            "request_sha256": self.request_sha256, "arguments": facts["arguments"],
            "effective_realization": self.realization, "cwd": facts["cwd"], "sysroot": facts["sysroot"],
            "compiler_sha256": facts["compiler_sha256"], "helper_sha256": facts["helper_sha256"],
            "dependency_artifacts": facts["dependency_artifacts"], "premerge_modules": expected,
            "compiler_output_candidates": native_candidates,
            "output_paths": final_outputs});
        let hash = publish(&absolute(&self.request, "plan_path")?, &plan)?;
        facts["effective_realization"] = self.realization.clone();
        facts["entry_input_plan_sha256"] = json!(hash);
        facts["entry_input_plan_path"] = self.request["plan_path"].clone();
        facts["entry_input_receipt_path"] = self.request["receipt_path"].clone();
        self.plan_sha256 = Some(hash);
        self.plan = Some(plan);
        Ok(())
    }

    pub fn complete(&self, facts_path: &Path) -> Result<(), String> {
        if crate::digest(&fs::read(&self.request_path).map_err(|error| error.to_string())?)
            != self.request_sha256
        {
            return Err("entry capture request changed during compilation".into());
        }
        let policy = absolute(&self.request, "policy_path")?;
        if crate::digest(&fs::read(policy).map_err(|error| error.to_string())?)
            != self.request["policy_sha256"]
        {
            return Err("entry capture policy changed during compilation".into());
        }
        let plan = self.plan.as_ref().ok_or("missing pending entry plan")?;
        let hash = self.plan_sha256.as_ref().ok_or("missing entry plan hash")?;
        if crate::digest(
            &fs::read(absolute(&self.request, "plan_path")?).map_err(|error| error.to_string())?,
        ) != *hash
        {
            return Err("pending entry plan changed during compilation".into());
        }
        let mut premerge = BTreeMap::new();
        for module in plan["premerge_modules"].as_array().ok_or("missing premerge module list")? {
            let path = absolute(module, "path")?;
            premerge.insert(
                path.clone(),
                crate::digest(
                    &fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?,
                ),
            );
        }
        let mut outputs = BTreeMap::new();
        for output in plan["output_paths"].as_array().ok_or("missing final entry outputs")? {
            let path = PathBuf::from(output.as_str().ok_or("invalid final entry output")?);
            outputs.insert(
                path.clone(),
                crate::digest(
                    &fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?,
                ),
            );
        }
        let mut compiler_outputs = BTreeMap::new();
        for candidate in plan["compiler_output_candidates"]
            .as_array()
            .ok_or("missing compiler output candidates")?
        {
            let path = absolute(candidate, "path")?;
            match fs::read(&path) {
                Ok(bytes) => {
                    compiler_outputs.insert(path, crate::digest(&bytes));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
        publish(
            &absolute(&self.request, "receipt_path")?,
            &json!({"state": "complete",
            "request_id": self.request["request_id"], "request_sha256": self.request_sha256,
            "plan_sha256": hash, "facts_path": facts_path,
            "facts_sha256": crate::digest(&fs::read(facts_path).map_err(|error| error.to_string())?),
            "premerge_artifacts": premerge, "output_artifacts": outputs,
            "compiler_output_artifacts": compiler_outputs}),
        )?;
        Ok(())
    }
}
