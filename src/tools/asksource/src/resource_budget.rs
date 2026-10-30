#[cfg(target_os = "linux")]
fn launcher() -> std::process::Command {
    let mut command = std::process::Command::new("python3");
    command.arg("-c").arg(include_str!("resource_budget.py"));
    command
}

pub fn require() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let status = launcher().arg("--guard").status().map_err(|error| error.to_string())?;
        if !status.success() {
            return Err("source analysis requires the shared memory budget; run through asksource.sh or the resource launcher".into());
        }
    }
    Ok(())
}

pub fn enter() -> Result<Option<i32>, String> {
    #[cfg(target_os = "linux")]
    {
        let status = launcher().arg("--guard").status().map_err(|error| error.to_string())?;
        match status.code() {
            Some(0) => return Ok(None),
            Some(1) => {},
            _ => return Err("shared analysis memory budget validation failed".into()),
        }
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let status = launcher().arg("--").arg(executable).args(std::env::args_os().skip(1))
            .status().map_err(|error| error.to_string())?;
        return Ok(Some(status.code().unwrap_or(1)));
    }
    #[cfg(not(target_os = "linux"))]
    Ok(None)
}
