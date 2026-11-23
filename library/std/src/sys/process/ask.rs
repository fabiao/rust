//! `std::process` PAL: signed packages launched through the ASK runtime.
//!
//! `Command::new` takes an installed package identity, not a filesystem path.
//! Motor OS's `motor.rs` is the file-shape donor; the runtime performs the
//! descriptor launch transaction over the inherited Launcher authority.

use super::CommandEnvs;
use super::env::{CommandEnv, CommandResolvedEnvs};
use crate::ffi::{OsStr, OsString};
use crate::num::NonZeroI32;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::os::fd::{FromRawFd, OwnedFd};
use crate::sys::FromInner;
use crate::sys::fd::FileDesc;
use crate::sys::fs::File;
use crate::sys::pal::{self, ask_error};
use crate::sys::pipe::Pipe;
use crate::{fmt, io};

pub type EnvKey = OsString;

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

        let directory = match self.cwd.as_ref() {
            Some(dir) => Some(
                dir.to_str().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidFilename))?,
            ),
            None => None,
        };
        let argv = pack_os_fields(self.args.iter().map(OsString::as_os_str))?;
        let env_pairs: Vec<(OsString, OsString)> = self.env.capture().into_iter().collect();
        let env = pack_env_pairs(&env_pairs)?;
        let mut stdin = plan_stdio(stdin, askrust::fd::number::STDIN)?;
        let mut stdout = plan_stdio(stdout, askrust::fd::number::STDOUT)?;
        let mut stderr = plan_stdio(stderr, askrust::fd::number::STDERR)?;
        let child = askrust::process::spawn(
            self.program.as_encoded_bytes(),
            &argv,
            &env,
            directory,
            [stdin.source, stdout.source, stderr.source],
        )
        .map_err(ask_error)?;
        let pipes = StdioPipes {
            stdin: stdin.parent.take(),
            stdout: stdout.parent.take(),
            stderr: stderr.parent.take(),
        };
        Ok((Process { child, status: None }, pipes))
    }
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
    }
    Ok(out)
}

struct StdioPlan {
    source: i32,
    _owned: Option<Stdio>,
    parent: Option<Pipe>,
}
fn plan_stdio(spec: Stdio, target: i32) -> io::Result<StdioPlan> {
    let mut parent = None;
    let spec = match spec {
        Stdio::Inherit => Stdio::Fd(FileDesc::from_inner(crate::sys::fd::duplicate(target)?)),
        Stdio::ParentStdout => Stdio::Fd(FileDesc::from_inner(crate::sys::fd::duplicate(
            askrust::fd::number::STDOUT,
        )?)),
        Stdio::ParentStderr => Stdio::Fd(FileDesc::from_inner(crate::sys::fd::duplicate(
            askrust::fd::number::STDERR,
        )?)),
        Stdio::Null => {
            let fd = askrust::fd::null().map_err(ask_error)?;
            // SAFETY: the runtime returned a newly bound descriptor owned by the caller.
            Stdio::Fd(unsafe { FileDesc::from_raw_fd(fd) })
        }
        Stdio::MakePipe => {
            let (reader, writer) = crate::sys::pipe::pipe()?;
            if target == askrust::fd::number::STDIN {
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
        Stdio::ParentStdout => askrust::fd::number::STDOUT,
        Stdio::ParentStderr => askrust::fd::number::STDERR,
        Stdio::InheritFile(file) => file.as_raw_fd(),
        Stdio::Pipe(pipe) => pipe.as_raw_fd(),
        Stdio::Fd(fd) => fd.as_raw_fd(),
        Stdio::Null | Stdio::MakePipe => return Err(pal::unsupported_err()),
    };
    Ok(StdioPlan { source, _owned: Some(spec), parent })
}

impl From<Pipe> for Stdio {
    fn from(pipe: Pipe) -> Stdio {
        Stdio::Pipe(pipe)
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
impl From<OwnedFd> for crate::process::Stdio {
    fn from(fd: OwnedFd) -> Self {
        Self::from_inner(Stdio::Fd(FileDesc::from_inner(fd)))
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExitStatus(askrust::process::Outcome);

impl Default for ExitStatus {
    fn default() -> Self { Self(askrust::process::Outcome::raw_exit(0)) }
}

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        if self.0.is_success() { Ok(()) } else { Err(ExitStatusError(*self)) }
    }

    pub fn code(&self) -> Option<i32> {
        self.0.raw_exit_value().and_then(|value| u32::try_from(value).ok()).map(|value| value as i32)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.raw_exit_value() {
            Some(value) => write!(f, "exit status: {value}"),
            None => write!(
                f,
                "exit status: {}(label={:#x}, value={:#x})",
                self.0.origin_name(),
                self.0.label,
                self.0.value
            ),
        }
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
        self.0.code().and_then(NonZeroI32::new)
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
    child: askrust::process::Child,
    status: Option<ExitStatus>,
}

impl Process {
    pub fn id(&self) -> u32 {
        self.child.pid as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        askrust::process::kill(self.child).map_err(ask_error)
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_mode(false)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self.wait_mode(true) {
            Ok(status) => Ok(Some(status)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn wait_mode(&mut self, nonblocking: bool) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = ExitStatus(askrust::process::wait(self.child, nonblocking).map_err(ask_error)?);
        self.status = Some(status);
        Ok(status)
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
    askrust::process::id()
}
