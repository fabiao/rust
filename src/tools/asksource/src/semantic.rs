use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustc_span::source_map::{FileLoader, RealFileLoader};

use rustc_driver::Compilation;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::{self, Visitor};
use rustc_hir::pat_util::EnumerateAndAdjustIterator;
use rustc_interface::interface;
use rustc_middle::ty::{self, Ty, TyCtxt, TypeVisitableExt};
use rustc_middle::ty::consts::ConstExt;
use rustc_middle::mono::MonoItem;
use rustc_span::Span;
use serde_json::{Value, json};
use crate::digest;
use crate::records::{self, Writer as RecordWriter};

#[cfg(feature = "snapshot-driver")]
use rustc_driver::run_compiler as compiler_entrypoint;
#[cfg(not(feature = "snapshot-driver"))]
use rustc_driver::compiler_entrypoint;

#[derive(Default)]
struct LoadedInputs {
    files: BTreeMap<PathBuf, String>,
    binary: BTreeSet<PathBuf>,
    exists: BTreeMap<PathBuf, bool>,
    error: Option<String>,
}

struct RecordingLoader {
    inner: Box<dyn FileLoader + Send + Sync>,
    inputs: Arc<Mutex<LoadedInputs>>,
}

impl RecordingLoader {
    fn record(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let path = std::path::absolute(path)?;
        let hash = digest(bytes);
        let mut inputs = self.inputs.lock().map_err(|_| std::io::Error::other("input recorder poisoned"))?;
        if inputs.files.get(&path).is_some_and(|previous| previous != &hash) {
            return Err(std::io::Error::other(format!("compiler input changed between reads: {}", path.display())));
        }
        inputs.files.insert(path, hash);
        Ok(())
    }
}

impl FileLoader for RecordingLoader {
    fn file_exists(&self, path: &Path) -> bool {
        let exists = self.inner.file_exists(path);
        if let (Ok(path), Ok(mut inputs)) = (std::path::absolute(path), self.inputs.lock()) {
            if inputs.exists.insert(path.clone(), exists).is_some_and(|previous| previous != exists) {
                inputs.error = Some(format!("compiler input existence changed between queries: {}", path.display()));
            }
        }
        exists
    }

    fn read_file(&self, path: &Path) -> std::io::Result<String> {
        let text = self.inner.read_file(path)?;
        self.record(path, text.as_bytes())?;
        Ok(text)
    }

    fn read_binary_file(&self, path: &Path) -> std::io::Result<Arc<[u8]>> {
        let bytes = self.inner.read_binary_file(path)?;
        self.record(path, &bytes)?;
        self.inputs.lock().map_err(|_| std::io::Error::other("input recorder poisoned"))?
            .binary.insert(std::path::absolute(path)?);
        Ok(bytes)
    }

    fn current_directory(&self) -> std::io::Result<PathBuf> {
        self.inner.current_directory()
    }
}

fn location(tcx: TyCtxt<'_>, span: Span) -> Value {
    if span.is_dummy() {
        return Value::Null;
    }
    let map = tcx.sess.source_map();
    let start = map.lookup_byte_offset(span.lo());
    let end = map.lookup_byte_offset(span.hi());
    json!({"path": start.sf.name.prefer_local_unconditionally().to_string(),
        "source_file_id": format!("{:?}", start.sf.stable_id),
        "span": [start.sf.original_relative_byte_pos(span.lo()).0,
                 end.sf.original_relative_byte_pos(span.hi()).0],
        "same_file": start.sf.start_pos == end.sf.start_pos})
}

fn origin(tcx: TyCtxt<'_>, span: Span) -> Value {
    let mut expansion = Vec::new();
    let mut context = span.ctxt();
    while !context.is_root() {
        let data = context.outer_expn_data();
        expansion.push(json!({"kind": format!("{:?}", data.kind),
            "call_site": location(tcx, data.call_site), "def_site": location(tcx, data.def_site),
            "definition": data.macro_def_id.map(|id| definition(tcx, id))}));
        context.remove_mark();
    }
    json!({"source": location(tcx, span), "call_site": location(tcx, span.source_callsite()),
           "expansion": expansion})
}

fn definition(tcx: TyCtxt<'_>, id: rustc_hir::def_id::DefId) -> Value {
    json!({"crate": tcx.crate_name(id.krate).to_string(),
        "path": tcx.def_path_str(id),
        "name": tcx.opt_item_name(id).map(|name| name.to_string()),
        "kind": format!("{:?}", tcx.def_kind(id)),
        "hash": format!("{:?}", tcx.def_path_hash(id)),
        "source": location(tcx, tcx.def_span(id))})
}

fn constant<'tcx>(tcx: TyCtxt<'tcx>, value: ty::Const<'tcx>) -> Value {
    let kind = match value.kind() {
        ty::ConstKind::Param(_) => "parameter",
        ty::ConstKind::Infer(_) => "inference",
        ty::ConstKind::Bound(..) => "bound",
        ty::ConstKind::Placeholder(_) => "placeholder",
        ty::ConstKind::Alias(..) => "alias",
        ty::ConstKind::Value(_) => "value",
        ty::ConstKind::Error(_) => "error",
        ty::ConstKind::Expr(_) => "expression",
    };
    json!({"value": value.try_to_target_usize(tcx), "display": value.to_string(), "kind": kind})
}

fn argument<'tcx>(tcx: TyCtxt<'tcx>, arg: ty::GenericArg<'tcx>) -> Value {
    match arg.kind() {
        ty::GenericArgKind::Type(ty) => type_identity(tcx, ty),
        ty::GenericArgKind::Const(value) => json!({"kind": "const", "constant": constant(tcx, value)}),
        ty::GenericArgKind::Lifetime(_) => json!({"kind": "lifetime"}),
    }
}

fn slice_length<'tcx>(tcx: TyCtxt<'tcx>, value: rustc_middle::mir::ConstValue,
    input: Ty<'tcx>, environment: ty::TypingEnv<'tcx>) -> Option<u64> {
    use rustc_middle::mir::{ConstValue, interpret::{GlobalAlloc, alloc_range}};
    match value {
        ConstValue::Slice { meta, .. } => Some(meta),
        ConstValue::Indirect { alloc_id, offset }
            if matches!(input.kind(), ty::Ref(_, element, _) if matches!(element.kind(), ty::Str | ty::Slice(_))) =>
        {
            let layout = tcx.layout_of(environment.as_query_input(input)).ok()?;
            if layout.fields.count() != 2 {
                return None;
            }
            let GlobalAlloc::Memory(allocation) = tcx.global_alloc(alloc_id) else { return None; };
            allocation.inner().read_scalar(&tcx,
                alloc_range(offset + layout.fields.offset(1), tcx.data_layout.pointer_size()), false)
                .ok()?.to_target_usize(&tcx).discard_err()
        }
        _ => None,
    }
}

pub mod semantic_type {
    pub const HEX_DIGITS: usize = (u128::BITS / 4) as usize;
}

pub mod compiler_items {
    pub const TYPE_SIZE: &str = "mem_size_of";
    pub const STRING_LENGTH: &str = "str_len";
}

pub mod generic_roles {
    pub const DECLARED: &str = "declared";
    pub const CLOSURE: &[&str] = &["closure_kind_ty", "closure_sig_as_fn_ptr_ty", "tupled_upvars_ty"];
    pub const COROUTINE: &[&str] = &["kind_ty", "resume_ty", "yield_ty", "return_ty", "tupled_upvars_ty"];
    pub const COROUTINE_CLOSURE: &[&str] = &[
        "closure_kind_ty", "signature_parts_ty", "tupled_upvars_ty", "coroutine_captures_by_ref_ty",
    ];
    pub const CONST_BLOCK: &[&str] = &["const_ty_placeholder"];
}

fn parameter_role(tcx: TyCtxt<'_>, parameter: &ty::GenericParamDef) -> Option<&'static str> {
    let kind = tcx.def_kind(parameter.def_id);
    if matches!(kind, DefKind::TyParam | DefKind::ConstParam | DefKind::LifetimeParam |
        DefKind::Trait | DefKind::TraitAlias) {
        return Some(generic_roles::DECLARED);
    }
    let roles = if kind == DefKind::AnonConst && parameter.name == rustc_span::sym::const_ty_placeholder {
        generic_roles::CONST_BLOCK
    } else if kind == DefKind::Closure {
        let input = tcx.type_of(parameter.def_id).instantiate_identity();
        let closure = tcx.try_normalize_erasing_regions(
            ty::TypingEnv::non_body_analysis(tcx, parameter.def_id), input,
        ).ok()?;
        match closure.kind() {
            ty::Closure(..) => generic_roles::CLOSURE,
            ty::Coroutine(..) => generic_roles::COROUTINE,
            ty::CoroutineClosure(..) => generic_roles::COROUTINE_CLOSURE,
            _ => return None,
        }
    } else {
        return None;
    };
    let generics = tcx.generics_of(parameter.def_id);
    if generics.own_params.len() != roles.len() { return None; }
    let index = (parameter.index as usize).checked_sub(generics.parent_count)?;
    roles.get(index).copied()
}

fn semantic_type_id<'tcx>(tcx: TyCtxt<'tcx>, value: Ty<'tcx>) -> Option<String> {
    if value.has_infer() || value.has_placeholders() || value.references_error() {
        return None;
    }
    Some(format!("{:0width$x}", tcx.type_id_hash(value).as_u128(), width = semantic_type::HEX_DIGITS))
}

