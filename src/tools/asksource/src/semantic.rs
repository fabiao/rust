use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use rustc_driver::Compilation;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::intravisit::{self, Visitor};
use rustc_interface::interface;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_middle::ty::consts::ConstExt;
use rustc_span::Span;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(feature = "snapshot-driver")]
use rustc_driver::run_compiler as compiler_entrypoint;
#[cfg(not(feature = "snapshot-driver"))]
use rustc_driver::compiler_entrypoint;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn location(tcx: TyCtxt<'_>, span: Span) -> Value {
    if span.is_dummy() {
        return Value::Null;
    }
    let map = tcx.sess.source_map();
    let start = map.lookup_byte_offset(span.lo());
    let end = map.lookup_byte_offset(span.hi());
    json!({"path": start.sf.name.prefer_local_unconditionally().to_string(),
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
        ty::GenericArgKind::Type(ty) => type_fact(tcx, ty),
        ty::GenericArgKind::Const(value) => json!({"kind": "const", "constant": constant(tcx, value)}),
        ty::GenericArgKind::Lifetime(_) => json!({"kind": "lifetime"}),
    }
}

fn type_fact<'tcx>(tcx: TyCtxt<'tcx>, value: Ty<'tcx>) -> Value {
    match value.kind() {
        ty::Array(element, extent) => json!({"kind": "array", "element": type_fact(tcx, *element),
                                           "extent": constant(tcx, *extent)}),
        ty::Slice(element) => json!({"kind": "slice", "element": type_fact(tcx, *element)}),
        ty::Ref(_, element, _) => json!({"kind": "reference", "element": type_fact(tcx, *element)}),
        ty::RawPtr(element, _) => json!({"kind": "pointer", "element": type_fact(tcx, *element)}),
        ty::Tuple(elements) => json!({"kind": "tuple", "elements": elements.as_slice().iter().map(|ty| type_fact(tcx, *ty)).collect::<Vec<_>>()}),
        ty::Adt(def, args) => json!({"kind": "adt", "definition": definition(tcx, def.did()),
            "arguments": args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()}),
        ty::FnDef(def, args) => json!({"kind": "function", "definition": definition(tcx, *def),
            "bound_variables": format!("{:?}", args.bound_vars()),
            "arguments": args.skip_binder().as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()}),
        ty::Param(param) => json!({"kind": "parameter", "index": param.index, "name": param.name.to_string()}),
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

fn resolution(tcx: TyCtxt<'_>, res: Res) -> Value {
    match res {
        Res::Def(kind, def) => json!({"kind": format!("{kind:?}"), "definition": definition(tcx, def),
                                    "origin": origin(tcx, tcx.def_span(def))}),
        Res::Local(id) => json!({"kind": "local", "owner": definition(tcx, id.owner.def_id.to_def_id()),
            "origin": origin(tcx, tcx.hir_span(id))}),
        _ => json!({"kind": format!("{res:?}")}),
    }
}

struct Facts<'tcx> {
    tcx: TyCtxt<'tcx>,
    typeck: &'tcx ty::TypeckResults<'tcx>,
    nodes: Vec<Value>,
}

impl<'tcx> Facts<'tcx> {
    fn node(&self, id: rustc_hir::HirId, span: Span, kind: &str) -> Value {
        let tcx = self.tcx;
        let typeck = self.typeck;
        json!({"kind": kind, "owner": definition(tcx, id.owner.def_id.to_def_id()), "origin": origin(tcx, span),
            "type": typeck.node_type_opt(id).map(|ty| type_fact(tcx, ty)),
            "arguments": typeck.node_args_opt(id).map(|args| args.as_slice().iter().map(|arg| argument(tcx, *arg)).collect::<Vec<_>>()),
            "selected_definition": typeck.type_dependent_def_id(id).map(|def| definition(tcx, def))})
    }
}

impl<'tcx> Visitor<'tcx> for Facts<'tcx> {
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
            _ => "expression",
        };
        let mut fact = self.node(expr.hir_id, expr.span, kind);
        match expr.kind {
            ExprKind::Path(ref path) => {
                fact["resolution"] = resolution(self.tcx, self.typeck.qpath_res(path, expr.hir_id));
            }
            ExprKind::Struct(path, ..) => {
                fact["resolution"] = resolution(self.tcx, self.typeck.qpath_res(path, expr.hir_id));
            }
            ExprKind::Field(base, _) => {
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
                fact["binding_origin"] = origin(self.tcx, self.tcx.hir_span(id));
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

struct ItemReferences<'tcx> {
    tcx: TyCtxt<'tcx>,
    nodes: Vec<Value>,
}

impl<'tcx> Visitor<'tcx> for ItemReferences<'tcx> {
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
    args: Vec<String>,
    pending: Option<Vec<u8>>,
    error: Option<String>,
}

