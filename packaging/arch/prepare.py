#!/usr/bin/env python3
"""Create a checked source snapshot and PKGBUILD without requiring a remote URL."""
import argparse
import hashlib
from pathlib import Path
import re
import tarfile
import tomllib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="package build directory outside source subdirectories")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    output = args.output.resolve()
    entries = ["Cargo.toml", "Cargo.lock", "LICENSE", "README.md", "Makefile",
               "src", "tests", "examples", "docs", "packaging", ".github"]
    if output == root or any(output.is_relative_to(root / entry) for entry in entries):
        parser.error("output must not overlap archived sources (use build/arch or /tmp)")
    version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        parser.error("Arch packaging currently requires a numeric MAJOR.MINOR.PATCH version")
    name = f"thinkfan-next-{version}"
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"{name}.tar.gz"

    def include(info):
        if "__pycache__" in Path(info.name).parts or info.name.endswith(".pyc"):
            return None
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        return info

    with tarfile.open(archive, "w:gz", dereference=False) as tar:
        for entry in entries:
            tar.add(root / entry, arcname=f"{name}/{entry}", filter=include)
    with archive.open("rb") as stream:
        checksum = hashlib.file_digest(stream, "sha256").hexdigest()
    recipe = (root / "packaging/arch/PKGBUILD").read_text()
    recipe = re.sub(r"^pkgver=.*$", f"pkgver={version}", recipe, flags=re.MULTILINE)
    recipe = recipe.replace("sha256sums=('SKIP')", f"sha256sums=('{checksum}')")
    (output / "PKGBUILD").write_text(recipe)
    print(f"Prepared {archive} and {output / 'PKGBUILD'}")


if __name__ == "__main__":
    main()
