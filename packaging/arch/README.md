# Arch Linux package

The package installs the executable at `/usr/bin/thinkfan-next`, the manual page,
systemd units, examples and documentation, and
`/usr/lib/modprobe.d/thinkfan-next.conf` enabling `thinkpad_acpi fan_control=1`.
It does not create `/etc/thinkfan.yaml`, start services, or rebuild the initramfs.
Follow the [operations guide](../../docs/operations.md) after installation.

## Build locally

Run `makepkg` as a regular user on Arch Linux:

```sh
sudo pacman -S --needed base-devel rust clang lm_sensors python
python packaging/arch/prepare.py build/arch
cd build/arch
makepkg --cleanbuild
sudo pacman -U ./thinkfan-next-*.pkg.tar.zst
```

`prepare.py` reads the version from `Cargo.toml`, archives an explicit set of source
paths, and writes a PKGBUILD with the archive's SHA-256 checksum. It works without
a Git remote or committed changes and excludes repository credentials and build
outputs. The checked-in PKGBUILD is a template for this local snapshot workflow;
use the generated recipe rather than invoking it directly. Numeric three-component
versions are currently required.

Dependencies are fetched with `cargo fetch --locked` during preparation. Build
and tests use `--frozen`. The target is x86_64 Linux. `makepkg` runs tests and checks
every example configuration without reading fan hardware. Optional runtime
backends need `nvidia-utils` or `libatasmart`; libsensors is required for all builds.

## GitHub Actions

[The workflow](../../.github/workflows/archlinux.yml) runs on pushes, pull requests,
and manual dispatch. It uses an Arch Linux container, installs dependencies, and
runs makepkg as an unprivileged user. It then installs the package inside that
container, checks paths and example configurations, and uploads the package and
`SHA256SUMS` as an Actions artifact retained for 14 days.

No repository URL is hard-coded. After the repository is created and the source
is pushed, download the ZIP from the workflow run's **Artifacts** section,
extract it, and run `sha256sum -c SHA256SUMS` before installation. The checksums
check artifact integrity; the packages are not signed. The workflow does not
publish a release or an AUR package.

`pkgrel` is maintained in the recipe. Development builds of the same Cargo version
have the same package version; the artifact name includes the commit SHA to
identify the source. Increment `pkgrel` or the Cargo version for distributed updates.
