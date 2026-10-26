# gcs-pypi

A ~3 MB static Rust binary that turns a GCS bucket of wheels/sdists into a local
PEP 503 index for **pip** and **uv**. It is meant to start right before an install
and stop right after.

```bash
gcs-pypi exec --bucket gs://my-pypi/packages -- pip install my-private-lib
gcs-pypi exec --bucket gs://my-pypi/packages -- uv sync
```

## How it works

```
pip / uv ──HTTP──▶ 127.0.0.1:<random>/simple/  (gcs-pypi)
                     │  project in bucket?  ── yes ─▶ HTML page generated from the bucket listing
                     │                       ── no ──▶ 307 redirect to https://pypi.org/simple/<name>/
                     └─ /files/<object> ─────────────▶ streamed from GCS with the VM's token
```

- **No index files in the bucket.** Drop `.whl` / `.tar.gz` / `.zip` files anywhere under
  the prefix. The listing is the index, so nothing needs regenerating and nothing goes stale.
- **Auth comes from the VM.** The token is fetched from the metadata server, which works on
  Vertex Workbench, GCE, GKE and Cloud Run. It is cached and refreshed before it expires.
  `--auth auto` falls back to `gcloud auth print-access-token` for laptops.
- **No dependency confusion.** A project that exists in the bucket is served *only* from
  the bucket and is never looked up upstream. Unknown projects are redirected to
  `--upstream`, which defaults to PyPI. Use `--upstream none` for air-gapped VMs, or set it
  to your mirror.
- **Hashes.** Every link carries `#md5=` from GCS metadata, so pip and uv verify each
  download. If an object has `x-goog-meta-sha256` metadata, that hash is used instead.
  `x-goog-meta-requires_python` becomes `data-requires-python`.
- **Fails fast.** The bucket is listed before the wrapped command starts. Missing
  permissions show up as one clear error rather than as a pip "no matching distribution".

## Usage

