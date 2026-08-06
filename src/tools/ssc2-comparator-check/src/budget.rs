//! Stack and block budget accounting over a comparator crate's own
//! fully-resolved static callgraph (docs/scheduling.md, "Stack budget,
//! execution-bound metric, and load-path enforcement"). Reuses `callgraph`'s
//! root enumeration and walk shape rather than re-deriving the callgraph a
//! second time.
//!
//! Stack size has no pre-codegen query anywhere in this fork (confirmed by
//! reading `rustc_monomorphize::partitioning`'s `size_estimate` — a pure
//! MIR-statement-count heuristic for codegen-unit load-balancing, unrelated
//! to runtime stack bytes) — real frame size is only fixed post-LLVM-codegen.
//! This is therefore necessarily an ESTIMATE: `tcx.layout_of` summed over
//! each function's own `body.local_decls`, ignoring register allocation,
//! spills, alignment padding, and calling-convention overhead. Good enough
//! to catch a comparator that is grossly over budget at load time; not a
//! substitute for the real post-codegen frame size docs/scheduling.md's
//! design already accepts as still open ("the exact per-comparator stack
//! budget... is a starting reference point, not a value ask has adopted").
//!
//! The block count, by contrast, is exact: MIR basic-block count is a
//! property of the function body itself, not an estimate of anything.

use std::collections::HashSet;
use std::fmt;

use rustc_middle::mir::Body;
use rustc_middle::ty::{self, Instance, TyCtxt};

/// `docs/scheduling.md`'s two fixed bring-up defaults ("Stack budget,
/// execution-bound metric, and load-path enforcement"). Canonical home:
/// `askabi::sched::{COMPARATOR_STACK_BUDGET_PAGES, COMPARATOR_BLOCK_BUDGET}`
/// (`recipes/core/services/askabi/source/src/sched.rs`) — kept as a local
/// default here instead of a Cargo dependency because `recipes/tools/rust`
/// is a separate submodule/workspace that must never depend on a crate
/// outside itself (docs/rust-toolchain.md, "ask-specific `src/tools/*`
/// additions"). `ask_cookbook sign` (outside that submodule, so free to
/// depend on `askabi` directly) is the real single source of truth at
/// runtime: it passes `--stack-budget-pages=<askabi::sched::
/// COMPARATOR_STACK_BUDGET_PAGES>`/`--block-budget=<askabi::sched::
/// COMPARATOR_BLOCK_BUDGET>` on every invocation (`Budget::parse_and_
/// strip`, `main.rs`), so these consts only serve a standalone/manual
/// invocation of this checker outside the `sign` gate — the shared wire
/// format docs/rust-toolchain.md's own comment anticipated, once
/// `ask_cookbook sign` became that real consumer on both sides of the
/// boundary.
const DEFAULT_COMPARATOR_STACK_BUDGET_PAGES: u64 = 4;
const DEFAULT_COMPARATOR_BLOCK_BUDGET: usize = 64;

/// Bring-up assumption matching `recipes/core/kernel/source`'s own build
/// target — the comparator's eventual load-path target is `x86_64-unknown-ask`,
/// whose page size is the ordinary x86_64 4 KiB page. Mirrors
/// `askabi::sched::PAGE_SIZE_BYTES`. Not `sign`-overridable like the two
/// budgets above: an x86_64 page size is a hardware fact, not policy.
const PAGE_SIZE_BYTES: u64 = 4096;

/// Resolved stack/block budget for one `check()` call — either
/// `sign`-provided (`--stack-budget-pages=N`/`--block-budget=N`, stripped
/// from the compiler's own arg list before `rustc_driver` sees them) or
/// this crate's own bring-up defaults for a standalone invocation.
#[derive(Clone, Copy)]
pub struct Budget {
    pub stack_budget_pages: u64,
    pub block_budget: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            stack_budget_pages: DEFAULT_COMPARATOR_STACK_BUDGET_PAGES,
            block_budget: DEFAULT_COMPARATOR_BLOCK_BUDGET,
        }
    }
}

