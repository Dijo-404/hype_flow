use anyhow::{Context, Result, bail};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(30);

pub(crate) struct ChildGroup {
    pub child: Child,
    armed: bool,
}

impl ChildGroup {
    pub(crate) fn spawn(command: &mut Command) -> Result<Self> {
        let child = command
            .process_group(0)
            .spawn()
            .with_context(|| format!("Could not start {:?}", command.get_program()))?;
        Ok(Self { child, armed: true })
    }

    fn signal(&self, signal: libc::c_int) {
        // A negative PID addresses the process group created by process_group(0).
        unsafe { libc::kill(-(self.child.id() as libc::pid_t), signal) };
    }

    pub(crate) fn exited(&self) -> io::Result<bool> {
        // Keep the leader's PID reserved until group signalling is complete.
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        loop {
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return Ok(unsafe { info.assume_init().si_pid() } != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    pub(crate) fn reap_exited(&mut self) -> io::Result<Option<ExitStatus>> {
        if !self.exited()? {
            return Ok(None);
        }
        self.signal(libc::SIGKILL);
        let status = self.child.wait()?;
        self.armed = false;
        Ok(Some(status))
    }

    pub(crate) fn terminate(&mut self, grace: Duration) {
        if !self.armed {
            return;
        }
        if !grace.is_zero() {
            self.signal(libc::SIGTERM);
            let deadline = Instant::now() + grace;
            while matches!(self.exited(), Ok(false)) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                thread::sleep(remaining.min(POLL_INTERVAL));
            }
        }
        self.signal(libc::SIGKILL);
        let _ = self.child.wait();
        self.armed = false;
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        self.terminate(Duration::ZERO);
    }
}

pub(crate) fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // Only the parent ends of the pipes are changed to nonblocking mode.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn poll(fds: &mut [libc::pollfd], timeout: Duration) -> io::Result<()> {
    let milliseconds = timeout.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
    // The slice owns the pollfd array for the duration of the system call.
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, milliseconds) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

fn poll_fd(pipe: Option<&impl AsRawFd>, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd: pipe.map_or(-1, AsRawFd::as_raw_fd),
        events,
        revents: 0,
    }
}

pub(crate) fn wait_for_io(
    fd: RawFd,
    events: libc::c_short,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<()> {
    if cancelled() {
        bail!("Operation cancelled");
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("Operation timed out");
    }
    poll(
        &mut [libc::pollfd {
            fd,
            events,
            revents: 0,
        }],
        remaining.min(POLL_INTERVAL),
    )?;
    if cancelled() {
        bail!("Operation cancelled");
    }
    if Instant::now() >= deadline {
        bail!("Operation timed out");
    }
    Ok(())
}

fn drain_pipe(pipe: &mut Option<impl Read>, output: &mut Vec<u8>, limit: usize) -> Result<()> {
    let mut buffer = [0; 8192];
    let length = match pipe
        .as_mut()
        .expect("polled pipe is open")
        .read(&mut buffer)
    {
        Ok(0) => {
            *pipe = None;
            return Ok(());
        }
        Ok(length) => length,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if output.len() + length > limit {
        bail!("Command exceeded the output limit");
    }
    output.extend_from_slice(&buffer[..length]);
    Ok(())
}

pub fn run_command(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output> {
    run_command_until(command, input, timeout, true, || false)
}

pub(crate) fn run_command_without_stderr(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output> {
    run_command_until(command, input, timeout, false, || false)
}

pub(crate) fn run_command_until(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    capture_stderr: bool,
    cancelled: impl Fn() -> bool,
) -> Result<Output> {
    if cancelled() {
        bail!("Command cancelled");
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("Invalid command timeout")?;
    let mut remaining_input = input.unwrap_or_default();
    let mut child = ChildGroup::spawn(
        command
            .stdin(if remaining_input.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(if capture_stderr {
                Stdio::piped()
            } else {
                Stdio::null()
            }),
    )?;
    let mut stdin = child.child.stdin.take();
    let mut stdout = child.child.stdout.take();
    let mut stderr = child.child.stderr.take();
    if let Some(pipe) = &stdin {
        nonblocking(pipe)?;
    }
    nonblocking(stdout.as_ref().expect("stdout is piped"))?;
    if let Some(pipe) = &stderr {
        nonblocking(pipe)?;
    }
    let mut output = Vec::new();
    let mut errors = Vec::new();
    // pidfd wakes when the leader exits, even after it has closed both pipes.
    let exit_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.child.id(), 0) };
    let exit_fd = (exit_fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(exit_fd as RawFd) });
    let mut exit_seen = false;
    loop {
        if cancelled() {
            bail!("Command cancelled");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("Command timed out");
        }
        if stdout.is_none()
            && stderr.is_none()
            && let Some(status) = child.child.try_wait()?
        {
            child.armed = false;
            return Ok(Output {
                status,
                stdout: output,
                stderr: errors,
            });
        }
        let mut fds = [
            poll_fd(stdin.as_ref(), libc::POLLOUT),
            poll_fd(stdout.as_ref(), libc::POLLIN),
            poll_fd(stderr.as_ref(), libc::POLLIN),
            poll_fd(exit_fd.as_ref().filter(|_| !exit_seen), libc::POLLIN),
        ];
        poll(&mut fds, remaining.min(POLL_INTERVAL))?;
        exit_seen |= fds[3].revents != 0;
        if fds[0].revents != 0 {
            let pipe = stdin.as_mut().expect("polled stdin is open");
            match pipe.write(remaining_input) {
                Ok(written) => remaining_input = &remaining_input[written..],
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => remaining_input = &[],
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            if remaining_input.is_empty() {
                stdin = None;
            }
        }
        if fds[1].revents != 0 {
            drain_pipe(&mut stdout, &mut output, OUTPUT_LIMIT - errors.len())?;
        }
        if fds[2].revents != 0 {
            drain_pipe(&mut stderr, &mut errors, OUTPUT_LIMIT - output.len())?;
        }
    }
}

pub(crate) const CHILD_GRACE: Duration = Duration::from_secs(3);
