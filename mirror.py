#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
"""Copy Python packages from PyPI into a gcs-pypi bucket, e.g. for VMs that cannot reach PyPI.

    ./mirror.py --bucket gs://my-pypi/packages --python-version 3.12 \\
      --platform x86_64-manylinux_2_31 requirements.txt

Downloads every package in a requirements file, including its dependencies, or every package in
./uv.lock (--uv), and uploads the files that the bucket does not have yet.

Note: once a project is in the bucket, gcs-pypi serves it only from the bucket. After mirroring
requests==2.32.3, pip and uv see no other version of requests.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import subprocess as sp
import sys
import tempfile
import urllib.request
import zipfile
from pathlib import Path

PIP = ["uvx", "pip@26.2.2"]

# The manylinux tags from before PEP 600, and the glibc version each one stands for
LEGACY_MANYLINUX = {17: "manylinux2014", 12: "manylinux2010", 5: "manylinux1"}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument(
        "requirements", nargs="?", type=Path, help="a requirements file"
    )
    source.add_argument("--uv", action="store_true", help="the packages in ./uv.lock")
    parser.add_argument(
        "--bucket",
        required=True,
        type=parse_bucket,
        help="where to upload to, gs://bucket[/prefix]",
    )
    parser.add_argument(
        "--python-version",
        required=True,
        type=parse_python_version,
        help="Python version of the machines that install from the bucket, e.g. 3.12",
    )
    parser.add_argument(
        "--platform",
        required=True,
        action="append",
        dest="platforms",
        metavar="PLATFORM",
        type=parse_platform,
        help="their platform, e.g. x86_64-manylinux_2_31 for Debian 11 (Vertex Workbench); "
        "repeat it for several platforms. Supported: {x86_64,aarch64}-manylinux_2_NN "
        "and -manylinux2014",
    )
    parser.add_argument("--dest", type=Path, help="keep the downloaded files in DEST")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="download, but only list what would be uploaded",
    )
    args = parser.parse_args()

    if args.requirements and not args.requirements.is_file():
        parser.error(f"{args.requirements} not found")
    if args.uv and not Path("uv.lock").is_file():
        parser.error(f"no uv.lock in {Path.cwd()}")
    if not shutil.which("uv"):
        parser.error("uv is not installed")
    if not args.dry_run and not shutil.which("gcloud"):
        parser.error("gcloud is not installed")
    return args


def parse_bucket(value: str) -> str:
    if not re.fullmatch(r"gs://[^/]+(/.*)?", value):
        raise argparse.ArgumentTypeError("expected gs://bucket[/prefix]")
    return value.rstrip("/")


def parse_python_version(value: str) -> str:
    if not re.fullmatch(r"3\.\d+", value):
        raise argparse.ArgumentTypeError("expected e.g. 3.12")
    return value


def parse_platform(value: str) -> str:
    pip_platform_tags(value)
    return value


def pip_platform_tags(platform: str) -> list[str]:
    """The pip platform tags of all wheels that a uv platform can install.

    uv and pip spell platforms differently, and pip only takes a wheel whose tag matches
    exactly. A machine with glibc 2.31 also installs manylinux_2_28 wheels, so
    x86_64-manylinux_2_31 becomes manylinux_2_31_x86_64, manylinux_2_30_x86_64, ... down to
    manylinux_2_5_x86_64, plus manylinux2014_x86_64, manylinux2010_x86_64 and
    manylinux1_x86_64.
    """
    match = re.fullmatch(r"(x86_64|aarch64)-manylinux(?:_2_(\d+)|2014)", platform)
    if not match:
        raise argparse.ArgumentTypeError("expected e.g. x86_64-manylinux_2_31")
    arch = match.group(1)
    glibc_minor = int(match.group(2) or 17)  # manylinux2014 is glibc 2.17

    tags = []
    for minor in range(glibc_minor, 4, -1):
        tags.append(f"manylinux_2_{minor}_{arch}")
        if minor in LEGACY_MANYLINUX:
            tags.append(f"{LEGACY_MANYLINUX[minor]}_{arch}")
    return tags


def export_uv_lock(output: Path) -> None:
    """Writes the packages in ./uv.lock as a requirements file, leaving out the project itself."""
    sp.run(
        [
            "uv", "export", "--quiet", "--frozen", "--format", "requirements.txt",
            "--all-extras", "--all-groups",
            "--no-emit-workspace", "--no-emit-local",
            "--output-file", output,
        ],
        check=True,
    )  # fmt: skip


def resolve(requirements: Path, platform: str, python_version: str) -> list[str]:
    """Pins every package and dependency for the target platform.

    Resolving on this machine instead would pick the dependencies for macOS, e.g. skip the ones
    marked sys_platform == "linux".
    """
    result = sp.run(
        [
            "uv", "pip", "compile", "--quiet", "--no-header", "--no-annotate",
            "--python-platform", platform,
            "--python-version", python_version,
            requirements,
        ],
        check=True,
        capture_output=True,
        text=True,
    )  # fmt: skip
    lines = (line.strip() for line in result.stdout.splitlines())
    return [line for line in lines if line and not line.startswith("#")]


def download(requirement: str, platform: str, python_version: str, dest: Path) -> bool:
    """Downloads the wheel of one pinned package, or its sdist if it has no wheel for the platform.

    pip runs once per package so that one missing wheel does not fail the rest.
    """
    platforms = [f"--platform={tag}" for tag in pip_platform_tags(platform)]
    wheel = sp.run(
        [
            *PIP, "download", "--quiet", "--no-deps", "--dest", dest,
            "--only-binary=:all:",
            *platforms,
            "--python-version", python_version,
            requirement,
        ],
        check=False,
        capture_output=True,
        text=True,
    )  # fmt: skip
    if wheel.returncode == 0:
        return True

    print(f"    no wheel for {platform}, downloading the sdist")
    if download_sdist(requirement, dest):
        return True
    print(wheel.stderr.strip(), file=sys.stderr)
    return False


def download_sdist(requirement: str, dest: Path) -> bool:
    """Downloads the sdist of a pinned package, e.g. "pyyaml==6.0.3", from PyPI.

    Not with pip download --no-binary, because pip would also build the package's metadata,
    which takes minutes for e.g. numpy.
    """
    match = re.fullmatch(r"([A-Za-z0-9._-]+)(?:\[.*\])?==([^\s;]+)", requirement)
    if not match:
        print(f"    {requirement} is not pinned with ==", file=sys.stderr)
        return False
    name, version = match.groups()

    with urllib.request.urlopen(
        f"https://pypi.org/pypi/{name}/{version}/json"
    ) as response:
        files = json.load(response)["urls"]
    sdists = [f for f in files if f["packagetype"] == "sdist"]
    if not sdists:
        print(f"    {name} {version} has no sdist either", file=sys.stderr)
        return False

    sdist = sdists[0]
    with urllib.request.urlopen(sdist["url"]) as response:
        content = response.read()
    if hashlib.sha256(content).hexdigest() != sdist["digests"]["sha256"]:
        print(f"    sha256 of {sdist['filename']} does not match PyPI", file=sys.stderr)
        return False
    (dest / sdist["filename"]).write_bytes(content)
    return True


def files_in_bucket(bucket: str, dry_run: bool) -> set[str]:
    if dry_run and not shutil.which("gcloud"):
        print("gcloud is not installed, assuming the bucket is empty")
        return set()
    # ls fails on an empty bucket, which is fine
    result = sp.run(
        ["gcloud", "storage", "ls", f"{bucket}/**"],
        check=False,
        capture_output=True,
        text=True,
    )
    return {url.rsplit("/", 1)[-1] for url in result.stdout.splitlines()}


def requires_python_of(file: Path) -> str | None:
    """The Requires-Python of a wheel, e.g. ">=3.8", or None."""
    if file.suffix != ".whl":
        return None
    with zipfile.ZipFile(file) as wheel:
        name = next(n for n in wheel.namelist() if n.endswith(".dist-info/METADATA"))
        metadata = wheel.read(name).decode()
    match = re.search(r"^Requires-Python: *(.+?)\s*$", metadata, re.MULTILINE)
    return match.group(1) if match else None


def upload(file: Path, bucket: str, dry_run: bool) -> None:
    """Uploads a file with the metadata that gcs-pypi puts in the index.

    sha256 is for pip's hash check, requires_python so pip skips versions that do not support
    its Python.
    """
    metadata = {"sha256": hashlib.sha256(file.read_bytes()).hexdigest()}
    if requires_python := requires_python_of(file):
        metadata["requires_python"] = requires_python

    if dry_run:
        print(f"would upload {file.name} {metadata}")
        return
    # "^|^" tells gcloud to separate the key=value pairs with "|" instead of ",",
    # because Requires-Python can contain commas, e.g. ">=3.8,<4".
    pairs = "|".join(f"{key}={value}" for key, value in metadata.items())
    sp.run(
        [
            "gcloud", "storage", "cp", file, f"{bucket}/{file.name}",
            f"--custom-metadata=^|^{pairs}",
            "--if-generation-match=0",
        ],
        check=True,
    )  # fmt: skip


def main() -> int:
    # Print progress right away, also when the output goes to a pipe or a file
    sys.stdout.reconfigure(line_buffering=True)
    args = parse_args()

    with tempfile.TemporaryDirectory() as work:
        work = Path(work)
        dest = args.dest or work / "dist"
        dest.mkdir(parents=True, exist_ok=True)

        requirements = args.requirements
        if args.uv:
            requirements = work / "uv-export.txt"
            export_uv_lock(requirements)

        failed = []
        for platform in args.platforms:
            print(f"==> resolving for {platform}, Python {args.python_version}")
            pinned = resolve(requirements, platform, args.python_version)

            print(f"==> downloading to {dest}")
            for requirement in pinned:
                print(f"--> {requirement}")
                if not download(requirement, platform, args.python_version, dest):
                    failed.append(f"{requirement} ({platform})")

        print(f"==> uploading to {args.bucket}")
        existing = files_in_bucket(args.bucket, args.dry_run)
        downloaded = [f for f in sorted(dest.iterdir()) if f.is_file()]
        new = [f for f in downloaded if f.name not in existing]
        for file in new:
            upload(file, args.bucket, args.dry_run)
        print(
            f"==> {len(new)} new files, {len(downloaded) - len(new)} already in the bucket"
        )

    if failed:
        print("failed to download:", *failed, sep="\n  ", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
