use std::path::PathBuf;
use std::process::Command;

mod native {
    use cargo;
    use cargo_util;
    trait TargetArgument {
        fn selected_target_argument(self) -> Self;
    }
    impl TargetArgument for cargo::util::command_prelude::Command {
        fn selected_target_argument(self) -> Self {
            use cargo::util::command_prelude::CommandExt;
            self.arg_target_tuple("Build for the target tuple")
        }
    }
    include!("cargo_executor.rs");
}

mod bootstrap {
    use cargo_bootstrap as cargo;
    use cargo_util_bootstrap as cargo_util;
    trait TargetArgument {
        fn selected_target_argument(self) -> Self;
    }
    impl TargetArgument for cargo::util::command_prelude::Command {
        fn selected_target_argument(self) -> Self {
            use cargo::util::command_prelude::CommandExt;
            self.arg_target_triple("Build for the target tuple")
        }
    }
    include!("cargo_executor.rs");
}

pub fn run(arguments: &[String]) -> Result<(), String> {
    let executable = PathBuf::from(std::env::var_os("CARGO").ok_or(
        "invoke through cargo asksource build so Cargo supplies its actual executable",
    )?);
    let version = Command::new(&executable).arg("-Vv").output().map_err(|e| e.to_string())?;
    if !version.status.success() {
        return Err(format!("cannot identify selected Cargo: {}", executable.display()));
    }
    let version = String::from_utf8(version.stdout).map_err(|e| e.to_string())?;
    let commits: Vec<_> = version.lines().filter_map(|line| line.strip_prefix("commit-hash: ")).collect();
    let [commit] = commits.as_slice() else {
        return Err("selected Cargo must report exactly one source commit".into());
    };
    if cargo::version().commit_info.is_some_and(|info| info.commit_hash == *commit) {
        native::run(arguments)
    } else if cargo_bootstrap::version().commit_info.is_some_and(|info| info.commit_hash == *commit) {
        bootstrap::run(arguments)
    } else {
        Err(format!("selected Cargo commit {commit} has no exact capture adapter; provision the matching source producer"))
    }
}

pub fn identity() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::json!({"path": executable,
        "sha256": crate::digest(&std::fs::read(&executable).map_err(|e| e.to_string())?)})
    );
    Ok(())
}