| Mode | Command |
|---|---|
| Wrap a command (recommended) | `gcs-pypi exec --bucket gs://b/prefix -- <cmd...>` serves on a free port only while `<cmd>` runs, sets `PIP_INDEX_URL` and `UV_DEFAULT_INDEX` for `<cmd>`, then exits with its exit code |
| Long-running | `gcs-pypi start --bucket gs://b/prefix` serves on `127.0.0.1:8765` until Ctrl-C, SIGTERM or `gcs-pypi stop` |
| Background | `gcs-pypi start --bucket gs://b/prefix --daemon` returns once the server is ready and prints its URL |
| Point the shell at a running server | `eval "$(gcs-pypi env)"` exports `PIP_INDEX_URL` and `UV_DEFAULT_INDEX` |
| Stop it | `gcs-pypi stop` sends SIGTERM and waits until the server has exited |
| systemd service | see [Running as a systemd service](#running-as-a-systemd-service) |

All subcommands take the same options, so replacing `start` with `env` or `stop` in a command line
finds the server that command started. `env` uses a fixed `--addr` as given. Otherwise it reads
the server's URL from the pid file, which works for a random port too, and falls back to
`127.0.0.1:8765` (e.g. the systemd service) if no server is recorded there.

```bash
opts=(--bucket gs://b/prefix --addr 127.0.0.1:0)
gcs-pypi start --daemon "${opts[@]}"
eval "$(gcs-pypi env "${opts[@]}")"
pip install my-private-lib
gcs-pypi stop "${opts[@]}"
```

| Flag / env | Default | |
|---|---|---|
| `--bucket` / `GCS_PYPI_BUCKET` | — | `gs://bucket[/prefix]`, required by `start` and `exec` |
| `--upstream` / `GCS_PYPI_UPSTREAM` | `https://pypi.org/simple/` | `none` disables |
| `--auth` / `GCS_PYPI_AUTH` | `auto` | `metadata`, `gcloud`, `none` |
| `--cache-ttl` / `GCS_PYPI_CACHE_TTL` | `60` | seconds the listing is cached |
| `--addr` / `GCS_PYPI_ADDR` | `127.0.0.1:8765`; with `exec` `127.0.0.1:0` | port 0 = random free port |
| `--endpoint` / `GCS_PYPI_ENDPOINT` | `https://storage.googleapis.com` | for emulators |
| `-v` | off | log fetches and upstream redirects |
| `--daemon` | off | `start` in the background |
| `--log-file` / `GCS_PYPI_LOG_FILE` | `<temp dir>/gcs-pypi.log` | log of the background server; if it fails to start, the error is also printed |
| `--pid-file` / `GCS_PYPI_PID_FILE` | `$XDG_RUNTIME_DIR/gcs-pypi.pid`, or `<temp dir>/gcs-pypi.pid` without it; with `exec` none | the server's PID (line 1) and URL (line 2), removed when it stops; `env` and `stop` read it, `start` fails while that process runs |

## Publishing

```bash
python -m build    # or: uv build
for f in dist/*; do
  gcloud storage cp "$f" gs://my-pypi/packages/ \
    --custom-metadata=sha256=$(sha256sum "$f" | cut -d' ' -f1) \
    --if-generation-match=0      # refuse to overwrite an existing version
done
```

The `sha256` metadata is optional; md5 works without it.

## Setup on Vertex Workbench / GCE

1. Grant the VM's service account `roles/storage.objectViewer` on the bucket.
2. Make sure the VM has the `cloud-platform` scope. Workbench instances have it by default.
3. Ship the binary in your image, or `gcloud storage cp` it from a bucket in a startup
   script. Build it once with `make dist`, which produces a fully static binary (see
   [Development](#development)). Or install the wheel, see below.

Example Dockerfile or startup step:

```dockerfile
RUN gcs-pypi exec --bucket gs://my-pypi/packages -- pip install --no-cache-dir -r requirements.txt
```

## Install from a wheel

`make wheel` builds one wheel per platform, e.g.
`gcs_pypi_proxy-<version>-py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64.whl`:

| Platform | Wheel tag |
|---|---|
| Linux x86_64 / aarch64, glibc 2.17+ | `manylinux_2_17_{x86_64,aarch64}` |
| Linux x86_64 / aarch64, musl (Alpine) | `musllinux_1_2_{x86_64,aarch64}` |
| macOS Intel, 10.12+ | `macosx_10_12_x86_64` |
| macOS Apple silicon, 11+ | `macosx_11_0_arm64` |

A wheel contains only the binary, so it works with any Python version. Upload all of them to the
same place and pip or uv picks the one that matches the machine. The distribution is called
`gcs-pypi-proxy`, because `gcs-pypi` on PyPI is an unrelated project; the command is still
`gcs-pypi`.

In a running container:

```bash
gcloud storage cp 'gs://my-pypi/tools/gcs_pypi_proxy-0.3.0-*.whl' wheels/
pip install --no-index --find-links wheels/ gcs-pypi-proxy==0.3.0
# or: uv tool install --no-index --find-links wheels/ gcs-pypi-proxy==0.3.0

gcs-pypi start --bucket gs://my-pypi/packages --daemon
eval "$(gcs-pypi env)"
pip install my-private-lib
```

## Running as a systemd service

`systemd/gcs-pypi.service` runs the server in the foreground on the fixed address
`127.0.0.1:8765`, with metadata-server auth and a dynamic, unprivileged user:

```bash
sudo install -m 0755 gcs-pypi /usr/local/bin/gcs-pypi
sudo install -m 0644 systemd/gcs-pypi.service /etc/systemd/system/
echo 'GCS_PYPI_BUCKET=gs://my-pypi/packages' | sudo tee /etc/default/gcs-pypi
sudo systemctl daemon-reload
sudo systemctl enable --now gcs-pypi
journalctl -u gcs-pypi -f
```

Then point pip and uv at it, either per shell, e.g. in `~/.bashrc`:

```bash
eval "$(gcs-pypi env)"
```

or for everyone, in `/etc/pip.conf` and `/etc/uv/uv.toml`:

```ini
[global]
index-url = http://127.0.0.1:8765/simple/
```

```toml
[[index]]
url = "http://127.0.0.1:8765/simple/"
default = true
```

## Testing locally without GCP

`--auth none --endpoint http://localhost:4443` works against
[fake-gcs-server](https://github.com/fsouza/fake-gcs-server).

## Development

`make` lists the targets:

| Target | |
|---|---|
| `make build` | debug build |
| `make test` | run the unit tests |
| `make fmt` | apply `clippy --fix`, then `cargo fmt` |
| `make lint` | `cargo fmt --check` and `cargo clippy -D warnings`; fails on any warning |
| `make dist` | `clean`, `lint`, `test`, then optimized binaries copied to `dist/gcs-pypi-<commit>-<target>` |
| `make wheel` | `clean`, `lint`, `test`, then the wheels in `dist/` |
| `make dist-all` | `clean`, `lint`, `test`, then the binaries and the wheels in `dist/` |
| `make clean` | `cargo clean` and remove `dist/` |

Both build for Linux x86_64 and aarch64 and for macOS x86_64 and arm64. `make dist` builds
fully static musl binaries for Linux, so one binary per architecture runs on any distro.
`make wheel` builds manylinux2014 and musllinux_1_2 wheels for Linux and native macOS wheels.

Linux targets are cross-compiled with zig (cargo-zigbuild for `dist`, maturin for `wheel`) on
Linux and macOS hosts alike. macOS targets link against the macOS SDK, so they build only on a
Mac. Elsewhere, pick the targets yourself:

```bash
make dist DIST_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"
make wheel WHEEL_TARGETS="x86_64-unknown-linux-gnu"
```

Only `uv` needs to be installed: `uvx` fetches cargo-zigbuild, maturin and zig at the versions
pinned in the Makefile. `rust-toolchain.toml` lists all targets, so rustup installs them on the
first build. `make dist` and `make wheel` both start with `make clean`, which empties `dist/`;
run `make dist-all` to get the binaries and the wheels together.

The version comes from `Cargo.toml`; bump it there before building a wheel to hand out.

The toolchain (stable, with rustfmt and clippy) is pinned in `rust-toolchain.toml`. To skip the
reformat commit in `git blame`, run `git config blame.ignoreRevsFile .git-blame-ignore-revs`.