fn type_identity<'tcx>(tcx: TyCtxt<'tcx>, value: Ty<'tcx>) -> Value {
    let mut fact = match value.kind() {
        ty::Array(element, extent) => json!({"kind": "array", "element": type_identity(tcx, *element),
                                           "extent": constant(tcx, *extent)}),
        ty::Slice(element) => json!({"kind": "slice", "element": type_identity(tcx, *element)}),
        ty::Ref(_, element, mutability) => json!({"kind": "reference", "element": type_identity(tcx, *element),
            "mutability": if mutability.is_mut() { "mutable" } else { "immutable" }}),
        ty::RawPtr(element, mutability) => json!({"kind": "pointer", "element": type_identity(tcx, *element),
            "mutability": if mutability.is_mut() { "mutable" } else { "immutable" }}),
        ty::Tuple(elements) => json!({"kind": "tuple", "elements": elements.as_slice().iter().map(|ty| type_identity(tcx, *ty)).collect::<Vec<_>>()}),
        ty::Adt(def, args) => json!({"kind": "adt", "definition": definition(tcx, def.did()),
            "arguments": args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()}),
        ty::FnDef(def, args) => json!({"kind": "function", "definition": definition(tcx, *def),
            "bound_variables": format!("{:?}", args.bound_vars()),
            "arguments": args.skip_binder().as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()}),
        ty::Param(param) => json!({"kind": "parameter", "index": param.index, "name": param.name.to_string()}),
        ty::Closure(def, args) | ty::CoroutineClosure(def, args) | ty::Coroutine(def, args) =>
            json!({"kind": "closure_or_coroutine", "definition": definition(tcx, *def),
                "arguments": args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()}),
        ty::Alias(rigid, alias) => {
            let (kind, def) = match alias.kind {
                ty::AliasTyKind::Projection { def_id } => ("Projection", def_id),
                ty::AliasTyKind::Inherent { def_id } => ("Inherent", def_id),
                ty::AliasTyKind::Opaque { def_id } => ("Opaque", def_id),
                ty::AliasTyKind::Free { def_id } => ("Free", def_id),
            };
            json!({"kind": "alias", "alias_kind": kind, "rigidity": format!("{rigid:?}"), "definition": definition(tcx, def),
                "arguments": alias.args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()})
        }
        _ => json!({"kind": "scalar_or_other", "display": value.to_string()}),
    };
    fact["compiler_type_id"] = json!(semantic_type_id(tcx, value));
    if fact["compiler_type_id"].is_null() {
        fact["compiler_type_id_error"] = json!("type contains inference, placeholders or errors");
    }
    fact
}

fn inline_storage<'tcx>(tcx: TyCtxt<'tcx>, value: Ty<'tcx>, typing_env: ty::TypingEnv<'tcx>,
    ancestors: &mut Vec<Ty<'tcx>>) -> Value {
    if ancestors.contains(&value) {
        return json!({"kind": "unresolved_cycle", "type": type_identity(tcx, value)});
    }
    ancestors.push(value);
    let storage = match value.kind() {
        ty::Array(element, extent) => json!({"kind": "array", "extent": constant(tcx, *extent),
            "element": inline_storage(tcx, *element, typing_env, ancestors)}),
        ty::Tuple(elements) => {
            let members: Vec<_> = elements.as_slice().iter().enumerate().filter_map(|(index, element)| {
                let storage = inline_storage(tcx, *element, typing_env, ancestors);
                (!storage.is_null()).then(|| json!({"index": index, "storage": storage}))
            }).collect();
            if members.is_empty() { Value::Null } else { json!({"kind": "tuple", "members": members}) }
        }
        ty::Adt(adt, args) => {
            let mut fields = Vec::new();
            for variant in adt.variants().iter() {
                for field in &variant.fields {
                    let input = field.ty(tcx, args);
                    let field_type = match tcx.try_normalize_erasing_regions(typing_env, input) {
                        Ok(value) => value,
                        Err(error) => {
                            fields.push(json!({"definition": definition(tcx, field.did),
                                "storage": {"kind": "unresolved", "error": format!("{error:?}")}}));
                            continue;
                        }
                    };
                    let storage = inline_storage(tcx, field_type, typing_env, ancestors);
                    if !storage.is_null() {
                        fields.push(json!({"variant": definition(tcx, variant.def_id),
                            "definition": definition(tcx, field.did), "type": type_identity(tcx, field_type),
                            "storage": storage}));
                    }
                }
            }
            if fields.is_empty() { Value::Null } else {
                json!({"kind": "adt", "definition": definition(tcx, adt.did()), "fields": fields})
            }
        }
        ty::Closure(def, args) | ty::CoroutineClosure(def, args) => {
            let upvars = match value.kind() {
                ty::Closure(..) => args.as_closure().tupled_upvars_ty(),
                _ => args.as_coroutine_closure().tupled_upvars_ty(),
            };
            let storage = inline_storage(tcx, upvars, typing_env, ancestors);
            if storage.is_null() { Value::Null } else {
                let captures: Vec<_> = def.as_local().into_iter().flat_map(|id| tcx.closure_captures(id))
                    .map(|capture| json!({"place": capture.to_string(tcx),
                        "binding": location(tcx, capture.var_ident.span),
                        "mode": format!("{:?}", capture.info.capture_kind)})).collect();
                json!({"kind": "closure", "definition": definition(tcx, *def),
                    "captures": captures, "storage": storage})
            }
        }
        ty::Coroutine(def, args) => {
            let captures = inline_storage(tcx, args.as_coroutine().tupled_upvars_ty(), typing_env, ancestors);
            match tcx.coroutine_layout(*def, args) {
                Ok(layout) => {
                    let fields: Vec<_> = layout.field_tys.iter_enumerated().filter_map(|(index, field)| {
                        let input = ty::EarlyBinder::bind(tcx, field.ty).instantiate(tcx, args);
                        let storage = match tcx.try_normalize_erasing_regions(typing_env, input) {
                            Ok(value) => inline_storage(tcx, value, typing_env, ancestors),
                            Err(error) => json!({"kind": "unresolved", "error": format!("{error:?}")}),
                        };
                        (!storage.is_null()).then(|| json!({"index": index.as_u32(), "origin": origin(tcx, field.source_info.span),
                            "name": field.debuginfo_name.map(|name| name.to_string()), "storage": storage}))
                    }).collect();
                    if fields.is_empty() && captures.is_null() { Value::Null } else {
                        json!({"kind": "coroutine", "definition": definition(tcx, *def),
                            "captures": captures, "saved_fields": fields})
                    }
                }
                Err(error) => json!({"kind": "unresolved", "error": format!("{error:?}")}),
            }
        }
        ty::Pat(inner, _) => inline_storage(tcx, *inner, typing_env, ancestors),
        ty::UnsafeBinder(bound) => inline_storage(tcx,
            tcx.instantiate_bound_regions_with_erased((*bound).into()), typing_env, ancestors),
        ty::Bool | ty::Char | ty::Int(_) | ty::Uint(_) | ty::Float(_) | ty::Str |
        ty::Slice(_) | ty::RawPtr(..) | ty::Ref(..) | ty::FnDef(..) | ty::FnPtr(..) |
        ty::Never => Value::Null,
        _ => json!({"kind": "unresolved", "type": type_identity(tcx, value)}),
    };
    ancestors.pop();
    storage
}

fn type_fact<'tcx>(tcx: TyCtxt<'tcx>, value: Ty<'tcx>, typing_env: ty::TypingEnv<'tcx>) -> Value {
    let mut fact = type_identity(tcx, value);
    let storage_env = typing_env.with_post_analysis_normalized(tcx);
    let input = if storage_env != typing_env {
        ty::set_aliases_to_non_rigid(tcx, value)
    } else {
        ty::Unnormalized::new_wip(value)
    };
    fact["inline_storage"] = match tcx.try_normalize_erasing_regions(storage_env, input) {
        Ok(value) => inline_storage(tcx, value, storage_env, &mut Vec::new()),
        Err(error) => json!({"kind": "unresolved", "error": format!("{error:?}")}),
    };
    fact
}

fn parameter_bindings<'tcx>(tcx: TyCtxt<'tcx>, selected: rustc_hir::def_id::DefId,
    args: ty::GenericArgsRef<'tcx>, caller: Option<LocalDefId>) -> Result<Vec<Value>, String> {
    let selected = tcx.generics_of(selected);
    if args.len() != selected.count() {
        return Err("selected compiler instance has incomplete arguments".into());
    }
    let caller = caller.map(|owner| tcx.generics_of(owner));
    let mut bindings = Vec::new();
    for (index, arg) in args.as_slice().iter().enumerate() {
        let parameter = selected.param_at(index, tcx);
        let role = parameter_role(tcx, parameter)
            .ok_or("selected compiler parameter has an unsupported semantic role")?;
        let argument_index = match arg.kind() {
            ty::GenericArgKind::Const(value) => match value.kind() {
                ty::ConstKind::Param(param) => Some(param.index),
                _ => None,
            },
            ty::GenericArgKind::Type(value) => match value.kind() {
                ty::Param(param) => Some(param.index),
                _ => None,
            },
            ty::GenericArgKind::Lifetime(_) => None,
        };
        let argument_parameter = if let Some(index) = argument_index {
            let caller = caller.ok_or("native compiler argument remains an unbound parameter")?;
            if index as usize >= caller.count() {
                return Err("selected argument has no declaring caller parameter".into());
            }
            Some(caller.param_at(index as usize, tcx))
        } else {
            None
        };
        let argument_role = argument_parameter.and_then(|parameter| parameter_role(tcx, parameter));
        if argument_parameter.is_some() && argument_role.is_none() {
            return Err("caller compiler parameter has an unsupported semantic role".into());
        }
        bindings.push(json!({"parameter": definition(tcx, parameter.def_id), "role": role,
            "argument": argument(tcx, *arg),
            "argument_parameter": argument_parameter.map(|parameter| definition(tcx, parameter.def_id)),
            "argument_parameter_role": argument_role}));
    }
    Ok(bindings)
}

fn instance_fact<'tcx>(tcx: TyCtxt<'tcx>, instance: ty::Instance<'tcx>) -> Value {
    let (bindings, error) = match parameter_bindings(tcx, instance.def_id(), instance.args, None) {
        Ok(bindings) => (json!(bindings), Value::Null),
        Err(error) => (Value::Null, json!(error)),
    };
    json!({"definition": definition(tcx, instance.def_id()),
        "symbol": tcx.symbol_name(instance).name,
        "compiler_type_id": semantic_type_id(tcx, instance.ty(tcx, ty::TypingEnv::fully_monomorphized())),
        "parameter_bindings": bindings, "parameter_binding_error": error,
        "arguments": instance.args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()})
}

