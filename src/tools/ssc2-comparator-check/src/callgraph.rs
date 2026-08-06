//! Recursion and indirect-call bans over a comparator crate's own
//! fully-resolved static callgraph (docs/scheduling.md, "Safe-Rust-subset
//! restriction rules" and "Static stack-usage analysis").
//!
//! `rustc_monomorphize::collector`'s real callgraph walk is `pub(crate)`-only
//! (confirmed by reading `compiler/rustc_monomorphize/src/collector.rs` —
//! `collect_crate_mono_items`/`collect_items_rec` are not reachable from an
//! external crate), so this walks MIR call terminators directly instead of
//! depending on that crate at all: `tcx.hir_crate_items(())` enumerates the
//! checked crate's own `fn` items as roots, then a manual worklist visits
//! each `Call`/`TailCall` terminator's callee (`compiler/rustc_monomorphize/
//! src/collector.rs`'s `visit_fn_use`, lines ~947-977, is the model for
//! resolving a callee type to a concrete `Instance` via
//! `ty::Instance::expect_resolve`).
//!
//! Recursion detection uses a simple "currently on the DFS path" set rather
//! than adapting `rustc_data_structures::graph::iterate::TriColorDepthFirstSearch`
//! to a custom `Instance`-keyed `DirectedGraph` impl — confirmed that trait
//! generalizes cleanly to any graph (it is not `mir::BasicBlocks`-specific),
//! so that remains a valid refactor if a future session needs the richer
//! tree/forward/back-edge classification `TriColorVisitor` provides; a plain
//! path-set is sufficient to just detect "does a cycle exist."

use std::collections::HashSet;
use std::fmt;

use rustc_hir::def::DefKind;
use rustc_middle::mir::TerminatorKind;
use rustc_middle::ty::{self, Instance, TyCtxt};

/// One comparator-callgraph rule violation, reported by `check`.
pub enum Violation {
    Recursion { function: String },
    IndirectCall { function: String },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::Recursion { function } => {
                write!(f, "comparator callgraph contains recursion, reached via {function:?}")
            }
            Violation::IndirectCall { function } => {
                write!(
                    f,
                    "comparator callgraph contains an indirect call (dyn Trait or fn pointer) in {function:?}"
                )
            }
        }
    }
}

/// Walks every `fn`/associated-fn item in the checked crate as a callgraph
/// root, rejecting recursion (direct or indirect/mutual) and indirect calls
/// (through a `dyn Trait` reference or captured closure/function pointer) —
/// docs/scheduling.md's "Safe-Rust-subset restriction rules": both are
/// banned outright rather than runtime-guarded, since ask's kernel has no
/// unwinding/panic-recovery mechanism to build a Rex-style runtime backstop
/// with (see that section's "Where ask must diverge from Rex, and why").
pub fn check(tcx: TyCtxt<'_>) -> Result<(), Violation> {
    reachable_instances(tcx, ty::TypingEnv::fully_monomorphized()).map(|_| ())
}

/// Same walk as `check`, but returns the full set of reachable `Instance`s
/// instead of discarding it — `budget` reuses this rather than walking the
/// callgraph a second time to sum stack/block budgets over the same set
/// `check` already validated.
pub fn reachable_instances<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
) -> Result<HashSet<Instance<'tcx>>, Violation> {
    let mut reachable = HashSet::new();

    for id in tcx.hir_crate_items(()).free_items() {
        if !matches!(tcx.def_kind(id.owner_id), DefKind::Fn | DefKind::AssocFn) {
            continue;
        }
        let def_id = id.owner_id.to_def_id();
        if !tcx.generics_of(def_id).own_requires_monomorphization() {
            let args = ty::GenericArgs::identity_for_item(tcx, def_id);
            let instance = Instance::new_raw(def_id, args);
            let mut on_path = HashSet::new();
            walk(tcx, typing_env, instance, &mut on_path, &mut reachable)?;
        }
    }

    Ok(reachable)
}

fn walk<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    instance: Instance<'tcx>,
    on_path: &mut HashSet<Instance<'tcx>>,
    reachable: &mut HashSet<Instance<'tcx>>,
) -> Result<(), Violation> {
    let function = tcx.def_path_str(instance.def_id());

    if !on_path.insert(instance) {
        return Err(Violation::Recursion { function });
    }
    reachable.insert(instance);

    if instance.def_id().as_local().is_none() || !tcx.is_mir_available(instance.def_id()) {
        on_path.remove(&instance);
        return Ok(());
    }

    let body = tcx.instance_mir(instance.def);
    for block in body.basic_blocks.iter() {
        let func = match &block.terminator().kind {
            TerminatorKind::Call { func, .. } | TerminatorKind::TailCall { func, .. } => func,
            _ => continue,
        };

        let callee_ty = func.ty(&body.local_decls, tcx);
        let callee_ty = instance.instantiate_mir_and_normalize_erasing_regions(
            tcx,
            typing_env,
            ty::EarlyBinder::bind(tcx, callee_ty),
        );

        let ty::FnDef(callee_def_id, callee_args) = *callee_ty.kind() else {
            // Not a statically-known function item — a `dyn Trait` vtable
            // call or a captured closure/fn-pointer call, both banned
            // (docs/scheduling.md: "a comparator body may not call through
            // a `dyn Trait` reference or a captured closure/function
            // pointer — only statically resolved... calls").
            on_path.remove(&instance);
            return Err(Violation::IndirectCall { function });
        };
        let Some(callee_args) = callee_args.no_bound_vars() else {
            on_path.remove(&instance);
            return Err(Violation::IndirectCall { function });
        };

        let Ok(Some(callee_instance)) =
            Instance::try_resolve(tcx, typing_env, callee_def_id, callee_args)
        else {
            // Same "not a single statically-known callee" case as above,
            // reached through a `FnDef` type that nonetheless fails to
            // resolve to one concrete `Instance` (e.g. an unresolved trait
            // method with no `impl` selected yet).
            on_path.remove(&instance);
            return Err(Violation::IndirectCall { function });
        };

        if matches!(callee_instance.def, ty::InstanceKind::Virtual(..)) {
            // `<dyn Trait as Trait>::method` resolves to a real `Instance`,
            // but `InstanceKind::Virtual` means "call through the vtable at
            // this index" — there is no single statically-known callee
            // body, so this is exactly the indirect-call case despite
            // resolving past the `ty::FnDef` check above.
            on_path.remove(&instance);
            return Err(Violation::IndirectCall { function });
        }

        walk(tcx, typing_env, callee_instance, on_path, reachable)?;
    }

    on_path.remove(&instance);
    Ok(())
}
