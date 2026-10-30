#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_feature;
extern crate rustc_ast;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;
extern crate rustc_session;
extern crate rustc_codegen_ssa;
extern crate rustc_structures;

mod syntax;
mod semantic;
mod entry_inputs;
mod records;
#[allow(dead_code, reason = "resource owner is shared with Askchef")]
mod resource_budget;
mod launcher;
mod cargo_build;
#[path = "bootstrap_config.rs"]
#[allow(dead_code, reason = "shared SDK capture configuration")]
mod config;
#[allow(dead_code, reason = "shared by the Python frontend helper and Rust bootstrap collector")]
mod sdk;

use std::process::ExitCode;
pub(crate) use launcher::digest;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && let Some(path) = args[1].strip_prefix('@') {
        match std::fs::read_to_string(path) {
            Ok(text) => args = std::iter::once(args[0].clone()).chain(text.lines().map(str::to_owned)).collect(),
            Err(error) => { eprintln!("asksource: argument file {path}: {error}"); return ExitCode::FAILURE; }
        }
    }
    if args.get(1).is_some_and(|arg| arg == "asksource") { args.remove(1); }
    if args.get(1).is_some_and(|arg| matches!(arg.as_str(), "source" | "build" | "sdk-frontend" | "capture")) {
        match resource_budget::enter() {
            Ok(Some(code)) => return if code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE },
            Ok(None) => {},
            Err(error) => { eprintln!("asksource: {error}"); return ExitCode::FAILURE; }
        }
    }
    let result = match args.get(1).map(String::as_str) {
        Some("source") => syntax::run(),
        Some("identity") => cargo_build::identity(),
        Some("build") => cargo_build::run(&args[1..]),
        Some("sdk-frontend") => sdk::frontend(&args[2..]),
        Some("capture") => semantic::run(&args[2..]),
        _ => Err("usage: cargo asksource build --capture-request <request> [Cargo options] | cargo-asksource source | capture --output <facts> -- <rustc> <arguments>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("asksource: {error}");
            ExitCode::FAILURE
        }
    }
}