impl Budget {
    /// Scans `args` for `--stack-budget-pages=N`/`--block-budget=N`,
    /// removing every matched flag in place (rustc would otherwise reject
    /// them as unknown) and returning the resolved `Budget` — defaults
    /// fill in whichever flag `sign` didn't pass, so a standalone
    /// invocation with neither flag reproduces today's fixed-default
    /// behavior exactly.
    pub fn parse_and_strip(args: &mut Vec<String>) -> Result<Self, String> {
        let mut budget = Self::default();
        let mut i = 0;
        while i < args.len() {
            if let Some(value) = args[i].strip_prefix("--stack-budget-pages=") {
                budget.stack_budget_pages = value
                    .parse()
                    .map_err(|_| format!("invalid --stack-budget-pages value: {value:?}"))?;
                args.remove(i);
                continue;
            }
            if let Some(value) = args[i].strip_prefix("--block-budget=") {
                budget.block_budget = value
                    .parse()
                    .map_err(|_| format!("invalid --block-budget value: {value:?}"))?;
                args.remove(i);
                continue;
            }
            i += 1;
        }
        Ok(budget)
    }
}

pub enum Violation {
    StackBudgetExceeded { estimated_bytes: u64, budget_bytes: u64 },
    BlockBudgetExceeded { blocks: usize, budget: usize },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::StackBudgetExceeded { estimated_bytes, budget_bytes } => write!(
                f,
                "comparator callgraph's estimated stack usage ({estimated_bytes} bytes) exceeds \
                 the {budget_bytes}-byte budget (--stack-budget-pages)"
            ),
            Violation::BlockBudgetExceeded { blocks, budget } => write!(
                f,
                "comparator callgraph has {blocks} basic blocks, exceeding the \
                 {budget}-block budget (--block-budget)"
            ),
        }
    }
}

/// Sums estimated stack bytes and exact basic-block count across every
/// `Instance` in `reachable` (the set `callgraph::reachable_instances`
/// already validated — a rejected callgraph never reaches this check, so
/// this takes the already-computed set rather than re-deriving it), and
/// rejects if either sum exceeds its fixed budget.
pub fn check<'tcx>(
    tcx: TyCtxt<'tcx>,
    reachable: &HashSet<Instance<'tcx>>,
    budget: Budget,
) -> Result<(), Violation> {
    let mut total_stack_bytes: u64 = 0;
    let mut total_blocks: usize = 0;

    for instance in reachable {
        let Some(def_id) = instance.def_id().as_local() else { continue };
        if !tcx.is_mir_available(def_id.to_def_id()) {
            continue;
        }
        let body = tcx.instance_mir(instance.def);

        total_stack_bytes = total_stack_bytes.saturating_add(estimate_stack_bytes(tcx, body));
        total_blocks = total_blocks.saturating_add(body.basic_blocks.len());
    }

    let budget_bytes = budget.stack_budget_pages.saturating_mul(PAGE_SIZE_BYTES);
    if total_stack_bytes > budget_bytes {
        return Err(Violation::StackBudgetExceeded {
            estimated_bytes: total_stack_bytes,
            budget_bytes,
        });
    }
    if total_blocks > budget.block_budget {
        return Err(Violation::BlockBudgetExceeded {
            blocks: total_blocks,
            budget: budget.block_budget,
        });
    }

    Ok(())
}

/// Sums `tcx.layout_of(local.ty).size` over every MIR local in `body` — an
/// estimate of one function's own stack frame, deliberately not a real
/// post-codegen figure (see module doc comment).
fn estimate_stack_bytes<'tcx>(tcx: TyCtxt<'tcx>, body: &Body<'tcx>) -> u64 {
    let typing_env = ty::TypingEnv::fully_monomorphized();
    body.local_decls
        .iter()
        .filter_map(|local| tcx.layout_of(typing_env.as_query_input(local.ty)).ok())
        .map(|layout| layout.size.bytes())
        .fold(0u64, |acc, size| acc.saturating_add(size))
}
