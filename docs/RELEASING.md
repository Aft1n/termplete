# Release and installation operations

This runbook describes the release package contract and the operator steps for publishing TermPlete's native and npm distributions. It distinguishes release automation in the repository from assets that are available to users.

## Current availability

The release workflow builds and publishes the native assets, checksum manifest, curl installer and generated Homebrew formula when a `v*` tag passes CI. After publishing the GitHub Release, it commits that formula to `Formula/termplete.rb` on the repository's default branch. Once that workflow finishes, Homebrew users can install using the explicit tap URL below. The Actions token must have `contents: write` permission, and default-branch protection must allow the workflow to push the formula update.

Public installation requires a public repository and a published release. Preparing the code or merging a PR alone does not make the download URLs available. Use the release checklist before announcing installation commands.

The npm package is named `termplete`; its version must match Cargo and the native release tag. Its initial publication is a maintainer step after the native release; later publication can use the configured npm trusted publisher.

The canonical source repository is [aft1n/termplete](https://github.com/aft1n/termplete). The application, Cargo crate and npm package remain named `termplete`; `tp` remains the short executable name.

## Release artifacts

The release workflow builds these four archives:

| Target | Platform |
| --- | --- |
| `aarch64-apple-darwin` | macOS, Apple silicon |
| `x86_64-apple-darwin` | macOS, Intel |
| `aarch64-unknown-linux-gnu` | Linux, ARM64, glibc 2.35+ |
| `x86_64-unknown-linux-gnu` | Linux, x86-64, glibc 2.35+ |

Each archive is named `termplete-VERSION-TARGET.tar.gz`, where `VERSION` omits the leading `v` from the Git tag. It contains exactly two root entries: `termplete` and `tp`. The release includes a standard two-column `SHA256SUMS` file with a SHA-256 digest and archive filename, plus `install.sh`, `LICENSE`, and a generated `termplete.rb` Homebrew formula. Checksums are generated for the release artifacts; this runbook does not embed placeholder values.

## Install from a published GitHub Release

The installer detects one of the four supported targets, downloads that target's archive and the release's `SHA256SUMS`, verifies the archive checksum, checks the archive contents, then installs both binaries. It rejects unsupported targets, mismatched checksums, and unexpected archive entries. It writes only to `$HOME/.local/bin` by default; use `TERMPLETE_INSTALL_DIR` or `--install-dir` to select another destination. It does not edit shell startup files. Use `tp setup` after installation to configure the shell integration.

~~~sh
curl -fsSL https://github.com/aft1n/termplete/releases/latest/download/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
tp setup
~~~

GitHub hosts both the [reviewable source](../scripts/install.sh) and the exact installer attached to each release; no Gist, GitHub Pages site or separate download service is needed. To inspect the release installer before executing it:

~~~sh
curl -fsSL https://github.com/aft1n/termplete/releases/latest/download/install.sh -o install.sh
less install.sh
sh install.sh
~~~

Pin both the installer and binary downloads to one release. Tags use a `v` prefix:

~~~sh
curl -fsSL https://github.com/aft1n/termplete/releases/download/v0.1.0/install.sh | sh -s -- --version v0.1.0
~~~

The script also accepts `--version v0.1.0` when run from a saved local copy.

Add `$HOME/.local/bin` to `PATH` if needed. The installer rolls back both files if replacing either binary fails. An exclusive lock protects the destination from concurrent replacements. If an interrupted installer leaves `.termplete-install.lock`, remove the empty lock only after confirming no installer is running.

## Install with Homebrew

After the release workflow completes its formula sync to the default branch:

~~~sh
brew tap aft1n/termplete https://github.com/aft1n/termplete
brew install aft1n/termplete/termplete
tp setup
~~~

The explicit URL taps the source repository and reads `Formula/termplete.rb`. Without that first command, Homebrew's shorthand looks for `aft1n/homebrew-termplete`, a different repository. See [Homebrew's tap naming rules](https://docs.brew.sh/Taps.html). The formula selects the matching macOS or Linux archive and verifies it against the SHA-256 checksum generated for that release. The formula is also attached to the GitHub Release for inspection and manual use. A separate Homebrew tap or inclusion in `homebrew/core` is optional; this source-repository tap needs no separate submission.

The source-build formula for a future `homebrew/core` submission must be regenerated against the final public release tag. The existing source-repository tap continues to use `Formula/termplete.rb`; see [`packaging/homebrew-core`](../packaging/homebrew-core/README.md).

## Install the npm wrapper

The npm wrapper requires Node.js 20 or newer. It exposes both `termplete` and `tp` commands, performs no postinstall download, and lazily fetches the matching native release archive on its first command run into the user's cache.

Once `termplete` is published to npm:

~~~sh
npm install -g termplete
tp setup
termplete --help
tp --help
~~~

For one-off use, invoke the package-matched executable:

~~~sh
npx termplete --help
npm exec --package=termplete -- tp --help
~~~

Bare `npx tp` can resolve a different npm package named `tp`; use the explicit package form for the alias.

## One-command release

After committing your feature on `main`, or merging its PR, run:

```sh
./scripts/publish.sh patch
```

The script requires Git, Python 3, and an authenticated GitHub CLI. Set up the
CLI once with `brew install gh` and `gh auth login`. Start with a clean `main`
checkout. The script pulls `origin/main` with fast-forward only, chooses the next
version, updates the Cargo and npm versions together, commits the version change,
and uses `release.sh` to atomically push `main` and the annotated tag. It then
waits for the exact tagged Release workflow to succeed and pulls the generated
Homebrew formula commit back into your checkout.

Choose the version increment or provide an exact version:

```sh
./scripts/publish.sh patch    # 0.1.0 -> 0.1.1
./scripts/publish.sh minor    # 0.1.0 -> 0.2.0
./scripts/publish.sh major    # 0.1.0 -> 1.0.0
./scripts/publish.sh 0.2.3    # exact new version
```

With no argument, `publish.sh` selects `patch`. An explicit current version is
also accepted for the first release, provided its tag does not already exist.
Existing tags are never replaced. If the hosted workflow fails, the script exits
with a link to that run; fix the cause or rerun its failed jobs in GitHub Actions,
then pull `origin/main` after the formula sync succeeds. A tag push alone does
not establish that the release has finished.

The successful Release workflow publishes all four native archives, checksums,
`install.sh`, and the generated formula. Homebrew updates and curl installations
then use those published assets. npm publication still needs its separate
one-time account setup and the Publish npm package workflow below.

Compiled dependency caches include both the runner image and target architecture.
Ubuntu 24.04 CI artifacts are kept separate from the Ubuntu 22.04 release builds
to preserve the release glibc baseline. PRs and release tags restore default-branch
caches when compatible; only `main` saves new caches.

## Maintainer release checklist

1. Make the canonical GitHub repository public for the open-source launch and have the validated code ready on local `main`. Configure a remote pointing to that public repository. Source builds and workflows require Rust 1.89; the publishing runtime uses Node.js 24 and npm 11.5.1 or newer.
2. For the launch release, keep the checked-in version (`0.1.0`). Use `scripts/publish.sh 0.1.0` for an untagged first release, or `scripts/publish.sh patch` for the next release. These commands update versions when needed, commit the version change, publish, wait, and pull the formula automatically. For manual control, use `scripts/bump-version.sh MAJOR.MINOR.PATCH`, review the changes, and commit them.
3. Review release notes and make sure CI has passed for the commit to be tagged.
4. If using the manual route, run `scripts/release.sh VERSION` after committing the version change. It requires a clean `main` branch, checks that local `main` includes the remote `main` when one exists, creates an annotated `vVERSION` tag, and atomically pushes `main` and the tag. Pass a remote name as a second argument when the public repository is not `origin`. This lower-level script returns after the push; GitHub Actions builds and publishes asynchronously. The tag starts the same Release workflow used by `publish.sh`.
5. Confirm the release assets, generated checksums and formula commit. If the release is already present, rerunning the workflow replaces its assets and retries the formula sync.
6. After the native release assets exist, bootstrap the npm package once from the repository root:
   ```sh
   cd npm
   npm login
   npm publish --access public
   ```
   Then configure a GitHub Actions trusted publisher on the npm package: repository `aft1n/termplete`, workflow `npm-publish.yml`. For subsequent versions, manually run **Publish npm package** with the exact published tag. It validates all four assets and checksums before publishing through OIDC; no long-lived npm token is stored in the workflow.
7. Confirm `termplete@VERSION` and both command shims are available in the public registry.
8. Exercise the curl installer, the explicit `brew tap` command above and `brew install aft1n/termplete/termplete` against the published artifacts. Exercise the npm lazy download after npm publication, before announcing each distribution.

Do not copy checksum values into source files by hand. The release workflow generates the manifest from the exact archives it publishes. Keep the GitHub release, formula URLs, npm package version, and native archive version aligned.

## Validation and release gates

Before building release archives, the tag workflow invokes the same CI workflow used for pull requests. Linux runs formatting, Clippy, Rust tests, the decision/privacy and full zsh PTY suites, installer and release-automation fixtures, npm checks, and verified Cargo packaging. macOS runs Rust tests, a native release build, and focused zsh setup/startup checks. The native matrix builds and smoke-tests both executables on all four architectures. Linux binaries are built on Ubuntu 22.04 for a glibc 2.35 baseline. The archives use the ustar format without macOS resource-fork entries, matching the strict installer contract.

Local verification does not establish that the hosted matrix or live package installations have run. Confirm the actual workflow runs, release downloads, Homebrew installation and registry publication before announcing each distribution. The formula sync requires the Actions token to push to the default branch; if branch protection blocks that push, the release assets remain published but the Homebrew tap update must be allowed or applied manually.

Official publishing setup: [npm trusted publishers](https://docs.npmjs.com/trusted-publishers/). Initial package creation and configuring the trusted publisher require the npm package owner.