impl rustc_driver::Callbacks for Capture {
    fn after_analysis<'tcx>(&mut self, _compiler: &interface::Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        tcx.dcx().abort_if_errors();
        match self.collect(tcx) {
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
    fn collect(&self, tcx: TyCtxt<'_>) -> Result<Vec<u8>, String> {
        let mut nodes = Vec::new();
        let mut items = ItemReferences { tcx, nodes: Vec::new() };
        tcx.hir_visit_all_item_likes_in_crate(&mut items);
        nodes.extend(items.nodes);
        for owner in tcx.hir_body_owners() {
            let mut facts = Facts { tcx, typeck: tcx.typeck(owner), nodes: Vec::new() };
            facts.visit_body(tcx.hir_body_owned_by(owner));
            nodes.extend(facts.nodes);
        }
        let mut definitions = Vec::new();
        for id in tcx.iter_local_def_id() {
            let kind = tcx.def_kind(id);
            let ty = if matches!(kind, DefKind::Const | DefKind::Static { .. } | DefKind::AssocConst |
                DefKind::TyAlias | DefKind::Field | DefKind::Struct | DefKind::Enum | DefKind::Union) {
                let input = tcx.type_of(id).instantiate_identity();
                match tcx.try_normalize_erasing_regions(ty::TypingEnv::non_body_analysis(tcx, id), input) {
                    Ok(value) => Some(type_fact(tcx, value)),
                    Err(error) => Some(json!({"kind": "unresolved", "error": format!("{error:?}")})),
                }
            } else {
                None
            };
            definitions.push(json!({"definition": definition(tcx, id.to_def_id()), "kind": format!("{kind:?}"),
                "origin": origin(tcx, tcx.def_span(id)), "type": ty}));
        }
        let mut sources = BTreeMap::new();
        for file in tcx.sess.source_map().files().iter() {
            if let Some(text) = &file.src {
                let path = file.name.prefer_local_unconditionally().to_string();
                let raw = fs::read(&path).map_err(|e| format!("cannot fingerprint compiler source {path}: {e}"))?;
                if rustc_span::SourceFileHash::new_in_memory(file.src_hash.kind, &raw) != file.src_hash {
                    return Err(format!("compiler source changed after loading: {path}"));
                }
                sources.insert(path, json!({"sha256": digest(&raw), "normalized_sha256": digest(text.as_bytes())}));
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
            environment.insert(name.as_str().to_owned(), value.map(|v| v.as_str().to_owned()));
        }
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let helper_hash = digest(&fs::read(&executable).map_err(|e| e.to_string())?);
        let compiler_hash = digest(&fs::read(self.args.first().ok_or("missing compiler")?).map_err(|e| e.to_string())?);
        let response = json!({"complete": true, "arguments": self.args, "sources": sources,
            "cfg": tcx.sess.config.iter().map(|(name, value)| json!([name.as_str(), value.map(|v| v.as_str().to_owned())])).collect::<Vec<_>>(),
            "helper_sha256": helper_hash, "compiler_sha256": compiler_hash,
            "file_inputs": inputs, "environment_inputs": environment,
            "definitions": definitions, "nodes": nodes});
        serde_json::to_vec(&response).map_err(|e| e.to_string())
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("capture requires the compiler path and its arguments".into());
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let compiler_name = if cfg!(windows) { "rustc.exe" } else { "rustc" };
    let expected = fs::canonicalize(executable.with_file_name(compiler_name)).map_err(|e| e.to_string())?;
    let requested = fs::canonicalize(&args[0]).map_err(|e| e.to_string())?;
    if requested != expected {
        return Err(format!("compiler {} does not match helper compiler {}; build the helper for the selected compiler",
                           requested.display(), expected.display()));
    }
    let destination: PathBuf = std::env::var_os("ASK_SOURCE_CAPTURE").ok_or("missing ASK_SOURCE_CAPTURE destination")?.into();
    let mut capture = Capture { args: args.to_vec(), pending: None, error: None };
    rustc_driver::catch_fatal_errors(|| compiler_entrypoint(args, &mut capture))
        .map_err(|_| "compiler refused capture".to_owned())?;
    if let Some(error) = capture.error {
        return Err(error);
    }
    let bytes = capture.pending.ok_or("compiler invocation produced no analysis facts")?;
    let temporary = destination.with_extension("partial");
    fs::write(&temporary, bytes).map_err(|e| format!("{}: {e}", temporary.display()))?;
    fs::rename(&temporary, &destination).map_err(|e| format!("{}: {e}", destination.display()))
}
