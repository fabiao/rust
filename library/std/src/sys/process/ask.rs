//! `std::process` PAL: signed packages through Launcher's `askio::process`.
//!
//! `Command::new` takes an installed package identity, not a filesystem path.
//! Motor OS's `motor.rs` is the file-shape donor; ASK rewrites spawn around
//! `OP_LAUNCH` because `std` cannot depend on `askme`.

use super::CommandEnvs;
use super::env::{CommandEnv, CommandResolvedEnvs};
use crate::ffi::{OsStr, OsString};
use crate::num::NonZeroI32;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sync::{Mutex, OnceLock};
use crate::sys::channel::SyncChannel;
use crate::sys::fs::File;
use crate::sys::pipe::Pipe;
use crate::sys::{env, pal};
use crate::{fmt, io};

pub type EnvKey = OsString;

static LAUNCHER: OnceLock<Mutex<SyncChannel>> = OnceLock::new();

pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    InheritFile(File),
}

impl Stdio {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Inherit => Ok(Self::Inherit),
            Self::Null => Ok(Self::Null),
            Self::MakePipe => Ok(Self::MakePipe),
            Self::ParentStdout => Ok(Self::ParentStdout),
            Self::ParentStderr => Ok(Self::ParentStderr),
            Self::InheritFile(_) => Ok(Self::Inherit),
        }
    }
}

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,
    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        Command {
            program: program.to_owned(),
            args: vec![program.to_owned()],
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_owned());
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, dir: &OsStr) {
        self.cwd = Some(dir.to_owned());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }

    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }

    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        let mut iter = self.args.iter();
        iter.next();
        CommandArgs { iter }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }

    pub fn get_env_clear(&self) -> bool {
        self.env.does_clear()
    }

    pub fn get_resolved_envs(&self) -> CommandResolvedEnvs {
        CommandResolvedEnvs::new(self.env.capture())
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(Path::new)
    }

    pub fn spawn(
        &mut self,
        default: Stdio,
        needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        let stdin = match self.stdin.as_ref() {
            Some(stdin) => stdin.try_clone()?,
            None if needs_stdin => default.try_clone()?,
            None => Stdio::Null,
        };
        let stdout = match self.stdout.as_ref() {
            Some(stdout) => stdout.try_clone()?,
            None => default.try_clone()?,
        };
        let stderr = match self.stderr.as_ref() {
            Some(stderr) => stderr.try_clone()?,
            None => default.try_clone()?,
        };

        if self.cwd.is_some() {
            return Err(io::const_error!(
                io::ErrorKind::Unsupported,
                "Command current_dir is not a process property on ask"
            ));
        }

        let name = package_name(&self.program)?;
        let argv = pack_os_fields(self.args.iter().map(OsString::as_os_str))?;
        let mut env_pairs: Vec<(OsString, OsString)> =
            self.env.capture().into_iter().map(|(k, v)| (k, v)).collect();
        append_stdio_env(&mut env_pairs, &stdin, &stdout, &stderr)?;
        let env = pack_env_pairs(&env_pairs)?;

        let pid = launch(name, &argv, &env)?;
        let pipes = attach_parent_stdio(pid, &stdin, &stdout, &stderr)?;
        Ok((Process { pid, status: None }, pipes))
    }
}

fn package_name(program: &OsStr) -> io::Result<&[u8]> {
    let bytes = program.as_encoded_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'/') {
        return Err(io::const_error!(
            io::ErrorKind::NotFound,
            "ask Command launches an installed package by name, not a filesystem path"
        ));
    }
    Ok(bytes)
}

fn pack_os_fields<'a, I>(fields: I) -> io::Result<Vec<u8>>
where
    I: Iterator<Item = &'a OsStr>,
{
    let mut out = Vec::new();
    for field in fields {
        if !out.is_empty() {
            out.push(0);
        }
        out.extend_from_slice(field.as_encoded_bytes());
        if out.len() > ask_abi::MAX_ARGV_ENV_LEN {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "argv exceeds MAX_ARGV_ENV_LEN"
            ));
        }
    }
    Ok(out)
}

fn pack_env_pairs(pairs: &[(OsString, OsString)]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for (key, value) in pairs {
        if !out.is_empty() {
            out.push(0);
        }
        out.extend_from_slice(key.as_encoded_bytes());
        out.push(b'=');
        out.extend_from_slice(value.as_encoded_bytes());
        if out.len() > ask_abi::MAX_ARGV_ENV_LEN {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "environment exceeds MAX_ARGV_ENV_LEN"
            ));
        }
    }
    Ok(out)
}

