#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;

mod syntax;
mod semantic;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("source") => syntax::run(),
        Some("capture") => semantic::run(&args[2..]),
        _ => Err("usage: asksource source | capture <rustc> <rustc arguments>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("asksource: {error}");
            ExitCode::FAILURE
        }
    }
}
