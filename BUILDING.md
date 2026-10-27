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
SHA-256 digest, installs the exact Patchright and DuckDuckGo AutoConsent versions,
and creates a deterministic archive. The Cargo build script verifies that archive
and embeds it. Cargo never downloads dependencies from the network during the
build itself.

On the first browser tool call, Rope extracts the payload into its versioned user
cache. Later launches reuse it. Each loaded session lazily creates its own
temporary Chrome profile, shared by that session's web tool calls and removed
during shutdown. Shell-job managers are also scoped to sessions.

The binary and library share the same core. `cargo test --locked` covers core
concurrency, shared approvals, snapshots, request deduplication, transport
authentication, attachments, and headless startup/shutdown. The integration
tests use temporary projects and a controlled provider; they need no API key.

Set `ROPE_BROWSER` to an external Chrome, Chromium, Brave, or Edge executable when
automatic discovery is not sufficient. Developers can skip embedding and point
`ROPE_PATCHRIGHT_RUNTIME_DIR` at an unpacked runtime payload.

## Running as a systemd service

The headless server is usually run as a system service so it survives logout and
restarts itself after a crash. `/etc/systemd/system/rope.service`:

```ini
[Unit]
Description=Rope headless server
StartLimitIntervalSec=0

[Service]
Type=exec
User=flame
WorkingDirectory=/projects/Rope
Environment=HOME=/home/flame
Environment=PATH=/home/flame/.cargo/bin:/home/flame/.local/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=/usr/bin/rope --headless --listen 0.0.0.0:8787
Restart=always
RestartSec=3
KillSignal=SIGTERM
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

`/usr/bin/rope` is a symlink to `target/release/rope`, so a new build is picked up
on the next restart. Cargo links into `target/release/deps` and then swaps the
`target/release/rope` path, which means a running instance keeps its own copy of
the old binary and a build needs no downtime; only the restart loads the new code:

```sh
cargo build --release
sudo systemctl restart rope
```

The unit runs the whole tree (Rope, its Node sidecar, and Chrome) in one cgroup, so
`systemctl stop` or a crash cleans the browser up too; `SIGTERM` triggers the normal
shutdown path, which stops active sessions and removes their temporary profiles.
Headless logging goes to stderr, which lands in the journal:

```sh
journalctl -u rope -f
systemctl status rope
```

## GitHub Actions

The `Build` workflow tests Rope and builds release archives for these native
targets:

| OS | Target | Archive |
| --- | --- | --- |
| Linux | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| macOS | `aarch64-apple-darwin` | `.tar.gz` |
| Windows | `x86_64-pc-windows-msvc` | `.zip` |

Pull requests, pushes to `main`, and manual runs only run the test suite. Pushing
a `v*` tag runs the tests, builds all three targets, and creates or updates a
GitHub release with the archives:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The workflow uses standard GitHub-hosted runners. They are free and unlimited
for public repositories; private repositories consume the account's included
Actions minutes. Cargo outputs and the browser payload are cached to keep tagged
builds short. Non-release runs use only the Linux test runner and store no
binary artifacts.
