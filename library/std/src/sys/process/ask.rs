//! `std::process` PAL: signed packages through Launcher's `askio::process`.
//!
//! `Command::new` takes an installed package identity, not a filesystem path.
//! Motor OS's `motor.rs` is the file-shape donor; ASK rewrites spawn around
//! the descriptor launch transaction over the inherited Launcher authority.

use super::CommandEnvs;
use super::env::{CommandEnv, CommandResolvedEnvs};
use crate::ffi::{OsStr, OsString};
use crate::num::NonZeroI32;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sync::{Mutex, OnceLock};
use crate::sys::FromInner;
use crate::sys::channel::SyncChannel;
use crate::sys::fd::FileDesc;
use crate::sys::fs::File;
#[path = "ask/activation.rs"]
mod activation;
use crate::sys::pal;
use crate::sys::pipe::Pipe;
use crate::{fmt, io};

pub type EnvKey = OsString;

static LAUNCHER: OnceLock<Mutex<SyncChannel>> = OnceLock::new();

fn require_class2() -> io::Result<()> {
    if crate::rt::ask_entry::execution_class() == ask_fd::execution_class::CLASS1 {
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    } else {
        Ok(())
    }
}

pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    InheritFile(File),
    Pipe(Pipe),
    Fd(FileDesc),
}

impl Stdio {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Inherit => Ok(Self::Inherit),
            Self::Null => Ok(Self::Null),
            Self::MakePipe => Ok(Self::MakePipe),
            Self::ParentStdout => Ok(Self::ParentStdout),
            Self::ParentStderr => Ok(Self::ParentStderr),
            Self::InheritFile(file) => file.duplicate().map(Self::InheritFile),
            Self::Pipe(pipe) => pipe.try_clone().map(Self::Pipe),
            Self::Fd(fd) => fd.try_clone().map(Self::Fd),
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
        require_class2()?;
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
        let env_pairs: Vec<(OsString, OsString)> = self.env.capture().into_iter().collect();
        let env = pack_env_pairs(&env_pairs)?;
        let mut stdin = plan_stdio(stdin, ask_io::fd::number::STDIN)?;
        let mut stdout = plan_stdio(stdout, ask_io::fd::number::STDOUT)?;
        let mut stderr = plan_stdio(stderr, ask_io::fd::number::STDERR)?;
        let actions = [&stdin, &stdout, &stderr]
            .into_iter()
            .map(|plan| ask_io::fd::OrderedAction {
                kind: ask_io::fd::action::DUP,
                source: plan.source,
                target: plan.target,
                reserved: 0,
            })
            .collect::<Vec<_>>();
        let launched = activation::launch(name, &argv, &env, &actions)?;
        let pipes = StdioPipes {
            stdin: stdin.parent.take(),
            stdout: stdout.parent.take(),
            stderr: stderr.parent.take(),
        };
        Ok((Process { identity: launched.identity(), status: None }, pipes))
    }
}

