#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;
extern crate rustc_session;

mod syntax;
mod semantic;
mod cargo_build;

use std::process::ExitCode;
use sha2::{Digest, Sha256};

pub(crate) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "asksource") { args.remove(1); }
    let result = match args.get(1).map(String::as_str) {
        Some("source") => syntax::run(),
        Some("identity") => cargo_build::identity(),
        Some("build") => cargo_build::run(&args[1..]),
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
