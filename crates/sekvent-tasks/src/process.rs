//! Process spawning behind the [`Runner`] trait, so tasks can be tested by
//! asserting on the commands they would run.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A command to run: program, arguments, extra environment and directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    /// The executable.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Variables added to the inherited environment.
    pub env: Vec<(String, String)>,
    /// Working directory; the current one when `None`.
    pub cwd: Option<PathBuf>,
}

impl Cmd {
    /// A command running `program` with no arguments.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// A command from an argv list; `None` when the list is empty.
    pub fn from_argv(argv: &[String]) -> Option<Self> {
        let (program, rest) = argv.split_first()?;
        Some(Self::new(program.clone()).args(rest.iter().cloned()))
    }

    /// Append one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append several arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set one environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Set several environment variables.
    #[must_use]
    pub fn envs(mut self, vars: &[(String, String)]) -> Self {
        self.env.extend(vars.iter().cloned());
        self
    }

    /// Run in `dir`.
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// The value this command sets for `key`, if any.
    pub fn env_value(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command.envs(self.env.iter().map(|(key, value)| (key, value)));
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        command
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&quote(&self.program))?;
        for arg in &self.args {
            write!(f, " {}", quote(arg))?;
        }
        Ok(())
    }
}

fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,+@%".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// What a captured command produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit code; `128 + signal` when killed by a signal.
    pub code: i32,
    /// Standard output, lossily decoded.
    pub stdout: String,
    /// Standard error, lossily decoded.
    pub stderr: String,
}

impl CmdOutput {
    /// The command exited with 0.
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// Runs commands. The real implementation is [`SystemRunner`]; tests use a
/// recording fake.
pub trait Runner {
    /// Run with inherited stdio and return the exit code.
    fn status(&self, cmd: &Cmd) -> io::Result<i32>;

    /// Run with captured output. With a `timeout`, a command still running at
    /// the deadline is killed and the call fails with
    /// [`io::ErrorKind::TimedOut`]; use it only for commands with small output.
    fn output(&self, cmd: &Cmd, timeout: Option<Duration>) -> io::Result<CmdOutput>;

