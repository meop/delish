# AGENTS.md

## After every code change

Run all four — do not skip any:

```sh
cargo build --release
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
```

The Windows code is checked from Linux or macOS with `--target x86_64-pc-windows-msvc` on `cargo check` and
`cargo clippy`; linking it needs a Windows linker.

## Keeping this file current

Update this file whenever a binary or source file is added, removed or renamed, the wire format changes, or a
convention changes — do not wait to be asked. If you notice a discrepancy between this file and the code, fix the
file immediately.

Update `README.md` when user-visible behavior changes: a binary's arguments, where the socket or log is, or how the
server is started.

## Cross-compile

The musl targets link with `musl-gcc` (`.cargo/config.toml`); the Windows targets link the C runtime statically.

```sh
cargo build --release --target aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin   # cross-compiled from macOS arm64
cargo build --release --target aarch64-pc-windows-msvc
cargo build --release --target x86_64-pc-windows-msvc
```

Add a target with `rustup target add <target>` before building for it.

## Publishing

To cut a release: bump `version` in `Cargo.toml` and push to `main`. A version with a pre-release part
(`0.1.0-alpha.1`) is published as a GitHub prerelease; one without, as a release.

The CI pipeline (`.github/workflows/pipeline.yaml`) runs: Version check (skips the rest if `v<version>` already
exists) → Validate (fmt + clippy, for Linux and for Windows, + test) → Build for every target → Publish, which signs
`SHA256SUMS`, creates a GPG-signed tag `v<version>` and publishes the archives to a public GitHub release. Requires
the `GPG_PRIVATE_KEY` and `GPG_PASSPHRASE` repository secrets. Never create the tag by hand.

## Layout

| file              | what it is                                                                                 |
| ----------------- | ------------------------------------------------------------------------------------------ |
| `src/proto.rs`    | the wire format: frames of kind byte + u32 LE length + payload; one connection, one command. a spawn is size, login flag, cwd, argv, env. `READY` is the proxy's own, before the server's frames |
| `src/sock.rs`     | the socket path and its permissions; `std` unix sockets, `uds_windows` on Windows          |
| `src/pty/unix.rs` | `openpty`, the child in its own session with the pty as its controlling terminal            |
| `src/pty/windows.rs` | ConPTY from kernel32 (the console's own; nothing side-loaded)                          |
| `src/cmdline.rs`  | a Windows command line from argv: the C runtime's quoting, or std's for a batch file through `cmd.exe`; plain strings, tested everywhere |
| `src/shell.rs`    | the login shell: the account's with `-l` on unix, OpenSSH's `DefaultShell` on Windows    |
| `src/term.rs`     | the client's terminal: size, and raw mode while a command runs                             |
| `src/server.rs`   | `deskptyd`: one thread per connection, the command's output out, its exit code last; refuses to start elevated, as root, or in session 0 |
| `src/client.rs`   | `deskpty run`, `shell`, `<host>` and `hop`: keys and resizes in, output out, the command's exit code as its own; to the local socket, or to ssh's stdin and stdout. `<host>` with a local server running hands itself to it as `deskpty hop --remote-command <cmd> <ssh args>`, which calls ssh; without one it calls ssh itself, except in a Windows ssh login (session 0), where it refuses |
| `src/ssh.rs`      | `deskpty <host>`: ssh's options and the host from the arguments, and the `ssh -T` command line |
| `src/proxy.rs`    | `deskpty proxy`: on the host, ssh's stdin and stdout joined to the local server, after a `READY` or `ERROR` |

## Conventions

- Two binaries in one crate, both released on every platform: `deskpty` and `deskptyd`
- Version is in `Cargo.toml`; CI creates a signed tag on version bump
- No async runtime: a few threads per connection, blocking I/O.
- One `deskptyd` process holds every session: a thread per connection, a pty and one child per session, never a
  fork of the server. Its exit ends them all; a helper process per session was judged not worth it.
- ssh always starts from the desktop session when a local server runs (`hop`): the hop runs with the server's
  environment, not the caller's, so anything it needs from the caller (`DESKPTY_REMOTE_COMMAND`) goes as an argument.
- Never hang: whatever cannot work (no server, Windows' `ssh.exe` in session 0) refuses at once with why.
- Dependencies stay few: `libc` on unix, `windows-sys` and `uds_windows` on Windows.
- Through ssh the command goes in the spawn frame, never in ssh's command line: the host's shell only ever runs
  `deskpty proxy`.
- A command is never killed once it has been waited on: its pid may be another process's by then.
