//! The only module that spawns `git`.
//!
//! Every call passes explicit arguments (no shell), runs with the environment in
//! [`BASE_ENV`], closes or feeds stdin, and returns the captured stderr on failure.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;

/// Applied to every call, before any per-handle environment.
///
/// `GIT_OPTIONAL_LOCKS=0` stops read-only commands such as `git status` from refreshing and
/// rewriting the index as a side effect: carve writes the index exactly once, at the end.
/// `GIT_LITERAL_PATHSPECS=1` makes a path like `a*.ts` name that one file, never a glob.
const BASE_ENV: &[(&str, &str)] = &[
    ("LC_ALL", "C"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_LITERAL_PATHSPECS", "1"),
];

/// Removed from every call's inherited environment (see `output`).
const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
];

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("could not run `git {args}`: {source}")]
    Spawn {
        args: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`git {args}` failed ({status}): {stderr}")]
    Failed {
        args: String,
        status: ExitStatus,
        stderr: String,
    },
    #[error("`git {args}` printed non-UTF-8 output")]
    NonUtf8 { args: String },
    #[error("unsupported object format `{0}`")]
    UnknownObjectFormat(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    /// Length of an object id in hex characters.
    pub fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }

    /// The all-zeros object id, used for deletions in `update-index --index-info`.
    pub fn null_oid(self) -> String {
        "0".repeat(self.hex_len())
    }
}

/// A handle for running git in one directory, with optional extra environment.
#[derive(Clone, Debug)]
pub struct Git {
    dir: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            env: Vec::new(),
        }
    }

    /// Sets an environment variable for every call made through this handle, such as
    /// `GIT_INDEX_FILE` for a temporary index. It overrides [`BASE_ENV`].
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The same handle, environment included, running in another directory.
    pub fn with_dir(&self, dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            env: self.env.clone(),
        }
    }

    /// Runs git and returns its stdout. A non-zero exit is an error that carries stderr.
    pub fn run<I, S>(&self, args: I) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect(args);
        checked(&args, self.output(&args, None)?)
    }

    /// Like [`Git::run`], writing `input` to git's stdin.
    pub fn run_with_stdin<I, S>(&self, args: I, input: &[u8]) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect(args);
        checked(&args, self.output(&args, Some(input))?)
    }

    /// Runs git and returns stdout as UTF-8 without the trailing newline.
    pub fn run_str<I, S>(&self, args: I) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect(args);
        let stdout = checked(&args, self.output(&args, None)?)?;
        text(&args, stdout)
    }

    /// Like [`Git::run_str`], but exit code 1 means "no answer" and returns `None`, as with
    /// `rev-parse --verify -q`, `symbolic-ref -q`, or `config` on an unset key.
    pub fn try_run_str<I, S>(&self, args: I) -> Result<Option<String>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect(args);
        let output = self.output(&args, None)?;
        match output.status.code() {
            Some(0) => text(&args, output.stdout).map(Some),
            Some(1) => Ok(None),
            _ => Err(failed(&args, output)),
        }
    }

    /// Runs a command that answers through its exit code, such as `diff --quiet`:
    /// 0 is `true`, 1 is `false`, anything else is an error.
    pub fn test<I, S>(&self, args: I) -> Result<bool, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect(args);
        let output = self.output(&args, None)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed(&args, output)),
        }
    }

    pub fn object_format(&self) -> Result<ObjectFormat, GitError> {
        match self
            .run_str(["rev-parse", "--show-object-format"])?
            .as_str()
        {
            "sha1" => Ok(ObjectFormat::Sha1),
            "sha256" => Ok(ObjectFormat::Sha256),
            other => Err(GitError::UnknownObjectFormat(other.to_owned())),
        }
    }

    fn output(&self, args: &[OsString], input: Option<&[u8]>) -> Result<Output, GitError> {
        let spawn_error = |source| GitError::Spawn {
            args: display(args),
            source,
        };
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(&self.dir);
        // Inherited repository variables would redirect every handle to one repository: git
        // exports GIT_DIR to a pre-push hook in a linked worktree, and the scratch worktrees'
        // `read-tree -u` would then rewrite the user's index. carve finds the repository from
        // each handle's directory; a handle that needs one of these sets it explicitly.
        for var in REPO_ENV {
            cmd.env_remove(var);
        }
        cmd.envs(BASE_ENV.iter().copied())
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let Some(input) = input else {
            return cmd.stdin(Stdio::null()).output().map_err(spawn_error);
        };
        let mut child = cmd.stdin(Stdio::piped()).spawn().map_err(spawn_error)?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        // Write from another thread: git may fill the stdout pipe before reading all of its
        // input, and reading stdout only after writing everything would deadlock.
        thread::scope(|scope| {
            let writer = scope.spawn(move || stdin.write_all(input));
            let output = child.wait_with_output().map_err(spawn_error)?;
            let written = writer.join().expect("stdin writer panicked");
            // If git failed, its stderr explains more than the broken pipe would.
            if output.status.success() {
                written.map_err(spawn_error)?;
            }
            Ok(output)
        })
    }
}

fn collect<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter().map(|a| a.as_ref().to_owned()).collect()
}

fn display(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn text(args: &[OsString], stdout: Vec<u8>) -> Result<String, GitError> {
    let mut text = String::from_utf8(stdout).map_err(|_| GitError::NonUtf8 {
        args: display(args),
    })?;
    if text.ends_with('\n') {
        text.pop();
    }
    Ok(text)
}

fn checked(args: &[OsString], output: Output) -> Result<Vec<u8>, GitError> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(failed(args, output))
    }
}

fn failed(args: &[OsString], output: Output) -> GitError {
    GitError::Failed {
        args: display(args),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr)
            .trim_end()
            .to_owned(),
    }
}
