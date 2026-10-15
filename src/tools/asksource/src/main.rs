#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;
extern crate rustc_session;

mod syntax;
mod semantic;
mod wrapper;

use std::process::ExitCode;
use sha2::{Digest, Sha256};

pub(crate) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("source") => syntax::run(),
        Some("capture") => semantic::run(&args[2..]),
        Some(_) if std::env::var_os("ASK_SOURCE_CAPTURE_REQUEST").is_some() => wrapper::run(&args[1..]),
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
