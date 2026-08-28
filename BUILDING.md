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
