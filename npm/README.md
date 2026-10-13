# TermPlete

Install the native command line app with Node.js 20 or newer:

```sh
npm install -g termplete
tp setup
```

For a one-time run with `npx`:

```sh
npx termplete setup
npm exec --package=termplete -- tp status
```

The `termplete` and `tp` commands download the matching native release on first
run, verify its SHA-256 checksum against the release's `SHA256SUMS` file, and
cache both executables under your user cache directory. npm installation does
not run a postinstall script or modify TermPlete configuration. The first
command run needs an internet connection; later runs use the cached binaries.

Prebuilt binaries are available for macOS and glibc-based Linux on x64 and
ARM64. Windows, Linux musl, and other architectures are not currently
supported by this package. To build from source, install Rust and run:

```sh
cargo install --git https://github.com/aft1n/termplete --locked
```

The package is published from a GitHub Release after the native release assets
and `SHA256SUMS` are attached. npm trusted publishing must be configured for
the repository before the publish workflow is run.