fn source_instance<'tcx>(tcx: TyCtxt<'tcx>, instance: ty::Instance<'tcx>,
    instances: &mut HashMap<LocalDefId, Vec<ty::Instance<'tcx>>>)
{
    let ty::InstanceKind::Item(def) = instance.def else { return };
    let Some(owner) = def.as_local() else { return };
    if tcx.hir_maybe_body_owned_by(owner).is_none() { return; }
    let selected = instances.entry(owner).or_default();
    if !selected.contains(&instance) { selected.push(instance); }
}

fn inline_scope<'tcx>(tcx: TyCtxt<'tcx>, instance: ty::Instance<'tcx>, body: &rustc_middle::mir::Body<'tcx>,
    scope: rustc_middle::mir::SourceScope) -> (ty::Instance<'tcx>, Vec<Value>) {
    let mut selected = instance;
    let mut frames = Vec::new();
    let mut current = Some(scope);
    while let Some(index) = current {
        let Some(scope) = body.source_scopes.get(index) else { break };
        if let Some((callee, call_site)) = scope.inlined {
            match instance.try_instantiate_mir_and_normalize_erasing_regions(
                tcx, ty::TypingEnv::fully_monomorphized(), ty::EarlyBinder::bind(tcx, callee)) {
                Ok(callee) => {
                    if frames.is_empty() { selected = callee; }
                    frames.push(json!({"instance": instance_fact(tcx, callee), "origin": origin(tcx, call_site)}));
                }
                Err(error) => frames.push(json!({"unresolved": format!("{error:?}")})),
            }
        }
        current = scope.parent_scope;
    }
    (selected, frames)
}

fn instantiated_storage<'tcx>(tcx: TyCtxt<'tcx>, nodes: &mut RecordWriter)
    -> HashMap<LocalDefId, Vec<ty::Instance<'tcx>>>
{
    let mut visited = HashSet::new();
    let mut instances = HashMap::new();
    for unit in tcx.collect_and_partition_mono_items(()).codegen_units {
        for (item, _) in unit.items_in_deterministic_order(tcx) {
            let MonoItem::Fn(instance) = item else { continue };
            if !visited.insert(instance) { continue; }
            source_instance(tcx, instance, &mut instances);
            let body = tcx.instance_mir(instance.def);
            for scope in &body.source_scopes {
                if let Some((callee, _)) = scope.inlined {
                    if let Ok(callee) = instance.try_instantiate_mir_and_normalize_erasing_regions(
                        tcx, ty::TypingEnv::fully_monomorphized(), ty::EarlyBinder::bind(tcx, callee)) {
                        source_instance(tcx, callee, &mut instances);
                    }
                }
            }
            let descriptor = instance_fact(tcx, instance);
            for (scope_id, scope) in body.source_scopes.iter_enumerated() {
                if scope.parent_scope.is_some() && scope.inlined.is_none() { continue; }
                let (selected, inlined) = inline_scope(tcx, instance, body, scope_id);
                if !matches!(selected.def, ty::InstanceKind::Item(_)) { continue; }
                nodes.push(json!({"kind": "instantiated_body", "owner": definition(tcx, selected.def_id()),
                    "origin": origin(tcx, tcx.def_span(selected.def_id())), "instance": descriptor,
                    "mir_scope": scope_id.as_u32(), "inlined": inlined,
                    "type": type_fact(tcx, selected.ty(tcx, ty::TypingEnv::fully_monomorphized()),
                        ty::TypingEnv::fully_monomorphized())}));
            }
            for (local, declaration) in body.local_decls.iter_enumerated() {
                let ty = match instance.try_instantiate_mir_and_normalize_erasing_regions(
                    tcx, ty::TypingEnv::fully_monomorphized(), ty::EarlyBinder::bind(tcx, declaration.ty)) {
                    Ok(value) => type_fact(tcx, value, ty::TypingEnv::fully_monomorphized()),
                    Err(error) => json!({"inline_storage": {"kind": "unresolved", "error": format!("{error:?}")}}),
                };
                if ty["inline_storage"].is_null() { continue; }
                let (selected, inlined) = inline_scope(tcx, instance, body, declaration.source_info.scope);
                let names: Vec<_> = body.var_debug_info.iter().filter_map(|variable| {
                    if let rustc_middle::mir::VarDebugInfoContents::Place(place) = &variable.value {
                        if place.local == local && place.projection.is_empty() {
                            return Some(json!({"name": variable.name.to_string(),
                                "origin": origin(tcx, variable.source_info.span)}));
                        }
                    }
                    None
                }).collect();
                nodes.push(json!({"kind": "instantiated_storage", "owner": definition(tcx, selected.def_id()),
                    "origin": origin(tcx, declaration.source_info.span), "instance": descriptor,
                    "mir_local": local.as_u32(), "inlined": inlined, "names": names, "type": ty}));
            }
        }
    }
    instances
}

fn resolution(tcx: TyCtxt<'_>, res: Res) -> Value {
    match res {
        Res::Def(kind, def) => json!({"kind": format!("{kind:?}"), "definition": definition(tcx, def),
                                    "origin": origin(tcx, tcx.def_span(def))}),
        Res::Local(id) => json!({"kind": "local", "owner": definition(tcx, id.owner.def_id.to_def_id()),
            "reference": node_reference(tcx, id, tcx.hir_span(id))}),
        _ => json!({"kind": format!("{res:?}")}),
    }
}

fn const_argument_literal(literal: rustc_ast::LitKind, negated: bool) -> Value {
    use rustc_ast::LitKind;
    let (kind, payload, suffix, style) = match literal {
        LitKind::Int(value, suffix) => ("integer", json!(value.get().to_string()), json!(format!("{suffix:?}")), Value::Null),
        LitKind::Float(value, suffix) => ("float", json!(value.as_str()), json!(format!("{suffix:?}")), Value::Null),
        LitKind::Bool(value) => ("boolean", json!(value), Value::Null, Value::Null),
        LitKind::Char(value) => ("character", json!(value.to_string()), Value::Null, Value::Null),
        LitKind::Byte(value) => ("byte", json!(value), Value::Null, Value::Null),
        LitKind::Str(value, style) => ("string", json!(value.as_str()), Value::Null, json!(format!("{style:?}"))),
        LitKind::ByteStr(value, style) => ("byte_string", json!(value.as_byte_str()), Value::Null, json!(format!("{style:?}"))),
        LitKind::CStr(value, style) => ("c_string", json!(value.as_byte_str()), Value::Null, json!(format!("{style:?}"))),
        LitKind::Err(_) => ("error", Value::Null, Value::Null, Value::Null),
    };
    json!({"kind": kind, "payload": payload, "suffix": suffix, "style": style, "negated": negated})
}

fn const_argument(tcx: TyCtxt<'_>, arg: &rustc_hir::ConstArg<'_, rustc_hir::AmbigArg>) -> Value {
    let (kind, body, owner) = match arg.kind {
        rustc_hir::ConstArgKind::Anon(value) => {
            let expression = tcx.hir_body(value.body).value;
            ("anonymous", node_reference(tcx, expression.hir_id, expression.span), definition(tcx, value.def_id.to_def_id()))
        }
        rustc_hir::ConstArgKind::Path(..) => ("path", Value::Null, Value::Null),
        rustc_hir::ConstArgKind::Tup(..) => ("tuple", Value::Null, Value::Null),
        rustc_hir::ConstArgKind::Struct(..) => ("struct", Value::Null, Value::Null),
        rustc_hir::ConstArgKind::TupleCall(..) => ("tuple_call", Value::Null, Value::Null),
        rustc_hir::ConstArgKind::Array(..) => ("array", Value::Null, Value::Null),
        rustc_hir::ConstArgKind::Error(..) => ("error", Value::Null, Value::Null),
        rustc_hir::ConstArgKind::Infer(never) => match never {},
        rustc_hir::ConstArgKind::Literal { .. } => ("literal", Value::Null, Value::Null),
    };
    let value_definition = match arg.kind {
        rustc_hir::ConstArgKind::Anon(value) => Some(value.def_id.to_def_id()),
        rustc_hir::ConstArgKind::Path(rustc_hir::QPath::Resolved(_, path)) => match path.res {
            Res::Def(DefKind::ConstParam | DefKind::Const | DefKind::AssocConst, definition) => Some(definition),
            _ => None,
        },
        _ => None,
    };
    let value_type = if let Some(definition) = value_definition {
        let input = tcx.type_of(definition).instantiate_identity();
        match tcx.try_normalize_erasing_regions(ty::TypingEnv::non_body_analysis(tcx, definition), input) {
            Ok(value) => type_identity(tcx, value),
            Err(error) => json!({"kind": "unresolved", "error": format!("{error:?}")}),
        }
    } else {
        match tcx.parent_hir_node(arg.hir_id) {
            rustc_hir::Node::Ty(rustc_hir::Ty { kind: rustc_hir::TyKind::Array(_, value), .. })
                if value.hir_id == arg.hir_id => type_identity(tcx, tcx.types.usize),
            rustc_hir::Node::Expr(rustc_hir::Expr { kind: rustc_hir::ExprKind::Repeat(_, value), .. })
                if value.hir_id == arg.hir_id => type_identity(tcx, tcx.types.usize),
            _ => Value::Null,
        }
    };
    let mut fact = json!({"kind": "const_argument", "origin": origin(tcx, arg.span),
        "owner": definition(tcx, arg.hir_id.owner.def_id.to_def_id()), "local_id": arg.hir_id.local_id.as_u32(),
        "selected_definition": null, "selected_arguments": null, "selection_error": null,
        "argument_kind": kind, "body": body, "argument_definition": owner,
        "literal": match arg.kind {
            rustc_hir::ConstArgKind::Literal { lit, negated } => const_argument_literal(lit, negated),
            _ => Value::Null,
        }});
    if let rustc_hir::ConstArgKind::Path(rustc_hir::QPath::Resolved(_, path)) = arg.kind {
        fact["resolution"] = resolution(tcx, path.res);
    }
    let body_origin = match arg.kind {
        rustc_hir::ConstArgKind::Anon(value) => origin(tcx, tcx.hir_body(value.body).value.span),
        _ => Value::Null,
    };
    let semantic_error = if !matches!(kind, "anonymous" | "path" | "literal") {
        Some("direct aggregate/error const argument needs complete selected compiler semantics")
    } else if kind == "path" && fact.get("resolution").and_then(|value| value.get("definition")).is_none() {
        Some("const argument path needs its exact compiler-resolved declaration")
    } else if matches!(arg.kind, rustc_hir::ConstArgKind::Literal { lit: rustc_ast::LitKind::Err(_), .. }) {
        Some("const argument literal contains a compiler error")
    } else {
        None
    };
    fact["type"] = json!({"kind": "const_argument", "argument_kind": kind,
        "value_type": value_type, "literal": fact["literal"],
        "resolution": fact.get("resolution").cloned().unwrap_or(Value::Null),
        "argument_definition": fact["argument_definition"], "body_origin": body_origin,
        "semantic_error": semantic_error});
    fact
}

