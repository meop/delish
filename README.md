# deskpty

Desktop session PTY. Run a command inside your logged-in desktop session from somewhere that is not, like an ssh
login, with a real terminal streamed between the two.

An ssh login is its own session: on Windows it cannot start rustup's symlinked proxies (os error 448), on macOS
`utmctl` and other Apple Events tools refuse to run, and on Linux the desktop's env (`WAYLAND_DISPLAY`,
`XDG_CURRENT_DESKTOP`) is missing. `deskptyd` runs inside the desktop session, and runs what `deskpty` sends it there.

## Binaries

| Binary     | Purpose                                                                                           |
| ---------- | ------------------------------------------------------------------------------------------------- |
| `deskptyd` | the server: started in the desktop session, runs each command on a pty of its own                 |
| `deskpty`  | the client: `deskpty run -- <cmd> [args...]` is that command's terminal, and exits with its code |

`deskpty status` says whether a server is listening.

## How it works

- `deskptyd` listens on a unix socket at `~/.deskpty/deskptyd.sock` (`%USERPROFILE%` on Windows), in a directory only
  its user can open. `DESKPTY_SOCKET` moves it. It logs to `~/.deskpty/deskptyd.log`.
- `deskpty run` sends the command, its cwd, its terminal size and `TERM`, then relays keys and resizes one way and
  output the other. Its exit code is the command's.
- The command runs on a pty: `openpty` on Linux and macOS, the console's own ConPTY on Windows. It gets the server's
  environment, which is the desktop session's.
- A client that goes away takes its command with it, as a closed terminal does. Over `ssh -t`, dropping the
  connection ends the command; without `-t`, sshd leaves the client running, as it does any command.
- On Windows `deskptyd` is a GUI subsystem binary, so no console window opens in the session.

## Starting the server in the session

| OS      | how                                                                                                    |
| ------- | ------------------------------------------------------------------------------------------------------ |
| Linux   | `systemd-run --user --unit deskptyd deskptyd` (the user manager carries the desktop's env)             |
| macOS   | a LaunchAgent with `LimitLoadToSessionType` `Aqua`, `launchctl bootstrap gui/$UID <plist>` + `kickstart` |
| Windows | a scheduled task with logon mode "interactive only" (`schtasks /create ... /it`), then `schtasks /run`  |

## Building

```sh
cargo build --release
```

Produces `target/release/deskpty` and `target/release/deskptyd`.

## Cross-compile

Add the target with `rustup target add <target>` then build:

```sh
cargo build --release --target aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
cargo build --release --target aarch64-pc-windows-msvc
cargo build --release --target x86_64-pc-windows-msvc
```

## Verifying releases

Every release archive contains `LICENSE`. Verify the signed checksum manifest with the public release key before
checking downloaded artifacts:

```sh
gpg --verify SHA256SUMS.sig SHA256SUMS
sha256sum --ignore-missing --check SHA256SUMS
```

## License

[MIT](LICENSE)
