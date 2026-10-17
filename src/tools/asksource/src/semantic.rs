use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustc_span::source_map::{FileLoader, RealFileLoader};

use rustc_driver::Compilation;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::intravisit::{self, Visitor};
use rustc_interface::interface;
use rustc_middle::ty::{self, Ty, TyCtxt};
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

fn type_identity<'tcx>(tcx: TyCtxt<'tcx>, value: Ty<'tcx>) -> Value {
    match value.kind() {
        ty::Array(element, extent) => json!({"kind": "array", "element": type_identity(tcx, *element),
                                           "extent": constant(tcx, *extent)}),
        ty::Slice(element) => json!({"kind": "slice", "element": type_identity(tcx, *element)}),
        ty::Ref(_, element, _) => json!({"kind": "reference", "element": type_identity(tcx, *element)}),
        ty::RawPtr(element, _) => json!({"kind": "pointer", "element": type_identity(tcx, *element)}),
        ty::Tuple(elements) => json!({"kind": "tuple", "elements": elements.as_slice().iter().map(|ty| type_identity(tcx, *ty)).collect::<Vec<_>>()}),
        ty::Adt(def, args) => json!({"kind": "adt", "definition": definition(tcx, def.did()),
            "arguments": args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()}),
        ty::FnDef(def, args) => json!({"kind": "function", "definition": definition(tcx, *def),
            "bound_variables": format!("{:?}", args.bound_vars()),
            "arguments": args.skip_binder().as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()}),
        ty::Param(param) => json!({"kind": "parameter", "index": param.index, "name": param.name.to_string()}),
        ty::Closure(def, _) | ty::CoroutineClosure(def, _) | ty::Coroutine(def, _) =>
            json!({"kind": "closure_or_coroutine", "definition": definition(tcx, *def)}),
        ty::Alias(kind, alias) => {
            let def = match alias.kind {
                ty::AliasTyKind::Projection { def_id } | ty::AliasTyKind::Inherent { def_id } |
                ty::AliasTyKind::Opaque { def_id } | ty::AliasTyKind::Free { def_id } => def_id,
            };
            json!({"kind": "alias", "alias_kind": format!("{kind:?}"), "definition": definition(tcx, def),
                "arguments": alias.args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()})
        }
        _ => json!({"kind": "scalar_or_other", "display": value.to_string()}),
    }
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
                    let fields: Vec<_> = layout.field_tys.iter().filter_map(|field| {
                        let input = ty::EarlyBinder::bind(tcx, field.ty).instantiate(tcx, args);
                        let storage = match tcx.try_normalize_erasing_regions(typing_env, input) {
                            Ok(value) => inline_storage(tcx, value, typing_env, ancestors),
                            Err(error) => json!({"kind": "unresolved", "error": format!("{error:?}")}),
                        };
                        (!storage.is_null()).then(|| json!({"origin": origin(tcx, field.source_info.span),
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
    fact["inline_storage"] = inline_storage(tcx, value, typing_env, &mut Vec::new());
    fact
}

fn instance_fact<'tcx>(tcx: TyCtxt<'tcx>, instance: ty::Instance<'tcx>) -> Value {
    json!({"definition": definition(tcx, instance.def_id()),
        "symbol": tcx.symbol_name(instance).name,
        "arguments": instance.args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()})
}

fn instantiated_storage<'tcx>(tcx: TyCtxt<'tcx>, nodes: &mut RecordWriter) {
    let mut visited = HashSet::new();
    for unit in tcx.collect_and_partition_mono_items(()).codegen_units {
        for (item, _) in unit.items_in_deterministic_order(tcx) {
            let MonoItem::Fn(instance) = item else { continue };
            if !visited.insert(instance) { continue; }
            let body = tcx.instance_mir(instance.def);
            let descriptor = instance_fact(tcx, instance);
            for (local, declaration) in body.local_decls.iter_enumerated() {
                let ty = match instance.try_instantiate_mir_and_normalize_erasing_regions(
                    tcx, ty::TypingEnv::fully_monomorphized(), ty::EarlyBinder::bind(tcx, declaration.ty)) {
                    Ok(value) => type_fact(tcx, value, ty::TypingEnv::fully_monomorphized()),
                    Err(error) => json!({"inline_storage": {"kind": "unresolved", "error": format!("{error:?}")}}),
                };
                if ty["inline_storage"].is_null() { continue; }
                let mut inlined = Vec::new();
                let mut owner = instance.def_id();
                let mut current = Some(declaration.source_info.scope);
                while let Some(index) = current {
                    let Some(scope) = body.source_scopes.get(index) else { break };
                    if let Some((callee, call_site)) = scope.inlined {
                        match instance.try_instantiate_mir_and_normalize_erasing_regions(
                            tcx, ty::TypingEnv::fully_monomorphized(), ty::EarlyBinder::bind(tcx, callee)) {
                            Ok(callee) => {
                                if inlined.is_empty() { owner = callee.def_id(); }
                                inlined.push(json!({"instance": instance_fact(tcx, callee), "origin": origin(tcx, call_site)}));
                            }
                            Err(error) => inlined.push(json!({"unresolved": format!("{error:?}")})),
                        }
                    }
                    current = scope.parent_scope;
                }
                let names: Vec<_> = body.var_debug_info.iter().filter_map(|variable| {
                    if let rustc_middle::mir::VarDebugInfoContents::Place(place) = &variable.value {
                        if place.local == local && place.projection.is_empty() {
                            return Some(json!({"name": variable.name.to_string(),
                                "origin": origin(tcx, variable.source_info.span)}));
                        }
                    }
                    None
                }).collect();
                nodes.push(json!({"kind": "instantiated_storage", "owner": definition(tcx, owner),
                    "origin": origin(tcx, declaration.source_info.span), "instance": descriptor,
                    "inlined": inlined, "names": names, "type": ty}));
            }
        }
    }
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

fn node_reference(tcx: TyCtxt<'_>, id: rustc_hir::HirId, span: Span) -> Value {
    json!({"local_id": id.local_id.as_u32(),
        "owner": definition(tcx, id.owner.def_id.to_def_id()), "origin": origin(tcx, span)})
}

struct Facts<'tcx, 'records> {
    tcx: TyCtxt<'tcx>,
    typeck: &'tcx ty::TypeckResults<'tcx>,
    nodes: &'records mut RecordWriter,
}

impl<'tcx> Facts<'tcx, '_> {
    fn node(&self, id: rustc_hir::HirId, span: Span, kind: &str) -> Value {
        let tcx = self.tcx;
        let typeck = self.typeck;
        json!({"kind": kind, "local_id": id.local_id.as_u32(),
            "owner": definition(tcx, id.owner.def_id.to_def_id()), "origin": origin(tcx, span),
            "type": typeck.node_type_opt(id).map(|value|
                type_fact(tcx, value, ty::TypingEnv::post_analysis(tcx, id.owner.def_id))),
            "arguments": typeck.node_args_opt(id).map(|args| args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()),
            "selected_definition": typeck.type_dependent_def_id(id).map(|def| definition(tcx, def))})
    }
}

impl<'tcx> Visitor<'tcx> for Facts<'tcx, '_> {
    fn visit_block(&mut self, block: &'tcx rustc_hir::Block<'tcx>) {
        let fact = self.node(block.hir_id, block.span, "block");
        self.nodes.push(fact);
        intravisit::walk_block(self, block);
    }

    fn visit_local(&mut self, local: &'tcx rustc_hir::LetStmt<'tcx>) {
        let mut fact = self.node(local.hir_id, local.span, "local");
        fact["pattern"] = node_reference(self.tcx, local.pat.hir_id, local.pat.span);
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
            ExprKind::Binary(..) => "binary",
            ExprKind::Unary(..) => "unary",
            ExprKind::Lit(..) => "literal",
            ExprKind::Cast(..) => "cast",
            ExprKind::Type(..) => "type_ascription",
            ExprKind::DropTemps(..) => "drop_temps",
            _ => "expression",
        };
        let mut fact = self.node(expr.hir_id, expr.span, kind);
        match expr.kind {
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
            ExprKind::Path(ref path) => {
                fact["resolution"] = resolution(self.tcx, self.typeck.qpath_res(path, expr.hir_id));
            }
            ExprKind::Call(callee, arguments) => {
                fact["callee"] = node_reference(self.tcx, callee.hir_id, callee.span);
                fact["operands"] = json!(arguments.iter().map(|arg| node_reference(self.tcx, arg.hir_id, arg.span)).collect::<Vec<_>>());
            }
            ExprKind::MethodCall(_, receiver, arguments, _) => {
                fact["receiver"] = node_reference(self.tcx, receiver.hir_id, receiver.span);
                fact["operands"] = json!(arguments.iter().map(|arg| node_reference(self.tcx, arg.hir_id, arg.span)).collect::<Vec<_>>());
            }
            ExprKind::Struct(path, fields, ..) => {
                let resolved = self.typeck.qpath_res(path, expr.hir_id);
                fact["resolution"] = resolution(self.tcx, resolved);
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
                let typeck = self.typeck;
                if let Some(index) = typeck.opt_field_index(expr.hir_id) {
                    if let ty::Adt(adt, _) = typeck.expr_ty_adjusted(base).kind() {
                        if let Some(field) = adt.non_enum_variant().fields.get(index) {
                            fact["field_definition"] = definition(self.tcx, field.did);
                        }
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
                fact["binding_reference"] = node_reference(self.tcx, id, self.tcx.hir_span(id));
                fact["binding_mode"] = json!(format!("{:?}", self.typeck.pat_binding_modes().get(pat.hir_id)));
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
        let mut fact = json!({"kind": "const_argument", "origin": origin(self.tcx, arg.span)});
        if let rustc_hir::ConstArgKind::Path(rustc_hir::QPath::Resolved(_, path)) = arg.kind {
            fact["resolution"] = resolution(self.tcx, path.res);
        }
        self.nodes.push(fact);
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
}

impl rustc_driver::Callbacks for Capture {
    fn config(&mut self, config: &mut interface::Config) {
        self.standard.config(config);
        config.file_loader = Some(Box::new(RecordingLoader {
            inner: config.file_loader.take().unwrap_or_else(|| Box::new(RealFileLoader)),
            inputs: Arc::clone(&self.loaded),
        }));
    }

    fn after_analysis<'tcx>(&mut self, _compiler: &interface::Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        tcx.dcx().abort_if_errors();
        match rustc_middle::ty::print::with_no_trimmed_paths!(self.collect(tcx)) {
            Ok(bytes) => self.pending = Some(bytes),
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
        let mut items = ItemReferences { tcx, nodes: &mut self.nodes };
        tcx.hir_visit_all_item_likes_in_crate(&mut items);
        self.nodes.check()?;
        for owner in tcx.hir_body_owners() {
            let mut facts = Facts { tcx, typeck: tcx.typeck(owner), nodes: &mut self.nodes };
            facts.visit_body(tcx.hir_body_owned_by(owner));
            self.nodes.check()?;
        }
        instantiated_storage(tcx, &mut self.nodes);
        self.nodes.check()?;
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
            self.definitions.push(json!({"definition": definition(tcx, id.to_def_id()), "kind": format!("{kind:?}"),
                "origin": origin(tcx, tcx.def_span(id)), "type": ty}));
            self.definitions.check()?;
        }
        let mut sources = BTreeMap::new();
        for file in tcx.sess.source_map().files().iter() {
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
        let mut artifacts = BTreeMap::new();
        for &krate in tcx.crates(()).iter() {
            let source = tcx.used_crate_source(krate);
            for path in source.paths().chain(source.sdylib_interface.iter()) {
                let path = fs::canonicalize(path).map_err(|e| e.to_string())?;
                let hash = digest(&fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?);
                artifacts.insert(path, hash);
            }
        }
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
        let response = json!({"complete": true, "output_paths": output_paths, "arguments": self.args, "sources": sources,
            "cwd": cwd, "sysroot": sysroot, "dependency_artifacts": artifacts,
            "cfg": tcx.sess.config.iter().map(|(name, value)| json!([name.as_str(), value.map(|v| v.as_str().to_owned())])).collect::<Vec<_>>(),
            "helper_sha256": helper_hash, "compiler_sha256": compiler_hash,
            "file_inputs": inputs, "environment_inputs": environment});
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
    let mut capture = Capture { standard: rustc_driver::TimePassesCallbacks::default(), args: args.clone(), pending: None, error: None,
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
    fs::rename(&temporary, &destination).map_err(|e| format!("{}: {e}", destination.display()))
}
