use std::io::{self, Read};

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use serde_json::{Value, json};
use crate::digest;
use syn::spanned::Spanned;
use syn::ext::IdentExt;
use syn::visit::{self, Visit};

fn range(span: Span) -> Value {
    let range = span.byte_range();
    json!([range.start, range.end])
}

fn tokens(value: &impl ToTokens) -> String {
    value.to_token_stream().to_string()
}

fn attributes(attrs: &[syn::Attribute]) -> Value {
    Value::Array(attrs.iter().map(|attr| json!({
        "span": range(attr.span()), "meta": meta(&attr.meta),
    })).collect())
}

fn meta(value: &syn::Meta) -> Value {
    match value {
        syn::Meta::Path(path) => json!({"kind": "path", "path": tokens(path)}),
        syn::Meta::NameValue(value) => {
            let string = match &value.value {
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(value), .. }) => Some(value.value()),
                _ => None,
            };
            json!({"kind": "value", "path": tokens(&value.path), "value": tokens(&value.value), "string": string})
        }
        syn::Meta::List(list) => {
            use syn::parse::Parser;
            let parser = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated;
            let children = parser.parse2(list.tokens.clone()).ok().map(|items| {
                items.iter().map(meta).collect::<Vec<_>>()
            });
            json!({"kind": "list", "path": tokens(&list.path),
                   "children": children, "tokens": list.tokens.to_string()})
        }
    }
}

fn use_tree(tree: &syn::UseTree) -> Value {
    match tree {
        syn::UseTree::Path(node) => json!({"kind": "path", "name": node.ident.unraw().to_string(),
                                         "tree": use_tree(&node.tree)}),
        syn::UseTree::Name(node) => json!({"kind": "name", "name": node.ident.unraw().to_string()}),
        syn::UseTree::Rename(node) => json!({"kind": "rename", "name": node.ident.unraw().to_string(),
                                           "alias": node.rename.unraw().to_string()}),
        syn::UseTree::Glob(_) => json!({"kind": "glob"}),
        syn::UseTree::Group(node) => json!({"kind": "group", "items":
            node.items.iter().map(use_tree).collect::<Vec<_>>()}),
    }
}

struct Model {
    nodes: Vec<Value>,
    parents: Vec<usize>,
}

impl Model {
    fn enter(&mut self, kind: &str, span: Span, mut data: Value) {
        data["kind"] = json!(kind);
        data["span"] = range(span);
        data["parent"] = json!(self.parents.last());
        let id = self.nodes.len();
        self.nodes.push(data);
        self.parents.push(id);
    }

    fn leave(&mut self) {
        self.parents.pop();
    }
}

macro_rules! node_visit {
    ($method:ident, $ty:ident, $kind:literal) => {
        fn $method(&mut self, node: &'ast syn::$ty) {
            self.enter($kind, node.span(), json!({}));
            visit::$method(self, node);
            self.leave();
        }
    };
}