fn push_env(pairs: &mut Vec<(OsString, OsString)>, key: &str, value: OsString) {
    pairs.retain(|(existing, _)| existing.as_encoded_bytes() != key.as_bytes());
    pairs.push((OsString::from(key), value));
}

fn append_stdio_env(
    pairs: &mut Vec<(OsString, OsString)>,
    stdin: &Stdio,
    stdout: &Stdio,
    stderr: &Stdio,
) -> io::Result<()> {
    let self_pid = OsString::from(getpid().to_string());
    if matches!(stdin, Stdio::MakePipe) {
        push_env(pairs, "ASK_STDIN_PIPE", OsString::from("1"));
    }
    if matches!(stdout, Stdio::MakePipe) {
        push_env(pairs, "ASK_STDOUT_TO_PID", self_pid.clone());
    }
    if matches!(stderr, Stdio::MakePipe) {
        push_env(pairs, "ASK_STDERR_TO_PID", self_pid);
    }
    Ok(())
}

fn launcher_pid() -> io::Result<u32> {
    if let Some(pid) = parse_env_pid("ASK_LAUNCHER_PID") {
        return Ok(pid);
    }
    if let Some(pid) = parse_env_pid("ASKHELL_LAUNCHER_PID") {
        return Ok(pid);
    }
    Err(io::const_error!(
        io::ErrorKind::NotFound,
        "ASK_LAUNCHER_PID is not in the process environment"
    ))
}

fn parse_env_pid(key: &str) -> Option<u32> {
    let value = env::getenv(OsStr::new(key))?;
    value.to_str()?.parse().ok()
}

fn launcher() -> io::Result<crate::sync::MutexGuard<'static, SyncChannel>> {
    let cell = LAUNCHER.get_or_try_init(|| {
        let pid = launcher_pid()?;
        SyncChannel::create(u64::from(pid), ask_io::process::CHANNEL_PAGES)
            .map(Mutex::new)
            .map_err(|_| {
                io::const_error!(io::ErrorKind::NotConnected, "launcher channel rejected")
            })
    })?;
    Ok(cell.lock().unwrap_or_else(|e| e.into_inner()))
}

fn launch(name: &[u8], argv: &[u8], env: &[u8]) -> io::Result<u32> {
    let total = name
        .len()
        .checked_add(argv.len())
        .and_then(|len| len.checked_add(env.len()))
        .filter(|total| *total <= ask_io::process::DATA_LEN as usize)
        .ok_or_else(|| {
            io::const_error!(io::ErrorKind::InvalidInput, "launch payload exceeds channel window")
        })?;
    if name.is_empty() {
        return Err(io::const_error!(io::ErrorKind::NotFound, "empty package name"));
    }
    let mut guard = launcher()?;
    let window = guard
        .shared_region_mut(ask_io::process::DATA_OFFSET as usize, total)
        .ok_or_else(pal::unsupported_err)?;
    let argv_offset = name.len();
    let env_offset = argv_offset + argv.len();
    window[..argv_offset].copy_from_slice(name);
    window[argv_offset..env_offset].copy_from_slice(argv);
    window[env_offset..total].copy_from_slice(env);
    let request = ask_io::process::LaunchRequest {
        name: ask_io::process::Buffer::new(0, name.len() as u32).ok_or_else(pal::unsupported_err)?,
        argv: ask_io::process::Buffer::new(argv_offset as u32, argv.len() as u32)
            .ok_or_else(pal::unsupported_err)?,
        env: ask_io::process::Buffer::new(env_offset as u32, env.len() as u32)
            .ok_or_else(pal::unsupported_err)?,
        flags: ask_io::process::FLAG_FOREGROUND,
    };
    let mut payload = [0; ask_io::process::LAUNCH_REQUEST_LEN];
    let completion = guard.call(
        ask_io::process::OP_LAUNCH,
        ask_io::process::encode_launch_request(&mut payload, request),
    )?;
    drop(guard);
    if completion.result != ask_io::process::RESULT_OK {
        return Err(map_process_result(completion.result));
    }
    ask_io::process::decode_process_id(completion.payload()).ok_or_else(pal::unsupported_err)
}

