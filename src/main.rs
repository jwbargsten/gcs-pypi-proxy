//! gcs-pypi: serve a GCS bucket of Python distributions as a local PEP 503 "simple" index.
//!
//! The bucket needs no index files: wheels/sdists are dropped anywhere under the prefix and
//! the index is generated on the fly from the object listing. Auth uses the VM's metadata
//! server (Vertex Workbench / GCE / GKE), falling back to `gcloud` for local use.
//!
//!   gcs-pypi exec --bucket gs://my-pypi/packages -- pip install mylib
//!   gcs-pypi start --bucket gs://my-pypi/packages --daemon && eval "$(gcs-pypi env)"

mod auth;
mod daemon;
mod index;
mod pidfile;
mod server;
mod shellenv;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::Mutex,
};

use crate::{
    auth::{Auth, AuthMode},
    index::AppState,
};

#[derive(Parser, Debug)]
#[command(
    name = "gcs-pypi",
    version,
    about = "Serve a GCS bucket as a local Python package index for pip/uv",
    after_help = "All subcommands take the same options, so `env` and `stop` find the server that \
                  `start` with the same options runs."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Serve until interrupted (Ctrl-C, SIGTERM, `gcs-pypi stop`), or in the background with
    /// --daemon
    Start(Opts),
    /// Print a shell snippet that points pip and uv at a running server:
    /// eval "$(gcs-pypi env)"
    ///
    /// With a fixed --addr, that address. Otherwise the server recorded in the pid file, or
    /// 127.0.0.1:8765 (e.g. the systemd service) if none runs.
    Env(Opts),
    /// Stop the server recorded in the pid file
    Stop(Opts),
    /// Serve on a free port only while a command runs with PIP_INDEX_URL and UV_DEFAULT_INDEX
    /// set, and exit with its exit code
    Exec {
        #[command(flatten)]
        opts: Opts,

        /// Command to run with the index configured
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(clap::Args, Debug)]
struct Opts {
    /// Bucket and optional prefix, e.g. gs://my-pypi or gs://my-pypi/packages (required by
    /// start and exec)
    #[arg(long, env = "GCS_PYPI_BUCKET")]
    bucket: Option<String>,

    /// Address to listen on (port 0 picks a free port) [default: 127.0.0.1:8765, with exec
    /// a free port on 127.0.0.1]
    #[arg(long, env = "GCS_PYPI_ADDR")]
    addr: Option<SocketAddr>,

    /// Simple index to redirect to for projects NOT in the bucket ("none" to disable).
    /// Projects that exist in the bucket are never looked up upstream.
    #[arg(long, env = "GCS_PYPI_UPSTREAM", default_value = "https://pypi.org/simple/")]
    upstream: String,

    /// Seconds to cache the bucket listing
    #[arg(long, env = "GCS_PYPI_CACHE_TTL", default_value_t = 60)]
    cache_ttl: u64,

    /// Auth source
    #[arg(long, env = "GCS_PYPI_AUTH", value_enum, default_value = "auto")]
    auth: AuthMode,

    /// GCS API endpoint (override for emulators)
    #[arg(
        long,
        env = "GCS_PYPI_ENDPOINT",
        default_value = "https://storage.googleapis.com"
    )]
    endpoint: String,

    /// Log each request to stderr
    #[arg(long, short)]
    verbose: bool,

    /// With start: serve in the background, and return once the server is ready, printing its
    /// URL
    #[arg(long)]
    daemon: bool,

    /// Where the background server logs (with --daemon) [default: <temp dir>/gcs-pypi.log]
    #[arg(long, env = "GCS_PYPI_LOG_FILE")]
    log_file: Option<PathBuf>,

    /// The server's PID and URL, for env and stop; start refuses to run while that process
    /// does [default: $XDG_RUNTIME_DIR/gcs-pypi.pid or <temp dir>/gcs-pypi.pid, with exec
    /// none]
    #[arg(long, env = "GCS_PYPI_PID_FILE")]
    pid_file: Option<PathBuf>,
}

/// Where `start` listens by default, and where `env` points when no server is recorded.
const DEFAULT_ADDR: &str = "127.0.0.1:8765";