fn node_reference(tcx: TyCtxt<'_>, id: rustc_hir::HirId, span: Span) -> Value {
    json!({"local_id": id.local_id.as_u32(),
        "owner": definition(tcx, id.owner.def_id.to_def_id()), "origin": origin(tcx, span)})
}

struct DirectChildren<'tcx> {
    tcx: TyCtxt<'tcx>,
    references: Vec<Value>,
}

impl<'tcx> Visitor<'tcx> for DirectChildren<'tcx> {
    fn visit_expr(&mut self, expr: &'tcx rustc_hir::Expr<'tcx>) {
        self.references.push(node_reference(self.tcx, expr.hir_id, expr.span));
    }

    fn visit_pat(&mut self, pat: &'tcx rustc_hir::Pat<'tcx>) {
        self.references.push(node_reference(self.tcx, pat.hir_id, pat.span));
    }

    fn visit_block(&mut self, block: &'tcx rustc_hir::Block<'tcx>) {
        self.references.push(node_reference(self.tcx, block.hir_id, block.span));
    }

    fn visit_nested_body(&mut self, id: rustc_hir::BodyId) {
        let value = self.tcx.hir_body(id).value;
        self.references.push(node_reference(self.tcx, value.hir_id, value.span));
    }
}

struct Facts<'tcx, 'records> {
    tcx: TyCtxt<'tcx>,
    typeck: &'tcx ty::TypeckResults<'tcx>,
    body_owner: LocalDefId,
    nodes: &'records mut RecordWriter,
    instances: &'records [ty::Instance<'tcx>],
}

impl<'tcx> Facts<'tcx, '_> {
    fn type_instantiations(&self, id: rustc_hir::HirId) -> Vec<Value> {
        let Some(input) = self.typeck.node_type_opt(id) else { return Vec::new() };
        if !input.has_non_region_param() { return Vec::new(); }
        self.instances.iter().map(|instance| {
            let (ty, error) = match self.tcx.try_instantiate_and_normalize_erasing_regions(
                instance.args, ty::TypingEnv::fully_monomorphized(), ty::EarlyBinder::bind(self.tcx, input)) {
                Ok(value) => (type_fact(self.tcx, value, ty::TypingEnv::fully_monomorphized()), Value::Null),
                Err(error) => (Value::Null, json!(format!("{error:?}"))),
            };
            json!({"instance": instance_fact(self.tcx, *instance), "type": ty, "error": error})
        }).collect()
    }

    fn initializer_projection(&self, pat: &'tcx rustc_hir::Pat<'tcx>, local: rustc_hir::HirId)
        -> Result<Vec<Value>, &'static str>
    {
        use rustc_hir::PatKind;
        let mut child = pat.hir_id;
        let mut projection = Vec::new();
        for (id, node) in self.tcx.hir_parent_iter(pat.hir_id) {
            if id == local {
                projection.reverse();
                return Ok(projection);
            }
            match node {
                rustc_hir::Node::Pat(parent) => {
                    if self.typeck.pat_adjustments().get(parent.hir_id).is_some_and(|steps| !steps.is_empty()) {
                        return Err("implicit pattern adjustment requires reference-flow evidence");
                    }
                    match parent.kind {
                        PatKind::Binding(_, _, _, Some(inner)) if inner.hir_id == child => {}
                        PatKind::Tuple(elements, rest) => {
                            let Some(ty) = self.typeck.node_type_opt(parent.hir_id) else {
                                return Err("tuple pattern has no checked type");
                            };
                            let ty::Tuple(types) = ty.kind() else {
                                return Err("tuple pattern has no checked tuple shape");
                            };
                            if elements.len() > types.len() {
                                return Err("tuple pattern exceeds its checked shape");
                            }
                            let Some((index, _)) = elements.iter().enumerate_and_adjust(types.len(), rest)
                                .find(|(_, element)| element.hir_id == child) else {
                                return Err("binding has no checked tuple element");
                            };
                            projection.push(json!({"kind": "tuple", "index": index}));
                        }
                        PatKind::Struct(ref path, fields, _) => {
                            let Some(field) = fields.iter().find(|field| field.hir_id == child) else {
                                return Err("binding has no checked pattern field");
                            };
                            let Some(ty) = self.typeck.node_type_opt(parent.hir_id) else {
                                return Err("struct pattern has no checked type");
                            };
                            let ty::Adt(adt, _) = ty.kind() else {
                                return Err("struct pattern has no checked ADT shape");
                            };
                            let resolved = self.typeck.qpath_res(path, parent.hir_id);
                            let Some(index) = self.typeck.opt_field_index(field.hir_id) else {
                                return Err("pattern field has no checked index");
                            };
                            let Some(field) = adt.variant_of_res(resolved).fields.get(index) else {
                                return Err("pattern field exceeds its checked ADT shape");
                            };
                            projection.push(json!({"kind": "adt", "definition": definition(self.tcx, field.did)}));
                        }
                        PatKind::TupleStruct(ref path, elements, rest) => {
                            let Some(ty) = self.typeck.node_type_opt(parent.hir_id) else {
                                return Err("tuple constructor pattern has no checked type");
                            };
                            let ty::Adt(adt, _) = ty.kind() else {
                                return Err("tuple constructor pattern has no checked ADT shape");
                            };
                            let fields = &adt.variant_of_res(self.typeck.qpath_res(path, parent.hir_id)).fields;
                            if elements.len() > fields.len() {
                                return Err("tuple constructor pattern exceeds its checked shape");
                            }
                            let Some((index, _)) = elements.iter().enumerate_and_adjust(fields.len(), rest)
                                .find(|(_, element)| element.hir_id == child) else {
                                return Err("binding has no checked constructor element");
                            };
                            let Some(field) = fields.iter().nth(index) else {
                                return Err("binding has no checked constructor field");
                            };
                            projection.push(json!({"kind": "adt", "definition": definition(self.tcx, field.did)}));
                        }
                        PatKind::Slice(before, _, after) => {
                            let Some(ty) = self.typeck.node_type_opt(parent.hir_id) else {
                                return Err("array pattern has no checked type");
                            };
                            let ty::Array(_, length) = ty.kind() else {
                                return Err("slice pattern requires a captured fixed array length");
                            };
                            let Some(length) = length.try_to_target_usize(self.tcx)
                                .and_then(|length| usize::try_from(length).ok()) else {
                                return Err("array pattern length remains unresolved");
                            };
                            if before.len() + after.len() > length {
                                return Err("array pattern exceeds its checked shape");
                            }
                            let index = before.iter().position(|element| element.hir_id == child)
                                .or_else(|| after.iter().position(|element| element.hir_id == child)
                                    .map(|index| length - after.len() + index));
                            let Some(index) = index else {
                                return Err("slice-rest binding requires array-view flow evidence");
                            };
                            projection.push(json!({"kind": "array", "index": index}));
                        }
                        PatKind::Or(_) => return Err("or-pattern requires selected-branch evidence"),
                        _ => return Err("pattern requires additional initializer-flow evidence"),
                    }
                }
                rustc_hir::Node::PatField(_) => {}
                _ => return Err("binding projection has no direct declaring local"),
            }
            child = id;
        }
        Err("binding projection has no captured declaration path")
    }

    fn node(&self, id: rustc_hir::HirId, span: Span, kind: &str) -> Value {
        let tcx = self.tcx;
        let typeck = self.typeck;
        json!({"kind": kind, "local_id": id.local_id.as_u32(),
            "owner": definition(tcx, id.owner.def_id.to_def_id()), "origin": origin(tcx, span),
            "body_owner": definition(tcx, self.body_owner.to_def_id()),
            "type": typeck.node_type_opt(id).map(|value|
                type_fact(tcx, value, ty::TypingEnv::post_typeck_until_borrowck(tcx, self.body_owner))),
            "arguments": typeck.node_args_opt(id).map(|args| args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()),
            "selected_definition": null, "selected_arguments": null, "selection_error": null})
    }

    fn select(&self, fact: &mut Value, id: rustc_hir::HirId, def: rustc_hir::def_id::DefId) {
        let mut args = self.typeck.node_args(id);
        if let Some(value) = self.typeck.node_type_opt(id) {
            if let ty::FnDef(owner, arguments) = value.kind() {
                if *owner != def {
                    fact["selection_error"] = json!("compiler callable type disagrees with its resolved definition");
                    return;
                }
                let Some(arguments) = arguments.no_bound_vars() else {
                    fact["selection_error"] = json!("compiler callable arguments remain bound");
                    return;
                };
                args = arguments;
            }
        }
        let expected = self.tcx.generics_of(def).count();
        if args.len() != expected {
            fact["selection_error"] = json!(format!("compiler instance arguments incomplete: expected {expected}, recorded {}", args.len()));
            return;
        }
        let environment = ty::TypingEnv::post_analysis(self.tcx, id.owner.def_id);
        match ty::Instance::try_resolve(self.tcx, environment, def, args) {
            Ok(Some(instance)) => {
                if !matches!(instance.def, ty::InstanceKind::Item(_)) {
                    fact["selection_error"] = json!("compiler instance is not a source item");
                    return;
                }
                let bindings = match parameter_bindings(self.tcx, instance.def_id(), instance.args, Some(self.body_owner)) {
                    Ok(bindings) => bindings,
                    Err(error) => { fact["selection_error"] = json!(error); return; }
                };
                fact["selected_definition"] = definition(self.tcx, instance.def_id());
                fact["selected_arguments"] = json!(bindings);
            }
            Ok(None) => fact["selection_error"] = json!("compiler instance remains unresolved"),
            Err(error) => fact["selection_error"] = json!(format!("compiler instance resolution failed: {error:?}")),
        }
    }

    fn shape_query(&self, def: rustc_hir::def_id::DefId, args: ty::GenericArgsRef<'tcx>,
        operands: &[&'tcx rustc_hir::Expr<'tcx>]) -> Value {
        let tcx = self.tcx;
        let environment = ty::TypingEnv::post_analysis(tcx, self.body_owner);
        let string_length = tcx.is_diagnostic_item(rustc_span::Symbol::intern(compiler_items::STRING_LENGTH), def);
        if (tcx.lang_items().slice_len_fn() == Some(def) || string_length) && let [receiver] = operands {
            let input = self.typeck.expr_ty(receiver);
            let normalized = tcx.try_normalize_erasing_regions(environment,
                ty::set_aliases_to_non_rigid(tcx, input)).ok();
            let mut value = normalized.and_then(|value| match value.peel_refs().kind() {
                ty::Array(_, extent) => extent.try_to_target_usize(tcx),
                _ => None,
            });
            if value.is_none() && let rustc_hir::ExprKind::Path(ref path) = receiver.kind
                && let Res::Def(DefKind::Const | DefKind::AssocConst, owner) =
                    self.typeck.qpath_res(path, receiver.hir_id)
            {
                let arguments = self.typeck.node_args(receiver.hir_id);
                if arguments.len() == tcx.generics_of(owner).count()
                    && let Ok(constant) = tcx.const_eval_resolve(
                        environment, rustc_middle::mir::UnevaluatedConst::new(owner, arguments), receiver.span)
                {
                    value = normalized.and_then(|input| slice_length(tcx, constant, input, environment));
                }
            }
            if value.is_none() && let rustc_hir::ExprKind::Lit(literal) = receiver.kind
                && let rustc_ast::LitKind::Str(text, _) = literal.node
            {
                value = Some(text.as_str().len() as u64);
            }
            return json!({"kind": if string_length { "string_length" } else { "slice_length" },
                "definition": definition(tcx, def),
                "input": type_identity(tcx, input), "value": value,
                "error": value.is_none().then_some("slice length needs a resolved array or constant slice value")});
        }
        if tcx.is_diagnostic_item(rustc_span::Symbol::intern(compiler_items::TYPE_SIZE), def) && operands.is_empty()
            && let Some(input) = args.as_slice().first().and_then(|arg| arg.as_type())
        {
            let value = tcx.try_normalize_erasing_regions(environment,
                ty::set_aliases_to_non_rigid(tcx, input)).ok()
                .and_then(|value| tcx.layout_of(environment.as_query_input(value)).ok())
                .map(|layout| layout.size.bytes());
            return json!({"kind": "type_size", "definition": definition(tcx, def),
                "input": type_identity(tcx, input), "value": value,
                "error": value.is_none().then_some("type size needs a resolved target layout")});
        }
        Value::Null
    }
}

