# Building Rope

## Embedded browser runtime

Rope uses an external Chrome-family browser, but packages its pinned Node and
Patchright dependencies into the executable. Prepare the payload before the
release build:

```sh
./scripts/prepare-patchright-runtime.sh
cargo build --release
```

Pass a Rust target triple to prepare a cross-platform payload:

```sh
./scripts/prepare-patchright-runtime.sh aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

The preparation script downloads Node from nodejs.org, verifies its published
SHA-256 digest, installs the exact Patchright version, and creates a deterministic
archive. The Cargo build script verifies that archive and embeds it. Cargo never
downloads dependencies from the network during the build itself.

On the first browser tool call, Rope extracts the payload into its versioned user
cache. Later launches reuse it. A temporary Chrome profile is shared by all web
tool calls in one Rope process and removed during shutdown.

Set `ROPE_BROWSER` to an external Chrome, Chromium, Brave, or Edge executable when
automatic discovery is not sufficient. Developers can skip embedding and point
`ROPE_PATCHRIGHT_RUNTIME_DIR` at an unpacked runtime payload.

## GitHub Actions

The `Build` workflow tests Rope and builds release archives for these native
targets:

| OS | Target | Archive |
| --- | --- | --- |
| Linux | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| macOS | `aarch64-apple-darwin` | `.tar.gz` |
| Windows | `x86_64-pc-windows-msvc` | `.zip` |

Pull requests and pushes to `main` compile all three targets. To download the
binaries without making a release, run the workflow manually from the Actions
tab; its artifacts are retained for one day. Pushing a `v*` tag also creates or
updates a GitHub release with all three archives:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The workflow uses standard GitHub-hosted runners. They are free and unlimited
for public repositories; private repositories consume the account's included
Actions minutes. Cargo outputs and the browser payload are cached to keep those
builds short, and pull requests do not store binary artifacts.