fn package_name(program: &OsStr) -> io::Result<&[u8]> {
    let bytes = program.as_encoded_bytes();
    if bytes.is_empty()
        || bytes.contains(&0)
        || bytes.len() > ask_io::process::selector::MAX_LEN
        || bytes.split(|byte| *byte == b'/').count() > 2
        || bytes
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(io::const_error!(
            io::ErrorKind::NotFound,
            "ask Command requires an installed package or package/entry selector"
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
        if field.as_encoded_bytes().contains(&0) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
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
        if key.as_encoded_bytes().is_empty()
            || key.as_encoded_bytes().iter().any(|byte| *byte == 0 || *byte == b'=')
            || value.as_encoded_bytes().contains(&0)
        {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
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

struct StdioPlan {
    source: i32,
    target: i32,
    _owned: Option<Stdio>,
    parent: Option<Pipe>,
}
fn plan_stdio(spec: Stdio, target: i32) -> io::Result<StdioPlan> {
    let mut parent = None;
    let spec = match spec {
        Stdio::Inherit => Stdio::Fd(FileDesc::from_inner(crate::sys::fd::duplicate(target)?)),
        Stdio::ParentStdout => {
            Stdio::Fd(FileDesc::from_inner(crate::sys::fd::duplicate(ask_io::fd::number::STDOUT)?))
        }
        Stdio::ParentStderr => {
            Stdio::Fd(FileDesc::from_inner(crate::sys::fd::duplicate(ask_io::fd::number::STDERR)?))
        }
        Stdio::Null => Stdio::Fd(crate::sys::fd::null()?),
        Stdio::MakePipe => {
            let (reader, writer) = crate::sys::pipe::pipe()?;
            if target == ask_io::fd::number::STDIN {
                parent = Some(writer);
                Stdio::Pipe(reader)
            } else {
                parent = Some(reader);
                Stdio::Pipe(writer)
            }
        }
        other => other,
    };
    let source = match &spec {
        Stdio::Inherit => target,
        Stdio::ParentStdout => ask_io::fd::number::STDOUT,
        Stdio::ParentStderr => ask_io::fd::number::STDERR,
        Stdio::InheritFile(file) => file.as_raw_fd(),
        Stdio::Pipe(pipe) => pipe.as_raw_fd(),
        Stdio::Fd(fd) => fd.as_raw_fd(),
        Stdio::Null | Stdio::MakePipe => return Err(pal::unsupported_err()),
    };
    Ok(StdioPlan { source, target, _owned: Some(spec), parent })
}

fn launcher() -> io::Result<crate::sync::MutexGuard<'static, SyncChannel>> {
    let cell = LAUNCHER.get_or_try_init(|| {
        SyncChannel::create_leased(ask_abi::APP_LAUNCHER_TOKEN, ask_io::process::CHANNEL_PAGES)
            .map(Mutex::new)
            .map_err(|_| io::const_error!(io::ErrorKind::NotConnected, "launcher channel rejected"))
    })?;
    Ok(cell.lock().unwrap_or_else(|e| e.into_inner()))
}

fn map_process_result(result: i32) -> io::Error {
    match result {
        ask_io::process::result::NOT_FOUND => {
            io::const_error!(io::ErrorKind::NotFound, "package not installed")
        }
        ask_io::process::result::DENIED => {
            io::const_error!(io::ErrorKind::PermissionDenied, "launch denied")
        }
        ask_io::process::result::BUSY => {
            io::const_error!(io::ErrorKind::WouldBlock, "launcher busy")
        }
        _ => io::const_error!(io::ErrorKind::Other, "launcher rejected the request"),
    }
}

impl From<Pipe> for Stdio {
    fn from(pipe: Pipe) -> Stdio {
        Stdio::Pipe(pipe)
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
    identity: ask_io::process::ProcessIdentity,
    status: Option<ExitStatus>,
}

impl Process {
    pub fn id(&self) -> u32 {
        self.identity.pid as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        require_class2()?;
        let mut payload = [0; ask_io::process::PROCESS_IDENTITY_LEN];
        let mut guard = launcher()?;
        let completion = guard.call(
            ask_io::process::OP_CANCEL,
            ask_io::process::encode_process_identity(&mut payload, self.identity),
        )?;
        drop(guard);
        if completion.result != ask_io::process::result::OK {
            return Err(map_process_result(completion.result));
        }
        Ok(())
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_flags(0)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self.wait_flags(ask_io::process::wait_flag::NONBLOCK) {
            Ok(status) => Ok(Some(status)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn wait_flags(&mut self, flags: u32) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        require_class2()?;
        let mut payload = [0; ask_io::process::WAIT_REQUEST_LEN];
        let mut guard = launcher()?;
        let completion = guard.call(
            ask_io::process::OP_WAIT,
            ask_io::process::encode_wait_request(&mut payload, self.identity, flags),
        )?;
        drop(guard);
        if completion.result == ask_io::process::result::BUSY {
            return Err(io::const_error!(io::ErrorKind::WouldBlock, "child still running"));
        }
        if completion.result != ask_io::process::result::OK {
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