fn attach_parent_stdio(
    child: u32,
    stdin: &Stdio,
    stdout: &Stdio,
    stderr: &Stdio,
) -> io::Result<StdioPipes> {
    let stdin_pipe = if matches!(stdin, Stdio::MakePipe) {
        Some(crate::sys::pipe::writer_to_peer(child)?)
    } else {
        None
    };
    let stdout_pipe = if matches!(stdout, Stdio::MakePipe) {
        Some(crate::sys::pipe::accept_reader()?)
    } else {
        None
    };
    let stderr_pipe = if matches!(stderr, Stdio::MakePipe) {
        Some(crate::sys::pipe::accept_reader()?)
    } else {
        None
    };
    Ok(StdioPipes { stdin: stdin_pipe, stdout: stdout_pipe, stderr: stderr_pipe })
}

fn map_process_result(result: i32) -> io::Error {
    match result {
        ask_io::process::RESULT_NOT_FOUND => {
            io::const_error!(io::ErrorKind::NotFound, "package not installed")
        }
        ask_io::process::RESULT_DENIED => {
            io::const_error!(io::ErrorKind::PermissionDenied, "launch denied")
        }
        ask_io::process::RESULT_BUSY => {
            io::const_error!(io::ErrorKind::WouldBlock, "launcher busy")
        }
        _ => io::const_error!(io::ErrorKind::Other, "launcher rejected the request"),
    }
}

impl From<Pipe> for Stdio {
    fn from(_pipe: Pipe) -> Stdio {
        Stdio::MakePipe
    }
}

impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Stdio {
        Stdio::ParentStdout
    }
}

impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Stdio {
        Stdio::ParentStderr
    }
}

impl From<File> for Stdio {
    fn from(file: File) -> Stdio {
        Stdio::InheritFile(file)
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("program", &self.program)
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ExitStatus(i32);

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        if self.0 == 0 { Ok(()) } else { Err(ExitStatusError(*self)) }
    }

    pub fn code(&self) -> Option<i32> {
        Some(self.0)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit status: {}", self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitStatusError(ExitStatus);

impl From<ExitStatusError> for ExitStatus {
    fn from(status: ExitStatusError) -> ExitStatus {
        status.0
    }
}

impl ExitStatusError {
    pub fn code(self) -> Option<NonZeroI32> {
        NonZeroI32::new(self.0.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitCode(i32);

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn as_i32(&self) -> i32 {
        self.0
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code as i32)
    }
}

pub struct Process {
    pid: u32,
    status: Option<ExitStatus>,
}

impl Process {
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn kill(&mut self) -> io::Result<()> {
        let mut payload = [0; 4];
        let mut guard = launcher()?;
        let completion = guard.call(
            ask_io::process::OP_CANCEL,
            ask_io::process::encode_process_id(&mut payload, self.pid),
        )?;
        drop(guard);
        if completion.result != ask_io::process::RESULT_OK {
            return Err(map_process_result(completion.result));
        }
        Ok(())
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_flags(0)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self.wait_flags(ask_io::process::FLAG_NONBLOCK) {
            Ok(status) => Ok(Some(status)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn wait_flags(&mut self, flags: u32) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let mut payload = [0; ask_io::process::WAIT_REQUEST_LEN];
        let mut guard = launcher()?;
        let completion = guard.call(
            ask_io::process::OP_WAIT,
            ask_io::process::encode_wait_request(&mut payload, self.pid, flags),
        )?;
        drop(guard);
        if completion.result == ask_io::process::RESULT_BUSY {
            return Err(io::const_error!(io::ErrorKind::WouldBlock, "child still running"));
        }
        if completion.result != ask_io::process::RESULT_OK {
            return Err(map_process_result(completion.result));
        }
        let status = ask_io::process::decode_exit_status(completion.payload())
            .ok_or_else(pal::unsupported_err)?;
        self.status = Some(ExitStatus(status as i32));
        Ok(ExitStatus(status as i32))
    }
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}

impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;
    fn next(&mut self) -> Option<&'a OsStr> {
        self.iter.next().map(OsString::as_os_str)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl ExactSizeIterator for CommandArgs<'_> {
    fn len(&self) -> usize {
        self.iter.len()
    }
}

impl fmt::Debug for CommandArgs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

pub type ChildPipe = Pipe;

pub fn read_output(
    out: ChildPipe,
    stdout: &mut Vec<u8>,
    err: ChildPipe,
    stderr: &mut Vec<u8>,
) -> io::Result<()> {
    out.read_to_end(stdout)?;
    err.read_to_end(stderr)?;
    Ok(())
}

pub fn getpid() -> u32 {
    ask_sys::get_pid_uncached() as u32
}
