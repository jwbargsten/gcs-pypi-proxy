//! `start --daemon`: re-run this binary detached in its own session and return once it serves.

use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Seek, SeekFrom},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

/// Set for the detached child, so it serves in the foreground instead of spawning again.
const CHILD_ENV: &str = "GCS_PYPI_DAEMON_CHILD";

pub fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// Start the server in the background and print its URL once it is ready. The child writes
/// `pid_file` itself. If it fails to start, its log output is copied to stderr and an error is returned.
pub fn spawn(log_file: &Path, pid_file: &Path) -> Result<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .with_context(|| format!("opening log file {}", log_file.display()))?;
    let log_start = log.metadata()?.len();

    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(std::env::args_os().skip(1))
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // Own session without a controlling terminal: closing the shell must not SIGHUP the server.
    // SAFETY: setsid is async-signal-safe, so it may run between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting background server")?;

    // The child prints its index URL once it serves; EOF means it gave up.
    let stdout = child.stdout.take().context("background server has no stdout")?;
    let Some(url) = BufReader::new(stdout).lines().next().transpose()? else {
        let status = child.wait()?;
        let mut log = File::open(log_file)?;
        log.seek(SeekFrom::Start(log_start))?;
        io::copy(&mut log, &mut io::stderr())?;
        bail!(
            "background server failed to start ({status}), log: {}",
            log_file.display()
        );
    };

    println!("gcs-pypi: started server at {url}");
    eprintln!(
        "gcs-pypi: running in background (pid {}; stop it with `gcs-pypi stop`), pid file: {}, log: {}",
        child.id(),
        pid_file.display(),
        log_file.display()
    );
    Ok(())
}
