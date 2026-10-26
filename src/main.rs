//! gcs-pypi: serve a GCS bucket of Python distributions as a local PEP 503 "simple" index.
//!
//! The bucket needs no index files: wheels/sdists are dropped anywhere under the prefix and
//! the index is generated on the fly from the object listing. Auth uses the VM's metadata
//! server (Vertex Workbench / GCE / GKE), falling back to `gcloud` for local use.
//!
//!   gcs-pypi --bucket gs://my-pypi/packages -- pip install mylib
//!   gcs-pypi --bucket gs://my-pypi/packages -- uv sync

mod auth;
mod daemon;
mod index;
mod pidfile;
mod server;
mod shellenv;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

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
    after_help = "With a command after `--`, the server starts on a free port, PIP_INDEX_URL and \
                  UV_DEFAULT_INDEX are set for the command, and the server stops when it exits.\n\
                  Without a command, the server runs until interrupted, or in the background with \
                  --daemon; point the shell at it with `eval \"$(gcs-pypi env)\"`.",
    subcommand_negates_reqs = true
)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Bucket and optional prefix, e.g. gs://my-pypi or gs://my-pypi/packages
    #[arg(long, env = "GCS_PYPI_BUCKET", required = true)]
    bucket: Option<String>,

    /// Address to listen on (port 0 picks a free port) [default: 127.0.0.1:8765, or a free
    /// port on 127.0.0.1 with a command]
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

    /// Serve in the background and return once the server is ready, printing its URL (stop it
    /// with `kill "$(cat <pid file>)"`)
    #[arg(long, conflicts_with = "command")]
    daemon: bool,

    /// Where the background server logs (with --daemon) [default: <temp dir>/gcs-pypi.log]
    #[arg(long, env = "GCS_PYPI_LOG_FILE")]
    log_file: Option<PathBuf>,

    /// Write the server's PID here, and refuse to start while that process runs [default with
    /// --daemon: <temp dir>/gcs-pypi.pid, otherwise none]
    #[arg(long, env = "GCS_PYPI_PID_FILE", conflicts_with = "command")]
    pid_file: Option<PathBuf>,

    /// Command to run with the index configured
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print a shell snippet that points pip and uv at an already running server, e.g. the
    /// systemd service: eval "$(gcs-pypi env)"
    Env {
        /// Address the server listens on
        #[arg(long, env = "GCS_PYPI_ADDR", default_value = DEFAULT_ADDR)]
        addr: SocketAddr,
    },
}

/// Where a server without a command listens, and where `env` points.
const DEFAULT_ADDR: &str = "127.0.0.1:8765";

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
    let args = Args::parse();
    if let Some(Cmd::Env { addr }) = args.cmd {
        if addr.port() == 0 {
            bail!("env: --addr needs the server's port, not 0");
        }
        print!("{}", shellenv::snippet(&shellenv::index_url(addr)));
        return Ok(());
    }
    // Required by clap unless a subcommand is given.
    let (bucket, prefix) = parse_bucket(args.bucket.as_deref().context("--bucket is required")?)?;
    let addr = match args.addr {
        Some(a) => a,
        None if args.command.is_empty() => DEFAULT_ADDR.parse()?,
        None => "127.0.0.1:0".parse()?,
    };
    let pid_file = match args.pid_file {
        Some(p) => Some(p),
        None if args.daemon => Some(std::env::temp_dir().join("gcs-pypi.pid")),
        None => None,
    };
    if args.daemon && !daemon::is_child() {
        let log_file = args
            .log_file
            .unwrap_or_else(|| std::env::temp_dir().join("gcs-pypi.log"));
        // The child resolves the same default pid file, from the same args and environment.
        return daemon::spawn(&log_file, pid_file.as_deref().context("--daemon has a pid file")?);
    }
    // Held until main returns; removed on Ctrl-C, SIGTERM and failed starts.
    let _pid_file = pid_file.as_deref().map(pidfile::PidFile::create).transpose()?;
    let upstream = match args.upstream.as_str() {
        "" | "none" => None,
        u => Some(if u.ends_with('/') {
            u.to_string()
        } else {
            format!("{u}/")
        }),
    };

    let state = Arc::new(AppState {
        http: reqwest::Client::builder().build()?,
        auth: Auth::new(args.auth)?,
        endpoint: args.endpoint.trim_end_matches('/').to_string(),
        bucket,
        prefix,
        upstream,
        ttl: Duration::from_secs(args.cache_ttl),
        verbose: args.verbose,
        listing: Mutex::new(None),
    });

    // Fail fast on auth/permission problems before pip/uv produce confusing errors.
    let n = state.index().await.context("initial bucket listing")?.len();

    let app = server::router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let url = shellenv::index_url(addr);
    eprintln!(
        "gcs-pypi: serving gs://{}/{} ({n} projects) at {url}",
        state.bucket, state.prefix
    );

    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    if args.command.is_empty() {
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

    let status = tokio::process::Command::new(&args.command[0])
        .args(&args.command[1..])
        .env("PIP_INDEX_URL", &url)
        .env("UV_DEFAULT_INDEX", &url)
        .env_remove("UV_INDEX_URL") // deprecated alias; would conflict with UV_DEFAULT_INDEX
        .status()
        .await
        .with_context(|| format!("running {:?}", args.command[0]))?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use rstest::rstest;

    use super::*;

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
        Args::command().debug_assert();
    }

    #[rstest]
    #[case(&[], AuthMode::Auto)]
    #[case(&["--auth", "metadata"], AuthMode::Metadata)]
    #[case(&["--auth", "gcloud"], AuthMode::Gcloud)]
    #[case(&["--auth", "none"], AuthMode::None)]
    fn auth_flag(#[case] extra: &[&str], #[case] expected: AuthMode) {
        let argv = [&["gcs-pypi", "--bucket", "gs://b"], extra].concat();
        assert_eq!(Args::try_parse_from(argv).unwrap().auth, expected);
    }

    #[rstest]
    #[case(&["gcs-pypi", "env"], "127.0.0.1:8765")]
    #[case(&["gcs-pypi", "env", "--addr", "[::1]:9000"], "[::1]:9000")]
    fn env_needs_no_bucket(#[case] argv: &[&str], #[case] expected: SocketAddr) {
        let args = Args::try_parse_from(argv).unwrap();
        assert!(matches!(args.cmd, Some(Cmd::Env { addr }) if addr == expected));
    }

    #[test]
    fn serving_needs_bucket() {
        assert!(Args::try_parse_from(["gcs-pypi"]).is_err());
    }

    #[test]
    fn pid_file_conflicts_with_command() {
        assert!(
            Args::try_parse_from(["gcs-pypi", "--bucket", "gs://b", "--pid-file", "p", "--", "pip"]).is_err()
        );
    }

    #[test]
    fn default_addr_parses() {
        DEFAULT_ADDR.parse::<SocketAddr>().unwrap();
    }

    #[test]
    fn daemon_conflicts_with_command() {
        assert!(Args::try_parse_from(["gcs-pypi", "--bucket", "gs://b", "--daemon", "--", "pip"]).is_err());
    }

    #[test]
    fn auth_flag_rejects_unknown() {
        assert!(Args::try_parse_from(["gcs-pypi", "--bucket", "gs://b", "--auth", "bogus"]).is_err());
    }
}