impl<'tcx> Visitor<'tcx> for Facts<'tcx, '_> {
    fn visit_const_arg(&mut self, arg: &'tcx rustc_hir::ConstArg<'tcx, rustc_hir::AmbigArg>) {
        self.nodes.push(const_argument(self.tcx, arg));
        intravisit::walk_const_arg(self, arg);
    }

    fn visit_block(&mut self, block: &'tcx rustc_hir::Block<'tcx>) {
        let mut fact = self.node(block.hir_id, block.span, "block");
        fact["targeted_by_break"] = json!(block.targeted_by_break);
        let mut statements = Vec::new();
        let mut items = Vec::new();
        for statement in block.stmts {
            match statement.kind {
                rustc_hir::StmtKind::Let(local) =>
                    statements.push(node_reference(self.tcx, local.hir_id, local.span)),
                rustc_hir::StmtKind::Expr(expr) | rustc_hir::StmtKind::Semi(expr) =>
                    statements.push(node_reference(self.tcx, expr.hir_id, expr.span)),
                rustc_hir::StmtKind::Item(item) =>
                    items.push(definition(self.tcx, item.owner_id.def_id.to_def_id())),
            }
        }
        fact["statements"] = json!(statements);
        fact["item_definitions"] = json!(items);
        fact["tail"] = block.expr.map(|expr| node_reference(self.tcx, expr.hir_id, expr.span))
            .unwrap_or(Value::Null);
        self.nodes.push(fact);
        intravisit::walk_block(self, block);
    }

    fn visit_local(&mut self, local: &'tcx rustc_hir::LetStmt<'tcx>) {
        let mut fact = self.node(local.hir_id, local.span, "local");
        fact["pattern"] = node_reference(self.tcx, local.pat.hir_id, local.pat.span);
        let mut bindings = Vec::new();
        local.pat.each_binding(|_, id, span, _| {
            bindings.push(node_reference(self.tcx, id, span));
        });
        fact["bindings"] = json!(bindings);
        fact["initializer"] = local.init.map(|expr| node_reference(self.tcx, expr.hir_id, expr.span)).unwrap_or(Value::Null);
        fact["else"] = local.els.map(|block| node_reference(self.tcx, block.hir_id, block.span)).unwrap_or(Value::Null);
        self.nodes.push(fact);
        intravisit::walk_local(self, local);
    }

