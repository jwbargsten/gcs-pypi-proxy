//! `--pid-file`: record the serving process's PID and URL, and refuse to start while another one
//! runs. Like PostgreSQL's postmaster.pid, the PID is the first line, so `env` and `stop` find
//! the server, including one on a random port.
//!
//!   <pid>
//!   <index url>     (once the server listens)

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

/// Removes the file again when dropped, unless another process has taken it over by then.
pub struct PidFile {
    path: PathBuf,
    pid: u32,
}

/// A running server, as its pid file records it.
#[derive(Debug, PartialEq)]
pub struct Server {
    pub pid: u32,
    /// None while the server is still starting.
    pub url: Option<String>,
}

impl PidFile {
    /// Write this process's PID to `path`. A file left behind by a process that is gone is
    /// replaced; one whose process is still running is an error.
    pub fn create(path: &Path) -> Result<Self> {
        let pid = std::process::id();
        // Two tries: the second follows removing a stale file.
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut f) => {
                    writeln!(f, "{pid}").with_context(|| format!("writing pid file {}", path.display()))?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        pid,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if let Some(other) = running(path) {
                        bail!(
                            "gcs-pypi is already running (pid {}, pid file {})",
                            other.pid,
                            path.display()
                        );
                    }
                    fs::remove_file(path)
                        .with_context(|| format!("removing stale pid file {}", path.display()))?;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating pid file {}", path.display()));
                }
            }
        }
        bail!("pid file {} keeps reappearing", path.display())
    }

    /// Record the URL the server listens on. Replaces the file in one step, so readers never see
    /// half of it.
    pub fn set_url(&self, url: &str) -> Result<()> {
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        fs::write(&tmp, format!("{}\n{url}\n", self.pid))
            .and_then(|()| fs::rename(&tmp, &self.path))
            .with_context(|| format!("writing pid file {}", self.path.display()))
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if read(&self.path).is_some_and(|s| s.pid == self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The server recorded in `path`, if its process still runs.
pub fn running(path: &Path) -> Option<Server> {
    read(path).filter(|s| is_running(s.pid))
}

fn read(path: &Path) -> Option<Server> {
    let content = fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    let url = lines
        .next()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(String::from);
    Some(Server { pid, url })
}

pub fn is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 only checks the process exists; EPERM means it does, under another user.
    // SAFETY: kill with signal 0 sends nothing.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

pub fn terminate(pid: u32) -> Result<()> {
    let pid = libc::pid_t::try_from(pid).context("pid out of range")?;
    // SAFETY: plain syscall; pid comes from a pid file and was just checked to be running.
    if unsafe { libc::kill(pid, libc::SIGTERM) } == -1 {
        return Err(io::Error::last_os_error()).with_context(|| format!("stopping pid {pid}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gcs-pypi-test-{}-{name}.pid", std::process::id()))
    }

    fn me(url: Option<&str>) -> Option<Server> {
        Some(Server {
            pid: std::process::id(),
            url: url.map(String::from),
        })
    }

    #[test]
    fn writes_and_removes() {
        let path = temp_path("writes");
        let f = PidFile::create(&path).unwrap();
        assert_eq!(read(&path), me(None));
        drop(f);
        assert!(!path.exists());
    }

    #[test]
    fn records_the_url() {
        let path = temp_path("url");
        let f = PidFile::create(&path).unwrap();
        f.set_url("http://127.0.0.1:1/simple/").unwrap();
        assert_eq!(running(&path), me(Some("http://127.0.0.1:1/simple/")));
        drop(f);
        assert!(!path.exists());
    }

    #[test]
    fn refuses_while_running() {
        let path = temp_path("running");
        let _f = PidFile::create(&path).unwrap();
        let err = PidFile::create(&path).err().unwrap();
        assert!(err.to_string().contains("already running"), "{err}");
    }

    #[test]
    fn replaces_stale() {
        let path = temp_path("stale");
        // Above any real pid_max, so no process has it.
        fs::write(&path, "999999999\nhttp://old/\n").unwrap();
        assert_eq!(running(&path), None);
        let f = PidFile::create(&path).unwrap();
        assert_eq!(read(&path), me(None));
        drop(f);
    }

    #[test]
    fn keeps_a_file_taken_over() {
        let path = temp_path("taken");
        let f = PidFile::create(&path).unwrap();
        fs::write(&path, "1\n").unwrap();
        drop(f);
        assert_eq!(read(&path).map(|s| s.pid), Some(1));
        fs::remove_file(&path).unwrap();
    }
}
