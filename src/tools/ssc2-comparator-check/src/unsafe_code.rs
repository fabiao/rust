//! Real-unsafe-code ban over the checked crate's raw AST (docs/scheduling.md,
//! "Safe-Rust-subset restriction rules") — deliberately NOT implemented via
//! rustc's built-in `unsafe_code` lint (`-D unsafe_code`): that lint fires
//! from the identical `UNSAFE_CODE` lint id for both real unsafe constructs
//! (blocks, `unsafe fn`, `unsafe trait`/`impl`, `unsafe extern`,
//! `global_asm!`) AND the Rust 2024 `#[unsafe(no_mangle)]`/
//! `#[unsafe(export_name = "...")]` attribute syntax — with no query
//! reachable from outside `rustc_lint`'s own AST pass to tell the two
//! apart (THIR's `BlockSafety` isn't an external query; MIR doesn't retain
//! per-statement unsafe-block provenance either). Every SSC2 comparator
//! MUST export its `compare` symbol via `#[unsafe(no_mangle)]` for the
//! kernel's extension loader to find it by name at all
//! (`extension::install_comparator`'s own doc comment), so a blanket
//! `-D unsafe_code` denial rejects every possible conforming comparator,
//! including the reference one already QEMU-proven
//! (`recipes/core/ssc2-comparators/round-robin-reference`).
//!
//! This module re-implements the AST-level checks `rustc_lint::builtin::
//! UnsafeCode`'s `EarlyLintPass` impl performs (unsafe blocks, `unsafe fn`,
//! `unsafe trait`, `unsafe impl`, `unsafe extern` blocks, `global_asm!`),
//! run directly over the parsed-but-not-yet-expanded AST via
//! `Callbacks::after_crate_root_parsing` — the same point rustc's own
//! `EarlyLintPass` runs at, before macro expansion. It intentionally does
//! NOT check attribute safety at all: the only unsafe attribute a
//! `#![no_std]` freestanding comparator crate could plausibly carry is
//! `#[unsafe(no_mangle)]` on its exported entry point, and permitting it
//! doesn't weaken the safety guarantee this checker exists for — it's a
//! linker-visibility directive, not a memory-safety escape hatch, and
//! everything inside the function body it decorates is still fully
//! covered by this same walk plus the recursion/indirect-call/budget
//! checks in `callgraph`/`budget`.

use rustc_ast::visit::{self, FnKind, Visitor};
use rustc_ast::{Expr, ExprKind, Fn, FnHeader, Item, ItemKind, Safety};
use rustc_span::Span;

/// One real-unsafe-code violation found in the checked crate's AST.
pub struct Violation {
    pub span: Span,
    pub what: &'static str,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "comparator crates may not use {}", self.what)
    }
}

struct UnsafeCodeVisitor {
    violation: Option<Violation>,
}

impl<'ast> Visitor<'ast> for UnsafeCodeVisitor {
    fn visit_expr(&mut self, e: &'ast Expr) {
        if self.violation.is_some() {
            return;
        }
        if let ExprKind::Block(blk, _) = &e.kind {
            if matches!(blk.rules, rustc_ast::BlockCheckMode::Unsafe(rustc_ast::UnsafeSource::UserProvided))
            {
                self.violation = Some(Violation { span: blk.span, what: "an `unsafe` block" });
                return;
            }
        }
        visit::walk_expr(self, e);
    }

    fn visit_item(&mut self, item: &'ast Item) {
        if self.violation.is_some() {
            return;
        }
        match &item.kind {
            ItemKind::Trait(t) if matches!(t.safety, Safety::Unsafe(_)) => {
                self.violation = Some(Violation { span: item.span, what: "an `unsafe trait`" });
                return;
            }
            ItemKind::Impl(i) => {
                if let Some(of_trait) = &i.of_trait {
                    if matches!(of_trait.safety, Safety::Unsafe(_)) {
                        self.violation =
                            Some(Violation { span: item.span, what: "an `unsafe impl`" });
                        return;
                    }
                }
            }
            ItemKind::GlobalAsm(..) => {
                self.violation = Some(Violation { span: item.span, what: "`global_asm!`" });
                return;
            }
            ItemKind::ForeignMod(m) if matches!(m.safety, Safety::Unsafe(_)) => {
                self.violation =
                    Some(Violation { span: item.span, what: "an `unsafe extern` block" });
                return;
            }
            _ => {}
        }
        visit::walk_item(self, item);
    }

    fn visit_fn(
        &mut self,
        fk: FnKind<'ast>,
        _attrs: &rustc_ast::AttrVec,
        span: Span,
        _id: rustc_ast::NodeId,
    ) {
        if self.violation.is_some() {
            return;
        }
        if let FnKind::Fn(_, _, Fn { sig, .. }) = fk {
            if matches!(sig.header, FnHeader { safety: Safety::Unsafe(_), .. }) {
                self.violation = Some(Violation { span, what: "an `unsafe fn`" });
                return;
            }
        }
        visit::walk_fn(self, fk);
    }
}

/// Walks `krate`'s AST (called from `Callbacks::after_crate_root_parsing`,
/// the same pre-expansion point `rustc_lint`'s own `UnsafeCode` pass runs
/// at) and returns the first real-unsafe-code construct found, if any —
/// `#[unsafe(no_mangle)]` and any other unsafe-attribute usage is
/// deliberately not checked here (this module's own doc comment).
pub fn check(krate: &rustc_ast::Crate) -> Option<Violation> {
    let mut visitor = UnsafeCodeVisitor { violation: None };
    visitor.visit_crate(krate);
    visitor.violation
}
