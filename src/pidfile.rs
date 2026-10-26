//! `--pid-file`: record the serving process's PID, and refuse to start while another one runs.

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
                    if let Some(other) = read(path).filter(|&p| is_running(p)) {
                        bail!(
                            "gcs-pypi is already running (pid {other}, pid file {})",
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
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if read(&self.path) == Some(self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn read(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 only checks the process exists; EPERM means it does, under another user.
    // SAFETY: kill with signal 0 sends nothing.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gcs-pypi-test-{}-{name}.pid", std::process::id()))
    }

    #[test]
    fn writes_and_removes() {
        let path = temp_path("writes");
        let f = PidFile::create(&path).unwrap();
        assert_eq!(read(&path), Some(std::process::id()));
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
        fs::write(&path, "999999999\n").unwrap();
        let f = PidFile::create(&path).unwrap();
        assert_eq!(read(&path), Some(std::process::id()));
        drop(f);
    }

    #[test]
    fn keeps_a_file_taken_over() {
        let path = temp_path("taken");
        let f = PidFile::create(&path).unwrap();
        fs::write(&path, "1\n").unwrap();
        drop(f);
        assert_eq!(read(&path), Some(1));
        fs::remove_file(&path).unwrap();
    }
}