impl Opts {
    fn pid_file(&self) -> PathBuf {
        self.pid_file.clone().unwrap_or_else(|| {
            // Per user and cleared on logout, so a stale file outlives no session.
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|d| !d.is_empty())
                .map_or_else(std::env::temp_dir, PathBuf::from);
            dir.join("gcs-pypi.pid")
        })
    }

    /// The URL of the server that `start` with these options runs.
    fn server_url(&self) -> Result<String> {
        if let Some(addr) = self.addr.filter(|a| a.port() != 0) {
            return Ok(shellenv::index_url(addr));
        }
        let pid_file = self.pid_file();
        match pidfile::running(&pid_file) {
            Some(pidfile::Server { url: Some(url), .. }) => Ok(url),
            Some(s) => bail!("gcs-pypi (pid {}) is still starting, try again", s.pid),
            None if self.addr.is_none() => Ok(shellenv::index_url(DEFAULT_ADDR.parse()?)),
            None => bail!(
                "--addr has port 0, but no server runs (pid file {})",
                pid_file.display()
            ),
        }
    }
}

fn parse_bucket(spec: &str) -> Result<(String, String)> {
    let rest = spec.strip_prefix("gs://").unwrap_or(spec).trim_matches('/');
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if bucket.is_empty() {
        bail!("invalid --bucket {spec:?}, expected gs://bucket[/prefix]");
    }
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    Ok((bucket.to_string(), prefix))
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Start(opts) => {
            let pid_file = opts.pid_file();
            if opts.daemon && !daemon::is_child() {
                let log_file = opts
                    .log_file
                    .clone()
                    .unwrap_or_else(|| std::env::temp_dir().join("gcs-pypi.log"));
                // The child resolves the same pid file, from the same args and environment.
                return daemon::spawn(&log_file, &pid_file);
            }
            let addr = opts.addr.unwrap_or(DEFAULT_ADDR.parse()?);
            serve(&opts, addr, Some(&pid_file), &[]).await
        }
        Cmd::Env(opts) => {
            print!("{}", shellenv::snippet(&opts.server_url()?));
            Ok(())
        }
        Cmd::Stop(opts) => stop(&opts.pid_file()),
        // A free port and no pid file by default, so several can run at once.
        Cmd::Exec { opts, command } => {
            if opts.daemon {
                bail!("--daemon is for start; exec stops the server when the command exits");
            }
            let addr = opts.addr.unwrap_or(SocketAddr::from(([127, 0, 0, 1], 0)));
            serve(&opts, addr, opts.pid_file.as_deref(), &command).await
        }
    }
}