    fn visit_expr(&mut self, expr: &'tcx rustc_hir::Expr<'tcx>) {
        use rustc_hir::ExprKind;
        let kind = match expr.kind {
            ExprKind::Path(_) => "value_path",
            ExprKind::Call(..) => "call",
            ExprKind::MethodCall(..) => "method",
            ExprKind::Field(..) => "field_access",
            ExprKind::Struct(..) => "constructor",
            ExprKind::Repeat(..) => "repeat",
            ExprKind::Array(..) => "array_value",
            ExprKind::Tup(..) => "tuple_value",
            ExprKind::Binary(..) => "binary",
            ExprKind::Unary(..) => "unary",
            ExprKind::Lit(..) => "literal",
            ExprKind::Cast(..) => "cast",
            ExprKind::Type(..) => "type_ascription",
            ExprKind::DropTemps(..) => "drop_temps",
            _ => "expression",
        };
        let mut fact = self.node(expr.hir_id, expr.span, kind);
        let role = match expr.kind {
            ExprKind::ConstBlock(..) => "const_block",
            ExprKind::Array(..) => "array",
            ExprKind::Call(..) => "call",
            ExprKind::MethodCall(..) => "method",
            ExprKind::Use(..) => "use",
            ExprKind::Tup(..) => "tuple",
            ExprKind::Binary(..) => "binary",
            ExprKind::Unary(..) => "unary",
            ExprKind::Lit(..) => "literal",
            ExprKind::Cast(..) => "cast",
            ExprKind::Type(..) => "type_ascription",
            ExprKind::DropTemps(..) => "drop_temps",
            ExprKind::Let(..) => "let",
            ExprKind::If(..) => "if",
            ExprKind::Loop(..) => "loop",
            ExprKind::Match(..) => "match",
            ExprKind::Closure(..) => "closure",
            ExprKind::Block(..) => "block",
            ExprKind::Assign(..) => "assign",
            ExprKind::AssignOp(..) => "assign_op",
            ExprKind::Field(..) => "field",
            ExprKind::Index(..) => "index",
            ExprKind::Path(..) => "path",
            ExprKind::AddrOf(..) => "address",
            ExprKind::Break(..) => "break",
            ExprKind::Continue(..) => "continue",
            ExprKind::Ret(..) => "return",
            ExprKind::Become(..) => "become",
            ExprKind::InlineAsm(..) => "inline_asm",
            ExprKind::OffsetOf(..) => "offset_of",
            ExprKind::Struct(..) => "struct",
            ExprKind::Repeat(..) => "repeat",
            ExprKind::Yield(..) => "yield",
            ExprKind::UnsafeBinderCast(..) => "unsafe_binder_cast",
            ExprKind::Err(..) => "error",
        };
        let mut children = DirectChildren { tcx: self.tcx, references: Vec::new() };
        intravisit::walk_expr(&mut children, expr);
        fact["expression_role"] = json!(role);
        fact["children"] = json!(children.references);
        fact["control"] = match expr.kind {
            ExprKind::If(condition, then, otherwise) => json!({
                "condition": node_reference(self.tcx, condition.hir_id, condition.span),
                "then": node_reference(self.tcx, then.hir_id, then.span),
                "else": otherwise.map(|value| node_reference(self.tcx, value.hir_id, value.span)),
            }),
            ExprKind::Match(scrutinee, arms, _) => json!({
                "scrutinee": node_reference(self.tcx, scrutinee.hir_id, scrutinee.span),
                "arms": arms.iter().map(|arm| json!({
                    "pattern": node_reference(self.tcx, arm.pat.hir_id, arm.pat.span),
                    "guard": arm.guard.map(|value| node_reference(self.tcx, value.hir_id, value.span)),
                    "body": node_reference(self.tcx, arm.body.hir_id, arm.body.span),
                })).collect::<Vec<_>>(),
            }),
            ExprKind::Loop(body, _, _, _) => json!({
                "body": node_reference(self.tcx, body.hir_id, body.span),
            }),
            _ => Value::Null,
        };
        match expr.kind {
            ExprKind::Tup(elements) => {
                fact["operands"] = json!(elements.iter().map(|element|
                    node_reference(self.tcx, element.hir_id, element.span)).collect::<Vec<_>>());
            }
            ExprKind::Array(elements) => {
                fact["operands"] = json!(elements.iter().map(|element|
                    node_reference(self.tcx, element.hir_id, element.span)).collect::<Vec<_>>());
            }
            ExprKind::Repeat(value, _) => {
                fact["operands"] = json!([node_reference(self.tcx, value.hir_id, value.span)]);
            }
            ExprKind::Binary(operator, left, right) => {
                fact["operator"] = json!(operator.node.as_str());
                fact["operands"] = json!([node_reference(self.tcx, left.hir_id, left.span), node_reference(self.tcx, right.hir_id, right.span)]);
            }
            ExprKind::Unary(operator, operand) => {
                fact["operator"] = json!(operator.as_str());
                fact["operands"] = json!([node_reference(self.tcx, operand.hir_id, operand.span)]);
            }
            ExprKind::Lit(literal) => {
                fact["integer_digits"] = match literal.node {
                    rustc_ast::LitKind::Int(value, _) => json!(value.get().to_string()),
                    _ => Value::Null,
                };
            }
            ExprKind::Cast(operand, _) | ExprKind::Type(operand, _) | ExprKind::DropTemps(operand) => {
                fact["operands"] = json!([node_reference(self.tcx, operand.hir_id, operand.span)]);
            }
            ExprKind::AddrOf(_, _, operand) => {
                fact["operands"] = json!([node_reference(self.tcx, operand.hir_id, operand.span)]);
            }
            ExprKind::Path(ref path) => {
                let resolved = self.typeck.qpath_res(path, expr.hir_id);
                fact["resolution"] = resolution(self.tcx, resolved);
                if let Res::Def(kind, def) = resolved {
                    if matches!(kind, DefKind::Fn | DefKind::AssocFn | DefKind::Const |
                        DefKind::AssocConst | DefKind::AnonConst | DefKind::Static { .. } |
                        DefKind::Ctor(_, rustc_hir::def::CtorKind::Fn)) {
                        self.select(&mut fact, expr.hir_id, def);
                    }
                }
            }
            ExprKind::Block(block, _) => {
                fact["block"] = node_reference(self.tcx, block.hir_id, block.span);
            }
            ExprKind::Call(callee, arguments) => {
                fact["callee"] = node_reference(self.tcx, callee.hir_id, callee.span);
                fact["shape_query"] = match self.typeck.expr_ty(callee).kind() {
                    ty::FnDef(def, args) => args.no_bound_vars().map(|args|
                        self.shape_query(*def, args, &arguments.iter().collect::<Vec<_>>())).unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                if let ty::FnDef(def, _) = *self.typeck.expr_ty(callee).kind()
                    && let kind @ DefKind::Ctor(_, rustc_hir::def::CtorKind::Fn) = self.tcx.def_kind(def)
                    && let ty::Adt(adt, _) = self.typeck.expr_ty(expr).kind()
                {
                    let resolved = Res::Def(kind, def);
                    let fields = &adt.variant_of_res(resolved).fields;
                    fact["kind"] = json!("constructor");
                    fact["resolution"] = resolution(self.tcx, resolved);
                    fact["constructor_tail"] = json!("none");
                    fact["base"] = Value::Null;
                    fact["fields"] = json!(fields.iter().zip(arguments).map(|(field, value)|
                        json!({"definition": definition(self.tcx, field.did),
                            "origin": origin(self.tcx, value.span),
                            "value": node_reference(self.tcx, value.hir_id, value.span)})).collect::<Vec<_>>());
                } else {
                    fact["operands"] = json!(arguments.iter().map(|arg|
                        node_reference(self.tcx, arg.hir_id, arg.span)).collect::<Vec<_>>());
                }
            }
            ExprKind::MethodCall(_, receiver, arguments, _) => {
                fact["shape_query"] = Value::Null;
                if let Some(def) = self.typeck.type_dependent_def_id(expr.hir_id) {
                    fact["resolution"] = resolution(self.tcx, Res::Def(self.tcx.def_kind(def), def));
                    self.select(&mut fact, expr.hir_id, def);
                    let operands: Vec<_> = std::iter::once(receiver).chain(arguments.iter()).collect();
                    fact["shape_query"] = self.shape_query(def, self.typeck.node_args(expr.hir_id), &operands);
                }
                fact["receiver"] = node_reference(self.tcx, receiver.hir_id, receiver.span);
                fact["operands"] = json!(arguments.iter().map(|arg| node_reference(self.tcx, arg.hir_id, arg.span)).collect::<Vec<_>>());
            }
            ExprKind::Struct(path, fields, tail) => {
                fact["callee"] = Value::Null;
                let resolved = self.typeck.qpath_res(path, expr.hir_id);
                fact["resolution"] = resolution(self.tcx, resolved);
                let (tail, base) = match tail {
                    rustc_hir::StructTailExpr::None => ("none", Value::Null),
                    rustc_hir::StructTailExpr::Base(base) =>
                        ("base", node_reference(self.tcx, base.hir_id, base.span)),
                    rustc_hir::StructTailExpr::DefaultFields(_) => ("defaults", Value::Null),
                    rustc_hir::StructTailExpr::NoneWithError(_) => ("invalid", Value::Null),
                };
                fact["constructor_tail"] = json!(tail);
                fact["base"] = base;
                let mut assignments = Vec::new();
                for field in fields {
                    let mut assignment = json!({"origin": origin(self.tcx, field.span),
                        "value": node_reference(self.tcx, field.expr.hir_id, field.expr.span)});
                    if let ty::Adt(adt, _) = self.typeck.expr_ty(expr).kind() {
                        if let Some(index) = self.typeck.opt_field_index(field.hir_id) {
                            if let Some(definition_field) = adt.variant_of_res(resolved).fields.get(index) {
                                assignment["definition"] = definition(self.tcx, definition_field.did);
                            }
                        }
                    }
                    assignments.push(assignment);
                }
                fact["fields"] = json!(assignments);
            }
            ExprKind::Field(base, _) => {
                fact["receiver"] = node_reference(self.tcx, base.hir_id, base.span);
                fact["field"] = Value::Null;
                let typeck = self.typeck;
                if let Some(index) = typeck.opt_field_index(expr.hir_id) {
                    match typeck.expr_ty_adjusted(base).kind() {
                        ty::Adt(adt, _) if !adt.is_enum() => {
                            if let Some(field) = adt.non_enum_variant().fields.get(index) {
                                fact["field"] = json!({"kind": "adt", "definition": definition(self.tcx, field.did)});
                            }
                        }
                        ty::Tuple(_) => fact["field"] = json!({"kind": "tuple", "index": index.as_u32()}),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        self.nodes.push(fact);
        intravisit::walk_expr(self, expr);
    }

    fn visit_pat(&mut self, pat: &'tcx rustc_hir::Pat<'tcx>) {
        let mut fact = self.node(pat.hir_id, pat.span, "pattern");
        match pat.kind {
            rustc_hir::PatKind::Binding(_, id, name, _) => {
                fact["role"] = json!("binding");
                fact["name"] = json!(name.name.to_string());
                fact["body_owner"] = definition(self.tcx, self.body_owner.to_def_id());
                fact["type_instantiations"] = json!(self.type_instantiations(pat.hir_id));
                fact["binding_reference"] = node_reference(self.tcx, id, self.tcx.hir_span(id));
                fact["binding_mode"] = self.typeck.pat_binding_modes().get(pat.hir_id).map(|mode| {
                    match mode.0 {
                        rustc_hir::ByRef::No => "value",
                        rustc_hir::ByRef::Yes(rustc_hir::Pinnedness::Not, rustc_hir::Mutability::Not) => "shared",
                        rustc_hir::ByRef::Yes(rustc_hir::Pinnedness::Not, rustc_hir::Mutability::Mut) => "mutable",
                        rustc_hir::ByRef::Yes(_, rustc_hir::Mutability::Not) => "pinned_shared",
                        rustc_hir::ByRef::Yes(_, rustc_hir::Mutability::Mut) => "pinned_mutable",
                    }
                }).map(Value::from).unwrap_or(Value::Null);
                fact["mutable"] = json!(self.typeck.pat_binding_modes().get(pat.hir_id)
                    .map(|mode| mode.1 == rustc_hir::Mutability::Mut));
                let parent = self.tcx.hir_parent_iter(pat.hir_id).find(|(_, node)| {
                    !matches!(node, rustc_hir::Node::Pat(..) | rustc_hir::Node::PatField(..))
                });
                let local = parent.and_then(|(_, node)| match node {
                    rustc_hir::Node::LetStmt(local) => Some(local),
                    _ => None,
                });
                fact["declaration"] = local.map(|local|
                    node_reference(self.tcx, local.hir_id, local.span)).unwrap_or(Value::Null);
                let (projection, error) = match local.map(|local| self.initializer_projection(pat, local.hir_id)) {
                    Some(Ok(projection)) => (json!(projection), Value::Null),
                    Some(Err(error)) => (Value::Null, json!(error)),
                    None => (Value::Null, Value::Null),
                };
                fact["initializer_projection"] = projection;
                fact["projection_error"] = error;
            }
            rustc_hir::PatKind::Struct(ref path, ..) | rustc_hir::PatKind::TupleStruct(ref path, ..) => {
                fact["role"] = json!("constructor");
                fact["resolution"] = resolution(self.tcx, self.typeck.qpath_res(path, pat.hir_id));
            }
            _ => fact["role"] = json!("non_binding"),
        }
        self.nodes.push(fact);
        intravisit::walk_pat(self, pat);
    }

    fn visit_pat_expr(&mut self, expr: &'tcx rustc_hir::PatExpr<'tcx>) {
        let mut fact = self.node(expr.hir_id, expr.span, "pattern_expression");
        if let rustc_hir::PatExprKind::Path(ref path) = expr.kind {
            fact["resolution"] = resolution(self.tcx, self.typeck.qpath_res(path, expr.hir_id));
        }
        self.nodes.push(fact);
        intravisit::walk_pat_expr(self, expr);
    }
}

struct ItemReferences<'tcx, 'records> {
    tcx: TyCtxt<'tcx>,
    nodes: &'records mut RecordWriter,
}

impl<'tcx> Visitor<'tcx> for ItemReferences<'tcx, '_> {
    fn visit_ty(&mut self, ty: &'tcx rustc_hir::Ty<'tcx, rustc_hir::AmbigArg>) {
        let mut fact = json!({"kind": "type_site", "origin": origin(self.tcx, ty.span)});
        if let rustc_hir::TyKind::Path(rustc_hir::QPath::Resolved(_, path)) = ty.kind {
            fact["resolution"] = resolution(self.tcx, path.res);
        }
        self.nodes.push(fact);
        intravisit::walk_ty(self, ty);
    }

    fn visit_const_arg(&mut self, arg: &'tcx rustc_hir::ConstArg<'tcx, rustc_hir::AmbigArg>) {
        self.nodes.push(const_argument(self.tcx, arg));
        intravisit::walk_const_arg(self, arg);
    }
}

struct Capture {
    standard: rustc_driver::TimePassesCallbacks,
    args: Vec<String>,
    pending: Option<Value>,
    nodes: RecordWriter,
    definitions: RecordWriter,
    loaded: Arc<Mutex<LoadedInputs>>,
    error: Option<String>,
    entry: Option<crate::entry_inputs::Capture>,
}

impl rustc_driver::Callbacks for Capture {
    fn config(&mut self, config: &mut interface::Config) {
        self.standard.config(config);
        if let Some(entry) = &mut self.entry {
            if let Err(error) = entry.configure(config) { self.error = Some(error); }
        }
        config.file_loader = Some(Box::new(RecordingLoader {
            inner: config.file_loader.take().unwrap_or_else(|| Box::new(RealFileLoader)),
            inputs: Arc::clone(&self.loaded),
        }));
    }

    fn after_analysis<'tcx>(&mut self, _compiler: &interface::Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        tcx.dcx().abort_if_errors();
        if self.error.is_some() { return Compilation::Stop; }
        match rustc_middle::ty::print::with_no_trimmed_paths!(self.collect(tcx)) {
            Ok(mut bytes) => {
                if let Some(entry) = &mut self.entry {
                    if let Err(error) = tcx.sess.prof.verbose_generic_activity("asksource_entry_inputs")
                        .run(|| entry.pending(tcx, &mut bytes)) {
                        self.error = Some(error);
                        return Compilation::Stop;
                    }
                }
                self.pending = Some(bytes);
            }
            Err(error) => {
                self.error = Some(error);
                return Compilation::Stop;
            }
        }
        Compilation::Continue
    }
}

impl Capture {
    fn collect(&mut self, tcx: TyCtxt<'_>) -> Result<Value, String> {
        let _collection = tcx.sess.prof.verbose_generic_activity("asksource_collect");
        tcx.sess.prof.verbose_generic_activity("asksource_items").run(|| {
            let mut items = ItemReferences { tcx, nodes: &mut self.nodes };
            tcx.hir_visit_all_item_likes_in_crate(&mut items);
        });
        self.nodes.check()?;
        let mut bodies = HashMap::new();
        let instances = tcx.sess.prof.verbose_generic_activity("asksource_instantiated_storage")
            .run(|| instantiated_storage(tcx, &mut self.nodes));
        self.nodes.check()?;
        let hir_activity = tcx.sess.prof.verbose_generic_activity("asksource_hir");
        for owner in tcx.hir_body_owners() {
            let mut facts = Facts { tcx, typeck: tcx.typeck(owner), body_owner: owner, nodes: &mut self.nodes,
                instances: instances.get(&owner).map(Vec::as_slice).unwrap_or(&[]) };
            let body = tcx.hir_body_owned_by(owner);
            bodies.insert(owner, node_reference(tcx, body.value.hir_id, body.value.span));
            facts.visit_body(body);
            self.nodes.check()?;
        }
        drop(hir_activity);
        let definitions_activity = tcx.sess.prof.verbose_generic_activity("asksource_definitions");
        for id in tcx.iter_local_def_id() {
            let kind = tcx.def_kind(id);
            let ty = if matches!(kind, DefKind::Const | DefKind::Static { .. } | DefKind::AssocConst |
                DefKind::TyAlias | DefKind::Field | DefKind::Struct | DefKind::Enum | DefKind::Union) {
                let input = tcx.type_of(id).instantiate_identity();
                match tcx.try_normalize_erasing_regions(ty::TypingEnv::non_body_analysis(tcx, id), input) {
                    Ok(value) => Some(type_fact(tcx, value, ty::TypingEnv::non_body_analysis(tcx, id))),
                    Err(error) => Some(json!({"kind": "unresolved", "error": format!("{error:?}")})),
                }
            } else {
                None
            };
            let body_owner = if kind == DefKind::Field {
                match tcx.hir_node_by_def_id(id) {
                    rustc_hir::Node::Field(field) => field.default.map(|value| value.def_id),
                    _ => None,
                }
            } else {
                Some(id)
            };
            let (parameters, parameter_error) = if matches!(kind, DefKind::Fn | DefKind::AssocFn |
                DefKind::Const | DefKind::AssocConst | DefKind::AnonConst | DefKind::Closure |
                DefKind::Ctor(..) | DefKind::Static { .. } | DefKind::SyntheticCoroutineBody) {
                let generics = tcx.generics_of(id);
                let parameters: Result<Vec<_>, _> = (0..generics.count()).map(|index| {
                    let parameter = generics.param_at(index, tcx);
                    parameter_role(tcx, parameter).map(|role|
                        json!({"definition": definition(tcx, parameter.def_id), "role": role}))
                        .ok_or("compiler declaration has an unsupported parameter role")
                }).collect();
                match parameters {
                    Ok(parameters) => (json!(parameters), Value::Null),
                    Err(error) => (Value::Null, json!(error)),
                }
            } else {
                (Value::Null, Value::Null)
            };
            self.definitions.push(json!({"definition": definition(tcx, id.to_def_id()), "kind": format!("{kind:?}"),
                "origin": origin(tcx, tcx.def_span(id)), "type": ty,
                "generic_parameters": parameters, "generic_parameter_error": parameter_error,
                "body": body_owner.and_then(|owner| bodies.get(&owner))}));
            self.definitions.check()?;
        }
        drop(definitions_activity);
        let metadata_activity = tcx.sess.prof.verbose_generic_activity("asksource_metadata");
        let mut artifacts = BTreeMap::new();
        let mut crate_sources = BTreeMap::new();
        for &krate in tcx.crates(()).iter() {
            let source = tcx.used_crate_source(krate);
            let mut files = BTreeMap::new();
            for path in source.paths().chain(source.sdylib_interface.iter()) {
                let path = fs::canonicalize(path).map_err(|e| e.to_string())?;
                let hash = digest(&fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?);
                artifacts.insert(path.clone(), hash.clone());
                files.insert(path, hash);
            }
            crate_sources.insert(krate, (tcx.crate_name(krate).to_string(), files));
        }
        let mut sources = BTreeMap::new();
        for file in tcx.sess.source_map().files().iter() {
            if file.is_imported() {
                let (krate, files) = crate_sources.get(&file.cnum)
                    .ok_or_else(|| format!("imported source has no native crate artifacts: {}",
                        file.name.prefer_local_unconditionally()))?;
                if files.is_empty() { return Err(format!("imported source has no crate inputs: {}",
                    file.name.prefer_local_unconditionally())); }
                let id = format!("{:?}", file.stable_id);
                let record = json!({"kind": "metadata", "name": file.name.prefer_local_unconditionally().to_string(),
                    "path": None::<String>, "text": None::<String>, "compiler_hash": file.src_hash.to_string(),
                    "source_file_id": id, "exported_source_file_id": id,
                    "sha256": None::<String>, "normalized_sha256": None::<String>,
                    "crate": krate, "artifacts": files, "byte_length": file.unnormalized_source_len});
                if sources.insert(id, record).is_some() { return Err("duplicate compiler source identity".into()); }
                continue;
            }
            if let Some(text) = &file.src {
                let name = file.name.prefer_local_unconditionally().to_string();
                let generated = !matches!(file.name, rustc_span::FileName::Real(_));
                let raw = if generated { text.as_bytes().to_vec() } else {
                    fs::read(&name).map_err(|e| format!("cannot fingerprint compiler source {name}: {e}"))?
                };
                let binary = !generated && std::str::from_utf8(&raw).is_err() && self.loaded.lock()
                    .map_err(|_| "input recorder poisoned")?.binary.contains(&std::path::absolute(&name).map_err(|e| e.to_string())?);
                let compiler_bytes = if binary { &[][..] } else { raw.as_slice() };
                if !generated && rustc_span::SourceFileHash::new_in_memory(file.src_hash.kind, compiler_bytes) != file.src_hash {
                    return Err(format!("compiler source changed after loading: {name}"));
                }
                let mut exported_name = file.name.clone();
                if let rustc_span::FileName::Real(name) = &mut exported_name {
                    name.update_for_crate_metadata();
                }
                let exported_id = rustc_span::StableSourceFileId::from_filename_for_export(
                    &exported_name, tcx.stable_crate_id(rustc_hir::def_id::LOCAL_CRATE));
                let id = format!("{:?}", file.stable_id);
                let record = json!({"kind": if generated { "generated" } else if binary { "binary" } else { "text" },
                    "name": name, "path": if generated { None } else { Some(name.as_str()) },
                    "text": if generated { Some(text.as_str()) } else { None },
                    "compiler_hash": file.src_hash.to_string(),
                    "source_file_id": id, "exported_source_file_id": format!("{exported_id:?}"),
                    "sha256": digest(&raw), "normalized_sha256": digest(text.as_bytes())});
                if sources.insert(id, record).is_some() { return Err("duplicate compiler source identity".into()); }
            }
        }
        let mut inputs = BTreeMap::new();
        for path in tcx.sess.file_depinfo.borrow().iter() {
            let path = path.as_str();
            let bytes = fs::read(path).map_err(|e| format!("cannot fingerprint compiler input {path}: {e}"))?;
            inputs.insert(path.to_owned(), digest(&bytes));
        }
        let mut environment = BTreeMap::new();
        for (name, value) in tcx.sess.env_depinfo.borrow().iter() {
            environment.insert(name.as_str().to_owned(), value.map(|v| digest(v.as_str().as_bytes())));
        }
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let helper_hash = digest(&fs::read(&executable).map_err(|e| e.to_string())?);
        let compiler_hash = digest(&fs::read(self.args.first().ok_or("missing compiler")?).map_err(|e| e.to_string())?);
        let sysroot = fs::canonicalize(tcx.sess.opts.sysroot.path()).map_err(|e| e.to_string())?;
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let outputs = tcx.output_filenames(());
        let mut output_paths = Vec::new();
        for (kind, _) in outputs.outputs.iter() {
            if *kind == rustc_session::config::OutputType::Exe {
                for &crate_type in tcx.crate_types() {
                    let output = rustc_session::output::out_filename(tcx.sess, crate_type, outputs,
                        tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE));
                    if output.is_stdout() { return Err("capture requires file-backed compiler outputs".into()); }
                    output_paths.push(std::path::absolute(output.as_path()).map_err(|e| e.to_string())?);
                }
            } else {
                let output = outputs.path(*kind);
                if output.is_stdout() { return Err("capture requires file-backed compiler outputs".into()); }
                output_paths.push(std::path::absolute(output.as_path()).map_err(|e| e.to_string())?);
            }
        }
        let mut response = json!({"complete": true, "output_paths": output_paths, "arguments": self.args, "sources": sources,
            "cwd": cwd, "sysroot": sysroot, "dependency_artifacts": artifacts,
            "cfg": tcx.sess.config.iter().map(|(name, value)| json!([name.as_str(), value.map(|v| v.as_str().to_owned())])).collect::<Vec<_>>(),
            "helper_sha256": helper_hash, "compiler_sha256": compiler_hash,
            "file_inputs": inputs, "environment_inputs": environment});
        drop(metadata_activity);
        if tcx.output_filenames(()).outputs.should_codegen() {
            response["codegen_modules"] = json!(tcx.sess.prof.verbose_generic_activity("asksource_codegen_modules")
                .run(|| crate::entry_inputs::codegen_modules(tcx, &response))?);
            response["resolved_mir"] = json!(tcx.sess.prof.verbose_generic_activity("asksource_resolved_mir")
                .run(|| crate::entry_inputs::resolved_mir(tcx))?);
        }
        Ok(response)
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    use cargo::util::command_prelude::*;
    let matches = Command::new("capture")
        .arg(Arg::new("output").long("output").required(true).value_parser(value_parser!(PathBuf)))
        .arg(Arg::new("request").long("request").requires("request-sha256").value_parser(value_parser!(PathBuf)))
        .arg(Arg::new("request-sha256").long("request-sha256").requires("request"))
        .arg(Arg::new("launcher").long("launcher").requires("request"))
        .arg(Arg::new("entry-input-request").long("entry-input-request")
            .requires("entry-input-request-sha256").value_parser(value_parser!(PathBuf)))
        .arg(Arg::new("entry-input-request-sha256").long("entry-input-request-sha256").requires("entry-input-request"))
        .arg(Arg::new("compiler").required(true).num_args(1..).last(true))
        .try_get_matches_from(std::iter::once("capture").chain(args.iter().map(String::as_str)))
        .map_err(|e| e.to_string())?;
    let args: Vec<String> = matches.get_many::<String>("compiler").ok_or("missing compiler arguments")?
        .cloned().collect();
    let destination = matches.get_one::<PathBuf>("output").cloned().ok_or("missing capture output")?;
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let compiler_name = if cfg!(windows) { "rustc.exe" } else { "rustc" };
    let expected = fs::canonicalize(executable.with_file_name(compiler_name)).map_err(|e| e.to_string())?;
    let requested = fs::canonicalize(&args[0]).map_err(|e| e.to_string())?;
    if requested != expected {
        return Err(format!("compiler {} does not match helper compiler {}; build the helper for the selected compiler",
                           requested.display(), expected.display()));
    }
    if !destination.is_absolute() {
        return Err("capture destination must be absolute".into());
    }
    let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(destination.with_extension("lock")).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let mut tool_inputs = BTreeMap::new();
    for path in [&executable, &requested] {
        tool_inputs.insert(path.clone(), digest(&fs::read(path).map_err(|e| e.to_string())?));
    }
    let root = expected.parent().and_then(Path::parent).ok_or("compiler has no sysroot")?;
    for directory in [root.join("lib"), root.join("bin")] {
        for entry in fs::read_dir(&directory).map_err(|e| format!("{}: {e}", directory.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.path().is_file() && (name.starts_with("librustc_driver-") || name.starts_with("rustc_driver-")) {
                let path = entry.path();
                tool_inputs.insert(path.clone(), digest(&fs::read(&path).map_err(|e| e.to_string())?));
            }
        }
    }
    let request = if let Some(path) = matches.get_one::<PathBuf>("request") {
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        let expected = matches.get_one::<String>("request-sha256").ok_or("missing capture request fingerprint")?;
        if digest(&bytes) != *expected { return Err("capture request changed after executor selection".into()); }
        tool_inputs.insert(fs::canonicalize(path).map_err(|e| e.to_string())?, expected.clone());
        Some({
            let request = serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())?;
            crate::launcher::validate_request(&request)?;
            request
        })
    } else { None };
    let launcher = matches.get_one::<String>("launcher").map(|value| serde_json::from_str::<Value>(value))
        .transpose().map_err(|e| e.to_string())?.unwrap_or(Value::Null);
    if !launcher.is_null() {
        crate::launcher::validate(&launcher, request.as_ref().ok_or("launcher capture requires a request")?)?;
        tool_inputs.insert(PathBuf::from(launcher["program"].as_str().ok_or("missing launcher program")?),
            launcher["sha256"].as_str().ok_or("missing launcher hash")?.to_owned());
        for (name, hash) in launcher["response_files"].as_object().ok_or("missing launcher response inputs")? {
            tool_inputs.insert(PathBuf::from(name), hash.as_str().ok_or("missing response hash")?.to_owned());
        }
    }
    let effective_environment = crate::launcher::environment(&Default::default())?;
    let entry = matches.get_one::<PathBuf>("entry-input-request").map(|path| {
        crate::entry_inputs::Capture::load(path,
            matches.get_one::<String>("entry-input-request-sha256").ok_or("missing entry request hash")?, &destination, &args)
    }).transpose()?;
    if let Some(entry) = &entry { tool_inputs.extend(entry.tracked_inputs()?); }
    let mut capture = Capture { standard: rustc_driver::TimePassesCallbacks::default(), args: args.clone(), pending: None, error: None, entry,
        nodes: RecordWriter::new(&destination, records::field::NODES)?,
        definitions: RecordWriter::new(&destination, records::field::DEFINITIONS)?,
        loaded: Arc::new(Mutex::new(LoadedInputs::default())) };
    rustc_driver::catch_fatal_errors(|| compiler_entrypoint(&args, &mut capture))
        .map_err(|_| "compiler refused capture".to_owned())?;
    if let Some(error) = capture.error {
        return Err(error);
    }
    let mut facts = capture.pending.ok_or("compiler invocation produced no analysis facts")?;
    for (path, hash) in &tool_inputs {
        if digest(&fs::read(path).map_err(|e| e.to_string())?) != *hash {
            return Err(format!("compiler/helper input changed during capture: {}", path.display()));
        }
    }
    facts["compiler_realpath"] = json!(requested);
    facts["launcher"] = launcher;
    facts["effective_environment"] = effective_environment;
    facts["tool_inputs"] = serde_json::to_value(tool_inputs).map_err(|e| e.to_string())?;
    facts["capture_request"] = serde_json::to_value(request).map_err(|e| e.to_string())?;
    let output_paths: Vec<PathBuf> = serde_json::from_value(facts["output_paths"].take()).map_err(|e| e.to_string())?;
    let mut output_artifacts = BTreeMap::new();
    for path in output_paths {
        output_artifacts.insert(path.clone(), digest(&fs::read(&path).map_err(|e| format!("compiler output {}: {e}", path.display()))?));
    }
    facts["output_artifacts"] = serde_json::to_value(output_artifacts).map_err(|e| e.to_string())?;
    let loaded = capture.loaded.lock().map_err(|_| "input recorder poisoned")?;
    if let Some(error) = &loaded.error { return Err(error.clone()); }
    for (path, hash) in &loaded.files {
        let current = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if digest(&current) != *hash {
            return Err(format!("compiler input changed after loading: {}", path.display()));
        }
    }
    for (path, existed) in &loaded.exists {
        if path.exists() != *existed {
            return Err(format!("compiler input existence changed: {}", path.display()));
        }
    }
    let artifacts = facts["dependency_artifacts"].as_object().ok_or("missing dependency artifacts")?;
    for (path, hash) in artifacts {
        if digest(&fs::read(path).map_err(|e| format!("{path}: {e}"))?) != hash.as_str().ok_or("invalid artifact hash")? {
            return Err(format!("compiler dependency changed after analysis: {path}"));
        }
    }
    facts["loaded_inputs"] = serde_json::to_value(&loaded.files).map_err(|e| e.to_string())?;
    facts["input_existence"] = serde_json::to_value(&loaded.exists).map_err(|e| e.to_string())?;
    facts[records::field::NODES] = capture.nodes.finish()?;
    facts[records::field::DEFINITIONS] = capture.definitions.finish()?;
    let bytes = serde_json::to_vec(&facts).map_err(|e| e.to_string())?;
    let temporary = destination.with_extension("partial");
    fs::write(&temporary, bytes).map_err(|e| format!("{}: {e}", temporary.display()))?;
    fs::rename(&temporary, &destination).map_err(|e| format!("{}: {e}", destination.display()))?;
    if let Some(entry) = &capture.entry { entry.complete(&destination)?; }
    Ok(())
}