impl<'ast> Visit<'ast> for Model {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let (kind, data) = match item {
            syn::Item::Const(n) => ("constant", json!({"name": n.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "type": range(n.ty.span()), "value": range(n.expr.span())})),
            syn::Item::Static(n) => ("static", json!({"name": n.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "type": range(n.ty.span()), "value": range(n.expr.span())})),
            syn::Item::Mod(n) => ("module", json!({"name": n.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "inline": n.content.is_some(),
                "body": n.content.as_ref().map(|(brace, _)| range(brace.span.join()))})),
            syn::Item::Use(n) => ("use", json!({"attrs": attributes(&n.attrs),
                "absolute": n.leading_colon.is_some(), "tree": use_tree(&n.tree)})),
            syn::Item::Struct(n) => ("struct", json!({"name": n.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "fields": range(n.fields.span()),
                "tuple": matches!(n.fields, syn::Fields::Unnamed(_))})),
            syn::Item::Enum(n) => ("enum", json!({"name": n.ident.unraw().to_string(), "attrs": attributes(&n.attrs),
                "body": range(n.brace_token.span.join())})),
            syn::Item::Trait(n) => ("trait", json!({"name": n.ident.unraw().to_string(), "attrs": attributes(&n.attrs),
                "body": range(n.brace_token.span.join())})),
            syn::Item::Type(n) => ("alias", json!({"name": n.ident.unraw().to_string(), "attrs": attributes(&n.attrs),
                "type": range(n.ty.span())})),
            syn::Item::Fn(n) => ("function", json!({"name": n.sig.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "header": range(n.sig.span()), "body": range(n.block.span())})),
            syn::Item::Impl(n) => ("impl", json!({"name": match n.self_ty.as_ref() {
                syn::Type::Path(path) if path.qself.is_none() => path.path.segments.last().map(|segment| segment.ident.unraw().to_string()),
                _ => None,
            }, "self_type": range(n.self_ty.span()),
                "attrs": attributes(&n.attrs), "trait": n.trait_.as_ref().map(|(path, _)| tokens(path)),
                "body": range(n.brace_token.span.join())})),
            syn::Item::Macro(n) => ("item_macro", json!({"name": n.ident.as_ref().map(|name| name.unraw().to_string()),
                "attrs": attributes(&n.attrs)})),
            syn::Item::ExternCrate(n) => ("extern_crate", json!({"name": n.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "alias": n.rename.as_ref().map(|(_, name)| name.unraw().to_string())})),
            syn::Item::ForeignMod(n) => ("foreign", json!({"attrs": attributes(&n.attrs)})),
            syn::Item::TraitAlias(n) => ("trait_alias", json!({"name": n.ident.unraw().to_string(), "attrs": attributes(&n.attrs)})),
            syn::Item::Union(n) => ("union", json!({"name": n.ident.unraw().to_string(), "attrs": attributes(&n.attrs)})),
            syn::Item::Verbatim(n) => ("opaque_item", json!({"tokens": n.to_string()})),
            _ => ("unsupported_item", json!({})),
        };
        self.enter(kind, item.span(), data);
        visit::visit_item(self, item);
        self.leave();
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let (kind, data) = match item {
            syn::ImplItem::Const(n) => ("constant", json!({"name": n.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "type": range(n.ty.span()), "value": range(n.expr.span())})),
            syn::ImplItem::Type(n) => ("alias", json!({"name": n.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "type": range(n.ty.span())})),
            syn::ImplItem::Fn(n) => ("function", json!({"name": n.sig.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "header": range(n.sig.span()), "body": range(n.block.span())})),
            syn::ImplItem::Macro(n) => ("item_macro", json!({"attrs": attributes(&n.attrs)})),
            _ => ("opaque_item", json!({})),
        };
        self.enter(kind, item.span(), data);
        visit::visit_impl_item(self, item);
        self.leave();
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let (kind, data) = match item {
            syn::TraitItem::Const(n) => ("constant", json!({"name": n.ident.unraw().to_string(),
                "attrs": attributes(&n.attrs), "type": range(n.ty.span()),
                "value": n.default.as_ref().map(|(_, expr)| range(expr.span()))})),
            syn::TraitItem::Type(n) => ("alias", json!({"name": n.ident.unraw().to_string(), "attrs": attributes(&n.attrs),
                "type": n.default.as_ref().map(|(_, ty)| range(ty.span()))})),
            syn::TraitItem::Fn(n) => ("function", json!({"name": n.sig.ident.unraw().to_string(), "attrs": attributes(&n.attrs),
                "header": range(n.sig.span()), "body": n.default.as_ref().map(|body| range(body.span()))})),
            syn::TraitItem::Macro(n) => ("item_macro", json!({"attrs": attributes(&n.attrs)})),
            _ => ("opaque_item", json!({})),
        };
        self.enter(kind, item.span(), data);
        visit::visit_trait_item(self, item);
        self.leave();
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        self.enter("local", node.span(), json!({"attrs": attributes(&node.attrs),
            "pattern": range(node.pat.span()), "value": node.init.as_ref().map(|n| range(n.expr.span())),
            "else": node.init.as_ref().and_then(|n| n.diverge.as_ref().map(|(_, expr)| range(expr.span())))}));
        visit::visit_local(self, node);
        self.leave();
    }

    fn visit_field(&mut self, node: &'ast syn::Field) {
        self.enter("field", node.span(), json!({"name": node.ident.as_ref().map(|name| name.unraw().to_string()),
            "attrs": attributes(&node.attrs), "type": range(node.ty.span())}));
        visit::visit_field(self, node);
        self.leave();
    }

    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        self.enter("variant", node.span(), json!({"name": node.ident.unraw().to_string(), "attrs": attributes(&node.attrs)}));
        visit::visit_variant(self, node);
        self.leave();
    }

    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        let (kind, data) = match node {
            syn::Expr::Array(n) => ("array_value", json!({"elements": n.elems.len(), "attrs": attributes(&n.attrs)})),
            syn::Expr::Repeat(n) => ("repeat", json!({"extent": range(n.len.span()), "value": range(n.expr.span()), "attrs": attributes(&n.attrs)})),
            syn::Expr::Path(n) => ("value_path", json!({"path": tokens(&n.path), "qualified": n.qself.is_some(), "attrs": attributes(&n.attrs)})),
            syn::Expr::Call(n) => ("call", json!({"callee": range(n.func.span()), "attrs": attributes(&n.attrs)})),
            syn::Expr::MethodCall(n) => ("method", json!({"name": n.method.unraw().to_string(), "receiver": range(n.receiver.span()), "attrs": attributes(&n.attrs)})),
            syn::Expr::Field(n) => ("field_access", json!({"name": tokens(&n.member), "receiver": range(n.base.span()), "attrs": attributes(&n.attrs)})),
            syn::Expr::Binary(n) => ("binary", json!({"operator": tokens(&n.op), "left": range(n.left.span()), "right": range(n.right.span()), "attrs": attributes(&n.attrs)})),
            syn::Expr::Lit(n) => ("literal", json!({"value": tokens(&n.lit), "integer": matches!(n.lit, syn::Lit::Int(_)), "attrs": attributes(&n.attrs)})),
            syn::Expr::Struct(n) => ("constructor", json!({"path": tokens(&n.path), "qualified": n.qself.is_some(), "attrs": attributes(&n.attrs)})),
            syn::Expr::Macro(n) => ("expr_macro", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Cast(n) => ("cast", json!({"value": range(n.expr.span()), "type": range(n.ty.span()), "attrs": attributes(&n.attrs)})),
            syn::Expr::Reference(n) => ("reference", json!({"value": range(n.expr.span()), "attrs": attributes(&n.attrs)})),
            syn::Expr::Assign(n) => ("assign", json!({"left": range(n.left.span()), "right": range(n.right.span()), "attrs": attributes(&n.attrs)})),
            syn::Expr::Async(n) => ("async", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Await(n) => ("await", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Block(n) => ("block_expression", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Break(n) => ("break", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Closure(n) => ("closure", json!({"attrs": attributes(&n.attrs), "body": range(n.body.span())})),
            syn::Expr::Const(n) => ("const_block", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Continue(n) => ("continue", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::ForLoop(n) => ("for", json!({"attrs": attributes(&n.attrs), "pattern": range(n.pat.span()), "iterable": range(n.expr.span()), "body": range(n.body.span())})),
            syn::Expr::Group(n) => ("group", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::If(n) => ("if", json!({"attrs": attributes(&n.attrs), "condition": range(n.cond.span()), "then": range(n.then_branch.span()), "else": n.else_branch.as_ref().map(|(_, expr)| range(expr.span()))})),
            syn::Expr::Index(n) => ("index", json!({"attrs": attributes(&n.attrs), "receiver": range(n.expr.span()), "index": range(n.index.span())})),
            syn::Expr::Infer(n) => ("infer", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Let(n) => ("let_expression", json!({"attrs": attributes(&n.attrs), "pattern": range(n.pat.span()), "value": range(n.expr.span())})),
            syn::Expr::Loop(n) => ("loop", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Match(n) => ("match", json!({"attrs": attributes(&n.attrs), "value": range(n.expr.span())})),
            syn::Expr::Paren(n) => ("parentheses", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Range(n) => ("range", json!({"attrs": attributes(&n.attrs), "start": n.start.as_ref().map(|expr| range(expr.span())), "end": n.end.as_ref().map(|expr| range(expr.span()))})),
            syn::Expr::RawAddr(n) => ("raw_reference", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Return(n) => ("return", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Try(n) => ("try", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::TryBlock(n) => ("try_block", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Tuple(n) => ("tuple", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Unary(n) => ("unary", json!({"attrs": attributes(&n.attrs), "operator": tokens(&n.op)})),
            syn::Expr::Unsafe(n) => ("unsafe", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::While(n) => ("while", json!({"attrs": attributes(&n.attrs), "condition": range(n.cond.span()), "body": range(n.body.span())})),
            syn::Expr::Yield(n) => ("yield", json!({"attrs": attributes(&n.attrs)})),
            syn::Expr::Verbatim(n) => ("opaque_expr", json!({"tokens": n.to_string()})),
            _ => ("unsupported_expr", json!({})),
        };
        self.enter(kind, node.span(), data);
        visit::visit_expr(self, node);
        self.leave();
    }

    fn visit_type(&mut self, node: &'ast syn::Type) {
        let (kind, mut data) = match node {
            syn::Type::Array(n) => ("array_type", json!({"extent": range(n.len.span()), "element": range(n.elem.span())})),
            syn::Type::Path(n) => ("type_path", json!({"path": tokens(&n.path), "qualified": n.qself.is_some()})),
            syn::Type::Reference(n) => ("type_reference", json!({"type": range(n.elem.span())})),
            syn::Type::FnPtr(_) => ("function_type", json!({})),
            syn::Type::Group(_) => ("type_group", json!({})),
            syn::Type::ImplTrait(_) => ("opaque_trait_type", json!({})),
            syn::Type::Infer(_) => ("infer_type", json!({})),
            syn::Type::Never(_) => ("never_type", json!({})),
            syn::Type::Paren(_) => ("type_parentheses", json!({})),
            syn::Type::Ptr(n) => ("pointer_type", json!({"type": range(n.elem.span())})),
            syn::Type::Slice(n) => ("slice_type", json!({"element": range(n.elem.span())})),
            syn::Type::TraitObject(_) => ("trait_object_type", json!({})),
            syn::Type::Tuple(_) => ("tuple_type", json!({})),
            syn::Type::Macro(_) => ("type_macro", json!({})),
            syn::Type::Verbatim(n) => ("opaque_type", json!({"tokens": n.to_string()})),
            _ => ("unsupported_type", json!({})),
        };
        data["attrs"] = attributes(match node {
            syn::Type::Array(node) => &node.attrs,
            syn::Type::Path(node) => &node.attrs,
            syn::Type::Reference(node) => &node.attrs,
            syn::Type::FnPtr(node) => &node.attrs,
            syn::Type::Group(node) => &node.attrs,
            syn::Type::ImplTrait(node) => &node.attrs,
            syn::Type::Infer(node) => &node.attrs,
            syn::Type::Never(node) => &node.attrs,
            syn::Type::Paren(node) => &node.attrs,
            syn::Type::Ptr(node) => &node.attrs,
            syn::Type::Slice(node) => &node.attrs,
            syn::Type::TraitObject(node) => &node.attrs,
            syn::Type::Tuple(node) => &node.attrs,
            syn::Type::Macro(node) => &node.attrs,
            _ => &[],
        });
        self.enter(kind, node.span(), data);
        visit::visit_type(self, node);
        self.leave();
    }

    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        let (kind, data) = match node {
            syn::Pat::Ident(n) => ("pattern_ident", json!({"name": n.ident.unraw().to_string(), "by_ref": n.by_ref.is_some(), "attrs": attributes(&n.attrs)})),
            syn::Pat::Type(n) => ("pattern_type", json!({"type": range(n.ty.span()), "pattern": range(n.pat.span()), "attrs": attributes(&n.attrs)})),
            syn::Pat::Slice(n) => ("pattern_slice", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Tuple(n) => ("pattern_tuple", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Reference(n) => ("pattern_reference", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Struct(n) => ("pattern_struct", json!({"path": tokens(&n.path), "attrs": attributes(&n.attrs)})),
            syn::Pat::TupleStruct(n) => ("pattern_constructor", json!({"path": tokens(&n.path), "attrs": attributes(&n.attrs)})),
            syn::Pat::Const(n) => ("pattern_const", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Guard(n) => ("pattern_guard", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Lit(n) => ("pattern_literal", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Macro(n) => ("pattern_macro", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Or(n) => ("pattern_or", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Paren(n) => ("pattern_parentheses", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Path(n) => ("pattern_path", json!({"attrs": attributes(&n.attrs), "path": tokens(&n.path), "qualified": n.qself.is_some()})),
            syn::Pat::Range(n) => ("pattern_range", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Rest(n) => ("pattern_rest", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Wild(n) => ("pattern_wild", json!({"attrs": attributes(&n.attrs)})),
            syn::Pat::Verbatim(n) => ("opaque_pattern", json!({"tokens": n.to_string()})),
            _ => ("unsupported_pattern", json!({})),
        };
        self.enter(kind, node.span(), data);
        visit::visit_pat(self, node);
        self.leave();
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.enter("macro", node.span(), json!({"path": tokens(&node.path), "tokens": node.tokens.to_string()}));
        self.leave();
    }

    fn visit_const_param(&mut self, node: &'ast syn::ConstParam) {
        self.enter("const_parameter", node.span(), json!({"name": node.ident.unraw().to_string(), "attrs": attributes(&node.attrs),
            "default": node.default.as_ref().map(|(_, expr)| range(expr.span()))}));
        visit::visit_const_param(self, node);
        self.leave();
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        self.enter("path", node.span(), json!({"absolute": node.leading_colon.is_some(),
            "segments": node.segments.iter().map(|segment| json!({
                "name": segment.ident.unraw().to_string(), "span": range(segment.span()),
                "arguments": match segment.arguments {
                    syn::PathArguments::None => "none",
                    syn::PathArguments::AngleBracketed(_) => "angle",
                    syn::PathArguments::Parenthesized(_) => "parenthesized",
                },
            })).collect::<Vec<_>>()}));
        visit::visit_path(self, node);
        self.leave();
    }

    fn visit_type_param(&mut self, node: &'ast syn::TypeParam) {
        self.enter("type_parameter", node.span(), json!({"name": node.ident.unraw().to_string(), "attrs": attributes(&node.attrs)}));
        visit::visit_type_param(self, node);
        self.leave();
    }

    fn visit_generic_argument(&mut self, node: &'ast syn::GenericArgument) {
        let kind = match node {
            syn::GenericArgument::Const(_) => "const_argument",
            syn::GenericArgument::Type(_) => "type_argument",
            syn::GenericArgument::AssocConst(_) => "associated_const_argument",
            syn::GenericArgument::AssocType(_) => "associated_type_argument",
            syn::GenericArgument::Constraint(_) => "constraint_argument",
            syn::GenericArgument::Lifetime(_) => "lifetime_argument",
            _ => "unsupported_generic_argument",
        };
        self.enter(kind, node.span(), json!({}));
        visit::visit_generic_argument(self, node);
        self.leave();
    }

    node_visit!(visit_block, Block, "block");
    node_visit!(visit_arm, Arm, "arm");
    node_visit!(visit_signature, Signature, "signature");
    node_visit!(visit_generics, Generics, "generics");
    node_visit!(visit_field_value, FieldValue, "field_value");
}

fn token_ranges(stream: TokenStream, output: &mut Vec<Value>) {
    for token in stream {
        match token {
            TokenTree::Group(group) => {
                output.push(json!({"span": range(group.span_open()), "literal": false}));
                token_ranges(group.stream(), output);
                output.push(json!({"span": range(group.span_close()), "literal": false}));
            }
            TokenTree::Literal(literal) => {
                let is_text = matches!(syn::parse_str::<syn::Lit>(&literal.to_string()),
                    Ok(syn::Lit::Str(_) | syn::Lit::ByteStr(_) | syn::Lit::CStr(_) | syn::Lit::Char(_) | syn::Lit::Byte(_)));
                output.push(json!({"span": range(literal.span()), "literal": is_text}));
            }
            token => output.push(json!({"span": range(token.span()), "literal": false})),
        }
    }
}

pub fn run() -> Result<(), String> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(|e| e.to_string())?;
    let request: Value = serde_json::from_str(&input).map_err(|e| e.to_string())?;
    let sources = request.get("sources").and_then(Value::as_array).ok_or("missing sources array")?;
    let mut output = Vec::new();
    for source in sources {
        let path = source.get("path").and_then(Value::as_str).ok_or("missing source path")?;
        let text = source.get("text").and_then(Value::as_str).ok_or("missing source text")?;
        let mut model = Model { nodes: Vec::new(), parents: Vec::new() };
        let mut token_spans = Vec::new();
        match source.get("context").and_then(Value::as_str).unwrap_or("file") {
            "file" => {
                let file = syn::parse_file(text).map_err(|e| format!("{path}: {}: {e}", e.span().start().line))?;
                model.enter("file", file.span(), json!({"attrs": attributes(&file.attrs)}));
                model.visit_file(&file);
                model.leave();
                token_ranges(file.into_token_stream(), &mut token_spans);
            }
            "expression" => {
                let expr = syn::parse_str::<syn::Expr>(text).map_err(|e| format!("{path}: {e}"))?;
                model.visit_expr(&expr);
                token_ranges(expr.into_token_stream(), &mut token_spans);
            }
            "meta" => {
                let predicate = syn::parse_str::<syn::Meta>(text).map_err(|e| format!("{path}: {e}"))?;
                model.enter("meta", predicate.span(), json!({"meta": meta(&predicate)}));
                model.leave();
                token_ranges(predicate.into_token_stream(), &mut token_spans);
            }
            other => return Err(format!("{path}: unsupported context {other}")),
        }
        output.push(json!({"path": path, "sha256": digest(text.as_bytes()),
                          "nodes": model.nodes, "tokens": token_spans}));
    }
    let response = json!({"complete": true, "sources": output});
    serde_json::to_writer(io::stdout().lock(), &response).map_err(|e| e.to_string())
}