/// Send SIGTERM to the recorded server and wait for it to exit.
fn stop(pid_file: &Path) -> Result<()> {
    let Some(server) = pidfile::running(pid_file) else {
        bail!("no gcs-pypi running (pid file {})", pid_file.display());
    };
    pidfile::terminate(server.pid)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while pidfile::is_running(server.pid) {
        if Instant::now() > deadline {
            bail!("gcs-pypi (pid {}) did not stop within 10s", server.pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("gcs-pypi: stopped server (pid {})", server.pid);
    Ok(())
}

/// Serve until interrupted, or, with a command, until it exits.
async fn serve(opts: &Opts, addr: SocketAddr, pid_file: Option<&Path>, command: &[String]) -> Result<()> {
    let (bucket, prefix) = parse_bucket(opts.bucket.as_deref().context("--bucket is required")?)?;
    // Held until serve returns; removed on Ctrl-C, SIGTERM and failed starts.
    let pid_file = pid_file.map(pidfile::PidFile::create).transpose()?;
    let upstream = match opts.upstream.as_str() {
        "" | "none" => None,
        u => Some(if u.ends_with('/') {
            u.to_string()
        } else {
            format!("{u}/")
        }),
    };

    let state = Arc::new(AppState {
        http: reqwest::Client::builder().build()?,
        auth: Auth::new(opts.auth)?,
        endpoint: opts.endpoint.trim_end_matches('/').to_string(),
        bucket,
        prefix,
        upstream,
        ttl: Duration::from_secs(opts.cache_ttl),
        verbose: opts.verbose,
        listing: Mutex::new(None),
    });

    // Fail fast on auth/permission problems before pip/uv produce confusing errors.
    let n = state.index().await.context("initial bucket listing")?.len();

    let app = server::router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let url = shellenv::index_url(addr);
    if let Some(f) = &pid_file {
        f.set_url(&url)?;
    }
    eprintln!(
        "gcs-pypi: serving gs://{}/{} ({n} projects) at {url}",
        state.bucket, state.prefix
    );

    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    if command.is_empty() {
        if daemon::is_child() {
            // Tells the waiting parent the server is ready.
            println!("{url}");
        }
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            r = server => r??,
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        return Ok(());
    }

    let status = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .env("PIP_INDEX_URL", &url)
        .env("UV_DEFAULT_INDEX", &url)
        .env_remove("UV_INDEX_URL") // deprecated alias; would conflict with UV_DEFAULT_INDEX
        .status()
        .await
        .with_context(|| format!("running {:?}", command[0]))?;
    // exit skips destructors.
    drop(pid_file);
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use rstest::rstest;

    use super::*;

    fn opts(argv: &[&str]) -> Opts {
        match Cli::try_parse_from([&["gcs-pypi", "env"], argv].concat())
            .unwrap()
            .cmd
        {
            Cmd::Env(o) => o,
            cmd => panic!("{cmd:?}"),
        }
    }

    #[rstest]
    #[case("gs://b", "b", "")]
    #[case("gs://b/", "b", "")]
    #[case("gs://b/a/c/", "b", "a/c/")]
    #[case("gs://b/a/c", "b", "a/c/")]
    #[case("b/pkgs", "b", "pkgs/")]
    fn parse_bucket_ok(#[case] spec: &str, #[case] bucket: &str, #[case] prefix: &str) {
        assert_eq!(parse_bucket(spec).unwrap(), (bucket.into(), prefix.into()));
    }

    #[rstest]
    #[case("")]
    #[case("gs://")]
    #[case("gs:///")]
    fn parse_bucket_err(#[case] spec: &str) {
        assert!(parse_bucket(spec).is_err());
    }

    #[test]
    fn args_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[rstest]
    #[case(&[], AuthMode::Auto)]
    #[case(&["--auth", "metadata"], AuthMode::Metadata)]
    #[case(&["--auth", "gcloud"], AuthMode::Gcloud)]
    #[case(&["--auth", "none"], AuthMode::None)]
    fn auth_flag(#[case] extra: &[&str], #[case] expected: AuthMode) {
        assert_eq!(opts(extra).auth, expected);
    }

    #[test]
    fn auth_flag_rejects_unknown() {
        assert!(Cli::try_parse_from(["gcs-pypi", "start", "--auth", "bogus"]).is_err());
    }

    #[rstest]
    #[case("start")]
    #[case("env")]
    #[case("stop")]
    fn subcommands_take_the_same_options(#[case] cmd: &str) {
        let argv = [
            "gcs-pypi",
            cmd,
            "--bucket",
            "gs://b",
            "--addr",
            "127.0.0.1:0",
            "--upstream",
            "none",
            "--cache-ttl",
            "5",
            "--auth",
            "none",
            "--endpoint",
            "http://e",
            "-v",
            "--daemon",
            "--log-file",
            "l",
            "--pid-file",
            "p",
        ];
        Cli::try_parse_from(argv).unwrap();
    }

    #[test]
    fn exec_needs_a_command() {
        assert!(Cli::try_parse_from(["gcs-pypi", "exec", "--bucket", "gs://b"]).is_err());
        let cli = Cli::try_parse_from(["gcs-pypi", "exec", "--bucket", "gs://b", "--", "pip", "-V"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Exec { command, .. } if command == ["pip", "-V"]));
    }

    #[test]
    fn start_takes_no_command() {
        assert!(Cli::try_parse_from(["gcs-pypi", "start", "--bucket", "gs://b", "--", "pip"]).is_err());
    }

    #[test]
    fn a_subcommand_is_required() {
        assert!(Cli::try_parse_from(["gcs-pypi", "--bucket", "gs://b"]).is_err());
    }

    #[test]
    fn default_addr_parses() {
        DEFAULT_ADDR.parse::<SocketAddr>().unwrap();
    }

    fn temp_pid_file(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("gcs-pypi-main-{}-{name}.pid", std::process::id()));
        path.to_str().unwrap().to_string()
    }

    #[rstest]
    #[case(&["--addr", "[::1]:9000"], "http://[::1]:9000/simple/")]
    #[case(&[], "http://127.0.0.1:8765/simple/")]
    fn env_url_without_a_server(#[case] extra: &[&str], #[case] expected: &str) {
        let pid_file = temp_pid_file("none");
        let o = opts(&[&["--pid-file", &pid_file], extra].concat());
        assert_eq!(o.server_url().unwrap(), expected);
    }

    #[test]
    fn env_url_port_0_needs_a_server() {
        let pid_file = temp_pid_file("port0");
        assert!(
            opts(&["--pid-file", &pid_file, "--addr", "127.0.0.1:0"])
                .server_url()
                .is_err()
        );
    }

    #[rstest]
    #[case(&[])]
    #[case(&["--addr", "127.0.0.1:0"])]
    fn env_url_from_the_pid_file(#[case] extra: &[&str]) {
        let pid_file = temp_pid_file(&format!("recorded{}", extra.len()));
        let f = pidfile::PidFile::create(Path::new(&pid_file)).unwrap();
        f.set_url("http://127.0.0.1:4242/simple/").unwrap();
        let o = opts(&[&["--pid-file", &pid_file], extra].concat());
        assert_eq!(o.server_url().unwrap(), "http://127.0.0.1:4242/simple/");
    }
}
