use std::io;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::unistd;
use process_wrap::tokio::{ChildWrapper, CommandWrap, ProcessSession};
use rustix::io::Errno;
use rustix::process::{self, Pid, Signal};
use tokio::process::Command;

#[cfg(target_os = "macos")]
use cupel_macos_process::ProcessIdentity;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::process::EXIT_STDIO_GRACE;

pub(super) fn spawn(mut command: Command) -> io::Result<Box<dyn ChildWrapper>> {
    command.kill_on_drop(true);
    let mut command = CommandWrap::from(command);
    command.wrap(ProcessSession);
    Ok(command.spawn()?.into_inner())
}

pub(super) fn kill_group(session: Pid) -> io::Result<()> {
    match process::kill_process_group(session, Signal::KILL) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(io::Error::other(format!(
            "cannot signal process group: {error}"
        ))),
    }
}

pub(super) async fn kill(session: Pid) -> io::Result<()> {
    let group_result = kill_group(session);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let group_result = group_result.and(kill_remaining_members(session).await);
    group_result
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn kill_remaining_members(session: Pid) -> io::Result<()> {
    let mut command = Command::new("/bin/ps");
    command
        .args(["-A", "-o", "pid=,stat="])
        .env_clear()
        .kill_on_drop(true);
    let mut failure = None;
    for final_pass in [false, true] {
        let snapshot = process_snapshot(&mut command).await?;
        let processes = snapshot.lines();

        let mut found_live_member = false;
        for line in processes {
            let mut fields = line.split_ascii_whitespace();
            let Some(pid) = fields
                .next()
                .and_then(|pid| pid.parse().ok())
                .and_then(Pid::from_raw)
            else {
                continue;
            };
            if pid == session || fields.next().is_none_or(|status| status.starts_with('Z')) {
                continue;
            }
            match in_session(pid, session) {
                Ok(true) => found_live_member = true,
                Ok(false) => continue,
                Err(error) => {
                    failure.get_or_insert_with(|| {
                        io::Error::other(format!("cannot check session of PID {pid}: {error}"))
                    });
                    continue;
                }
            }
            if let Err(error) = kill_member(pid, session) {
                let disappeared = error.raw_os_error() == Some(Errno::SRCH.raw_os_error());
                if !disappeared || (final_pass && !matches!(in_session(pid, session), Ok(false))) {
                    failure.get_or_insert_with(|| {
                        io::Error::other(format!("cannot signal PID {pid}: {error}"))
                    });
                }
            }
        }
        if !found_live_member {
            break;
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn process_snapshot(command: &mut Command) -> io::Result<String> {
    let output = tokio::time::timeout(EXIT_STDIO_GRACE, command.output())
        .await
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("process scan timed out: {error}"),
            )
        })?
        .map_err(|error| io::Error::other(format!("cannot run /bin/ps: {error}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "/bin/ps exited with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| io::Error::other(format!("invalid /bin/ps output: {error}")))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn in_session(pid: Pid, session: Pid) -> io::Result<bool> {
    match unistd::getsid(Some(unistd::Pid::from_raw(pid.as_raw_pid()))) {
        Ok(actual) => Ok(actual.as_raw() == session.as_raw_pid()),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
fn kill_member(pid: Pid, session: Pid) -> io::Result<()> {
    let fd = process::pidfd_open(pid, process::PidfdFlags::empty())?;
    if in_session(pid, session)? {
        process::pidfd_send_signal(fd, Signal::KILL)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn kill_member(pid: Pid, session: Pid) -> io::Result<()> {
    let identity = ProcessIdentity::read(pid.as_raw_pid())?;
    if in_session(pid, session)? {
        identity.kill()?;
    }
    Ok(())
}

#[cfg(test)]
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod tests {
    use tokio::process::Command;

    use crate::jobs::process_session::process_snapshot;

    #[tokio::test]
    async fn process_scan_reports_nonzero_exit() {
        let error = process_snapshot(&mut Command::new("/usr/bin/false"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("/bin/ps exited with"), "{error}");
    }
}
