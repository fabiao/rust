//! Standalone rustc-driver tool enforcing the compiler-flag-level half of
//! SSC2's comparator safe-Rust-subset restrictions (docs/scheduling.md,
//! "Safe-Rust-subset restriction rules"). Modeled on miri's own
//! `rustc_driver::Callbacks` driver shape (`src/tools/miri/src/bin/miri.rs`),
//! but much thinner: this tool never executes the comparator, it only
//! rejects a compilation before codegen.
//!
//! Scope of this binary: reject `unsafe` code, unstable (`-Z`) features, and
//! floating-point/SIMD target features. It does NOT yet check recursion,
//! indirect calls, or the stack/block budgets (`COMPARATOR_STACK_BUDGET_PAGES`,
//! `COMPARATOR_BLOCK_BUDGET`) — those need the monomorphization callgraph
//! (`rustc_monomorphize::collector`) and are deferred to a follow-up pass
//! that will extend `Callbacks::after_analysis` below, marked at its
//! attachment point.

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;

use std::env;
use std::process::ExitCode;

use rustc_driver::Compilation;
use rustc_interface::interface;
use rustc_middle::ty::TyCtxt;

/// `rustc_driver::Callbacks` impl rejecting a comparator crate that uses
/// `unsafe` or floating-point/SIMD target features — the mechanical part
/// of the Rex-style blocklist (docs/scheduling.md). Unstable (`-Z`) feature
/// and forbidden target-feature rejection happen earlier, against the raw
/// CLI args (`reject_unstable_flags`, `reject_forbidden_target_features`)
/// — `Session::opts.unstable_features` cannot be used for this: it reports
/// `Allow` for any nightly compiler regardless of `-Z` flags actually
/// passed, including this checker's own nightly stage1 host compiler, so
/// it can't distinguish a comparator's `-Z` usage from the toolchain's own
/// channel.
struct Ssc2ComparatorCalls;

impl rustc_driver::Callbacks for Ssc2ComparatorCalls {
    fn config(&mut self, config: &mut interface::Config) {
        // `-D unsafe_code` is a built-in rustc lint, not a clippy lint —
        // forbidding it here at the config level makes it non-overridable
        // by any `#![allow(unsafe_code)]` in the checked crate itself.
        config.opts.lint_opts.push(("unsafe_code".to_owned(), rustc_session::lint::Level::Deny));
    }

    // Attachment point for the next implementation slice: recursion ban,
    // indirect-call ban, and stack/block budget accounting all need the
    // fully-resolved monomorphized callgraph, available here via
    // `rustc_monomorphize::collector::collect_crate_mono_items(tcx, ...)`
    // (compiler/rustc_monomorphize/src/collector.rs). Intentionally left
    // unimplemented this slice — see docs/scheduling.md's Open Decisions.
    fn after_analysis<'tcx>(
        &mut self,
        _compiler: &interface::Compiler,
        tcx: TyCtxt<'tcx>,
    ) -> Compilation {
        tcx.dcx().abort_if_errors();
        Compilation::Stop
    }
}

fn fatal_error(msg: &str) -> ! {
    eprintln!("ssc2-comparator-check: rejected: {msg}");
    std::process::exit(1)
}

/// Target features this slice forbids on the comparator's target spec —
/// floating point and SIMD, per Rex's own blocklist (docs/scheduling.md,
/// "Safe-Rust-subset restriction rules").
const FORBIDDEN_TARGET_FEATURES: &[&str] = &["sse", "sse2", "avx", "avx2", "fma"];

fn reject_forbidden_target_features(args: &[String]) {
    for arg in args {
        let Some(features) = arg.strip_prefix("-Ctarget-feature=") else { continue };
        for feature in features.split(',') {
            let enabled = feature.strip_prefix('+').unwrap_or(feature);
            if FORBIDDEN_TARGET_FEATURES.contains(&enabled) && feature.starts_with('+') {
                fatal_error(&format!(
                    "comparator crates may not enable target feature {enabled:?} (floating-point/SIMD forbidden)"
                ));
            }
        }
    }
}

/// Rejects any `-Z` flag in the args passed to the comparator's own
/// compilation — unlike `Session::opts.unstable_features` (see
/// `Ssc2ComparatorCalls`'s doc comment), the raw CLI args do distinguish
/// "the comparator crate asked for an unstable feature" from "this
/// checker's own host compiler happens to be nightly."
fn reject_unstable_flags(args: &[String]) {
    if let Some(flag) = args.iter().find(|arg| arg.starts_with("-Z")) {
        fatal_error(&format!("comparator crates may not pass unstable flag {flag:?}"));
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: ssc2-comparator-check <path-to-comparator-crate-main.rs> [rustc args...]");
        return ExitCode::FAILURE;
    }

    reject_forbidden_target_features(&args);
    reject_unstable_flags(&args);

    // `rustc_driver::run_compiler` itself drops `args[0]` (the program
    // name) internally, so the compiler's own args start at `args[1]` —
    // pass the full `args`, not `&args[1..]`.
    let mut callbacks = Ssc2ComparatorCalls;
    match rustc_driver::catch_fatal_errors(|| rustc_driver::run_compiler(&args, &mut callbacks)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
