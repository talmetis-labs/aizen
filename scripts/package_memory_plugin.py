"""Package an already-built native Memory provider without executing it.

Run with Python -I. This creates a new output directory; it never builds, installs, publishes,
replaces a package, or changes provider data. Hashes identify bytes, not publisher trust.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import tempfile
import unicodedata
import zipfile


MAX_FILE = 128 * 1024 * 1024
MAX_PACKAGE = 256 * 1024 * 1024
SUPPORTED_TARGETS = {
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-musl",
    "aarch64-apple-darwin",
}
CAPABILITIES = {"recall", "records", "context", "learning", "associations", "host_llm", "commands", "management", "legacy_adoption_v1", "legacy_adoption_v2"}
DEVICES = {"con", "prn", "aux", "nul", "clock", "conin", "conout"}
DEVICES.update(f"{prefix}{number}" for prefix in ("com", "lpt") for number in range(10))


def relative_name(value: str) -> str:
    if not value or len(value) > 240 or "\\" in value or ":" in value or value.startswith("/"):
        raise ValueError("package paths must be short relative POSIX paths")
    parts = value.split("/")
    if len(parts) > 12:
        raise ValueError("package paths exceed depth limit")
    for part in parts:
        if len(part) > 100 or part in ("", ".", "..") or part.startswith(".") or part.endswith((".", " ")):
            raise ValueError("invalid package path component")
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", part) or part.split(".")[0].lower() in DEVICES:
            raise ValueError("unsupported or reserved package path component")
    if str(PurePosixPath(value)) != value:
        raise ValueError("package paths must be canonical")
    return value


def inspect_plain(path: Path, *, regular: bool = False) -> os.stat_result:
    for ancestor in (path, *path.parents):
        metadata = ancestor.lstat()
        if stat.S_ISLNK(metadata.st_mode) or getattr(metadata, "st_file_attributes", 0) & 0x400:
            raise ValueError("links/reparse points are not package inputs")
    metadata = path.stat()
    if regular and (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1):
        raise ValueError("package inputs must be ordinary non-hardlinked files")
    return metadata


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def validate_version(value: str) -> None:
    if not value or len(value) > 64 or not value.isascii():
        raise ValueError("invalid semantic package version length")
    before_build, plus, build = value.partition("+")
    core, dash, prerelease = before_build.partition("-")
    parts = core.split(".")
    if len(parts) != 3:
        raise ValueError("use an explicit semantic package version")
    for part in parts:
        if (not re.fullmatch(r"[0-9]{1,10}", part)
                or (len(part) > 1 and part.startswith("0"))
                or int(part) > 0xFFFFFFFF):
            raise ValueError("invalid semantic version component")
    for suffix, present, is_prerelease in ((prerelease, dash, True), (build, plus, False)):
        if present:
            for part in suffix.split("."):
                if not re.fullmatch(r"[A-Za-z0-9-]+", part):
                    raise ValueError("invalid semantic version suffix")
                if is_prerelease and part.isdigit() and len(part) > 1 and part.startswith("0"):
                    raise ValueError("semantic prerelease numbers must not have leading zeros")


def validate_display_name(value: str) -> None:
    if (not value.strip() or len(value.encode("utf-8")) > 128
            or any(unicodedata.category(c) == "Cc" or 0x202A <= ord(c) <= 0x202E
                   or 0x2066 <= ord(c) <= 0x2069 for c in value)):
        raise ValueError("invalid display name")


def package(args: argparse.Namespace) -> Path:
    if not re.fullmatch(r"[a-z](?:[a-z0-9-]{0,62}[a-z0-9])?", args.id):
        raise ValueError("invalid plugin ID")
    relative_name(args.id)
    validate_version(args.version)
    validate_display_name(args.name)
    if args.target not in SUPPORTED_TARGETS:
        raise ValueError("unsupported package target")
    capabilities = sorted(set(args.capability))
    if len(capabilities) != len(args.capability):
        raise ValueError("duplicate capability")
    if set(capabilities) - CAPABILITIES:
        raise ValueError("unknown capability")
    if args.host_llm != ("host_llm" in capabilities):
        raise ValueError("host_llm permission and capability must agree")
    entrypoint = relative_name(args.entrypoint)
    if "/" in entrypoint:
        raise ValueError("this packaging helper requires a package-root native entrypoint")
    if args.target.endswith("windows-msvc") and not entrypoint.endswith(".exe"):
        raise ValueError("Windows entrypoints must have an .exe extension")
    inputs = [(entrypoint, args.binary), ("LICENSE", args.license), ("NOTICE", args.notice)]
    for item in args.file:
        published, separator, source = item.partition("=")
        if not separator or not source:
            raise ValueError("--file requires relative/package/path=source-file")
        inputs.append((relative_name(published), Path(source)))
    seen = {"plugin.json"}
    namespace = {"plugin.json": ("plugin.json", False)}
    total = 0
    checked = []
    for published, source in inputs:
        if published.lower() in seen:
            raise ValueError("duplicate/case-colliding package name")
        seen.add(published.lower())
        parts = published.split("/")
        for end in range(1, len(parts) + 1):
            node = "/".join(parts[:end])
            identity = (node, end < len(parts))
            previous = namespace.setdefault(node.lower(), identity)
            if previous != identity:
                raise ValueError("case/prefix/file-directory collision")
        source = source.absolute()
        metadata = inspect_plain(source, regular=True)
        if metadata.st_size > MAX_FILE:
            raise ValueError("package file exceeds 128 MiB")
        total += metadata.st_size
        if total > MAX_PACKAGE or len(inputs) > 128:
            raise ValueError("package exceeds file/count limits")
        checked.append((published, source, metadata.st_size, digest(source)))
    output = args.output.absolute()
    inspect_plain(output.parent)
    if output.exists() or output.is_symlink():
        raise ValueError("output already exists; select a new directory")
    staging = Path(tempfile.mkdtemp(prefix=".memory-package-", dir=output.parent))
    try:
        files = []
        payload = staging / "package"
        payload.mkdir()
        for published, source, size, sha256 in checked:
            destination = payload / published
            destination.parent.mkdir(parents=True, exist_ok=True)
            with source.open("rb") as source_stream, destination.open("xb") as target_stream:
                shutil.copyfileobj(source_stream, target_stream)
            if destination.stat().st_size != size or digest(destination) != sha256:
                raise ValueError("an input changed while packaging")
            if published == entrypoint:
                destination.chmod(0o755)
            else:
                destination.chmod(0o644)
            files.append({"path": published, "bytes": size, "sha256": sha256})
        manifest = {
            "schema_version": 1, "id": args.id, "name": args.name, "version": args.version,
            "kind": "memory", "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
            "target": args.target, "entrypoint": entrypoint, "capabilities": capabilities,
            "permissions": {"network": args.network, "host_llm": args.host_llm},
            "files": sorted(files, key=lambda item: item["path"]),
        }
        (payload / "plugin.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        archive = staging / f"{args.id}-{args.version}-{args.target}.zip"
        with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as zip_stream:
            for source in sorted(payload.rglob("*")):
                if not source.is_file():
                    continue
                name = source.relative_to(payload).as_posix()
                info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
                info.create_system = 3
                mode = 0o755 if name == entrypoint else 0o644
                info.external_attr = (stat.S_IFREG | mode) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                zip_stream.writestr(info, source.read_bytes(), compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
        (staging / "SHA256SUMS").write_text(f"{digest(archive)}  {archive.name}\n", encoding="ascii")
        # No existing destination is replaced; the sibling stage is the only cleanup-owned path.
        staging.rename(output)
    except BaseException:
        shutil.rmtree(staging)
        raise
    return output / archive.name


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--id", required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True, choices=sorted(SUPPORTED_TARGETS))
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--entrypoint", required=True)
    parser.add_argument("--license", required=True, type=Path)
    parser.add_argument("--notice", required=True, type=Path)
    parser.add_argument("--file", action="append", default=[])
    parser.add_argument("--capability", action="append", default=[], choices=sorted(CAPABILITIES))
    parser.add_argument("--network", action="store_true")
    parser.add_argument("--host-llm", action="store_true")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        archive = package(args)
    except (OSError, ValueError) as error:
        parser.exit(1, f"Packaging failed: {error}\n")
    print(f"Created local package: {archive}")
    print("Not installed or published. Hashes are not a publisher signature.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