    /// Replace this process with `cmd` where the platform allows it, else run
    /// it and return its exit code. Returns only on failure on Unix.
    fn exec(&self, cmd: &Cmd) -> io::Result<i32>;
}

/// Spawns real processes.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn status(&self, cmd: &Cmd) -> io::Result<i32> {
        let status = cmd
            .to_command()
            .status()
            .map_err(|error| spawn_error(cmd, &error))?;
        Ok(exit_code(status))
    }

    fn output(&self, cmd: &Cmd, timeout: Option<Duration>) -> io::Result<CmdOutput> {
        let mut command = cmd.to_command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| spawn_error(cmd, &error))?;
        if let Some(timeout) = timeout {
            let deadline = Instant::now() + timeout;
            while child.try_wait()?.is_none() {
                if Instant::now() >= deadline {
                    // Best effort: the child may exit between the check and the kill.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("`{cmd}` did not finish within {timeout:?}"),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let output = child.wait_with_output()?;
        Ok(CmdOutput {
            code: exit_code(output.status),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    #[cfg(unix)]
    fn exec(&self, cmd: &Cmd) -> io::Result<i32> {
        use std::os::unix::process::CommandExt;
        let error = cmd.to_command().exec();
        Err(spawn_error(cmd, &error))
    }

    #[cfg(not(unix))]
    fn exec(&self, cmd: &Cmd) -> io::Result<i32> {
        self.status(cmd)
    }
}

fn spawn_error(cmd: &Cmd, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("cannot run `{}`: {error}", cmd.program),
    )
}

/// The exit code of `status`, mapping a signal to `128 + signal`.
pub fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// A [`Runner`] that records commands instead of running them.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::{Cmd, CmdOutput, Runner};

    /// Records every command; answers `status` and `exec` from a queue of
    /// exit codes (0 when empty) and `output` from a queue of outputs.
    #[derive(Debug, Default)]
    pub(crate) struct FakeRunner {
        pub(crate) calls: RefCell<Vec<Cmd>>,
        pub(crate) codes: RefCell<VecDeque<i32>>,
        pub(crate) outputs: RefCell<VecDeque<std::io::Result<CmdOutput>>>,
    }

    impl FakeRunner {
        pub(crate) fn with_codes(codes: &[i32]) -> Self {
            let runner = Self::default();
            runner.codes.borrow_mut().extend(codes.iter().copied());
            runner
        }

        pub(crate) fn push_output(&self, stdout: &str) {
            self.outputs.borrow_mut().push_back(Ok(CmdOutput {
                code: 0,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            }));
        }

        pub(crate) fn calls(&self) -> Vec<Cmd> {
            self.calls.borrow().clone()
        }

        pub(crate) fn lines(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(ToString::to_string)
                .collect()
        }
    }

    impl Runner for FakeRunner {
        fn status(&self, cmd: &Cmd) -> std::io::Result<i32> {
            self.calls.borrow_mut().push(cmd.clone());
            Ok(self.codes.borrow_mut().pop_front().unwrap_or(0))
        }

        fn output(
            &self,
            cmd: &Cmd,
            _timeout: Option<std::time::Duration>,
        ) -> std::io::Result<CmdOutput> {
            self.calls.borrow_mut().push(cmd.clone());
            self.outputs
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(CmdOutput::default()))
        }

        fn exec(&self, cmd: &Cmd) -> std::io::Result<i32> {
            self.status(cmd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_collects_arguments_env_and_dir() {
        let cmd = Cmd::new("cargo")
            .arg("test")
            .args(["--workspace", "--locked"])
            .env("A", "1")
            .envs(&[("B".into(), "2".into())])
            .env("A", "3")
            .cwd("/work");
        assert_eq!(cmd.args, ["test", "--workspace", "--locked"]);
        assert_eq!(cmd.env_value("A"), Some("3"));
        assert_eq!(cmd.env_value("B"), Some("2"));
        assert_eq!(cmd.env_value("C"), None);
        assert_eq!(cmd.cwd, Some(PathBuf::from("/work")));
    }

    #[test]
    fn display_quotes_only_when_needed() {
        let cmd = Cmd::new("cargo").args(["x", "a b", "", "it's", "--x=1,2"]);
        assert_eq!(cmd.to_string(), r"cargo x 'a b' '' 'it'\''s' --x=1,2");
    }

    #[test]
    fn argv_lists_become_commands() {
        let argv = vec!["echo".to_owned(), "hi".to_owned()];
        let cmd = Cmd::from_argv(&argv).unwrap();
        assert_eq!(cmd.program, "echo");
        assert_eq!(cmd.args, ["hi"]);
        assert!(Cmd::from_argv(&[]).is_none());
    }

    #[test]
    fn captured_output_reports_success() {
        assert!(CmdOutput::default().success());
        let failed = CmdOutput {
            code: 2,
            ..CmdOutput::default()
        };
        assert!(!failed.success());
    }

    const MISSING: &str = "sekvent-tasks-no-such-program";

    fn shell(script: &str) -> Cmd {
        Cmd::new("sh").args(["-c", script])
    }

    #[cfg(unix)]
    #[test]
    fn the_system_runner_reports_exit_codes() {
        assert_eq!(SystemRunner.status(&Cmd::new("true")).unwrap(), 0);
        assert_eq!(SystemRunner.status(&Cmd::new("false")).unwrap(), 1);
        assert_eq!(SystemRunner.status(&shell("exit 3")).unwrap(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_maps_to_128_plus_its_number() {
        assert_eq!(SystemRunner.status(&shell("kill -9 $$")).unwrap(), 137);
        let output = SystemRunner.output(&shell("kill -15 $$"), None).unwrap();
        assert_eq!(output.code, 143);
    }

    #[cfg(unix)]
    #[test]
    fn output_is_captured_with_env_and_directory() {
        const SCRIPT: &str =
            "printf '%s|%s' \"$SEKVENT_PROCESS_TEST\" \"$(pwd -P)\"; echo oops >&2; exit 2";
        let dir = tempfile::tempdir().unwrap();
        let cmd = shell(SCRIPT)
            .env("SEKVENT_PROCESS_TEST", "value")
            .cwd(dir.path());
        let output = SystemRunner.output(&cmd, None).unwrap();
        let expected_dir = dir.path().canonicalize().unwrap();
        assert_eq!(output.stdout, format!("value|{}", expected_dir.display()));
        assert_eq!(output.stderr, "oops\n");
        assert_eq!(output.code, 2);
        assert!(!output.success());
    }

    #[cfg(unix)]
    #[test]
    fn output_within_the_timeout_is_returned() {
        let cmd = Cmd::new("echo").arg("hi");
        let output = SystemRunner
            .output(&cmd, Some(Duration::from_mins(1)))
            .unwrap();
        assert_eq!(output.stdout, "hi\n");
        assert!(output.success());
    }

    #[cfg(unix)]
    #[test]
    fn a_command_past_its_timeout_is_killed() {
        let started = Instant::now();
        let error = SystemRunner
            .output(
                &Cmd::new("sleep").arg("30"),
                Some(Duration::from_millis(100)),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("did not finish"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn a_missing_program_names_the_program() {
        let cmd = Cmd::new(MISSING);
        for error in [
            SystemRunner.status(&cmd).unwrap_err(),
            SystemRunner.output(&cmd, None).unwrap_err(),
            SystemRunner.exec(&cmd).unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("cannot run `{MISSING}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn the_fake_runner_answers_from_its_queues() {
        let runner = fake::FakeRunner::with_codes(&[4]);
        runner.push_output("out");
        assert_eq!(runner.exec(&Cmd::new("a")).unwrap(), 4);
        assert_eq!(runner.status(&Cmd::new("b")).unwrap(), 0);
        assert_eq!(runner.output(&Cmd::new("c"), None).unwrap().stdout, "out");
        assert_eq!(
            runner.output(&Cmd::new("d"), None).unwrap(),
            CmdOutput::default()
        );
        assert_eq!(runner.lines(), ["a", "b", "c", "d"]);
    }
}
