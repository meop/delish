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

`deskpty shell [-c <command>]` runs your login shell there instead, so your whole profile loads in the session: on
Linux and macOS the account's shell with `-l` (`zsh -l -c '<command>'`); on Windows, which has no login shell, the
shell OpenSSH is set to start (`HKLM\SOFTWARE\OpenSSH\DefaultShell`, with its `DefaultShellCommandOption`, `-c`
unless set), or `cmd.exe` when there is none. It is the shell an ssh login would get, in the desktop session.

`deskpty status` says whether a server is listening.

### Another host's desktop, over ssh

```sh
deskpty [ssh options] <host> [command]
```

is `ssh <host> [command]`, run in the host's desktop session: with no command, an interactive login shell there, in
this terminal; with one, the login shell runs it (the words after the host are joined with spaces, as ssh joins them)
and its exit code is this one's. ssh's options go before the host (`deskpty -p 2222 -J jump user@glass`), and its
config applies as it does to ssh.

It runs `ssh -T <host> deskpty proxy`: `deskpty proxy` on the host joins ssh's stdin and stdout to the user's
`deskptyd` there, and the frames go through ssh untouched. So the host needs `deskpty` on the PATH an ssh command gets
(`DESKPTY_REMOTE_COMMAND` replaces `deskpty proxy`) and a `deskptyd` running in its desktop session. The command
travels in the frames, not as a command line for the host's shell, so nothing in it is quoted twice. A password or
host key prompt is answered before the terminal goes raw. A dropped connection ends the command, as with `ssh -t`.

A host named `run`, `shell`, `status` or `proxy` is written `user@host`, or after `--`.

On Windows, from inside an ssh login (session 0) `deskpty <host>` runs itself through the local `deskptyd`, as
`deskpty run -- deskpty <host>` would: Windows' `ssh.exe` never reads a pipe for its stdin in session 0, and would hang.
With no local server it refuses rather than hang. From a terminal on the desktop it runs directly, as it does nested on
Linux and macOS.

## How it works

- `deskptyd` listens on a unix socket at `~/.deskpty/deskptyd.sock` (`%USERPROFILE%` on Windows), in a directory only
  its user can open. `DESKPTY_SOCKET` moves it. It logs to `~/.deskpty/deskptyd.log`.
- `deskpty run` sends the command, its cwd, its terminal size and `TERM`, then relays keys and resizes one way and
  output the other. Its exit code is the command's.
- The command runs on a pty: `openpty` on Linux and macOS, the console's own ConPTY on Windows. It gets the server's
  environment, which is the desktop session's.
- A client that goes away takes its command with it, as a closed terminal does. Over `ssh -t`, dropping the
  connection ends the command; without `-t`, sshd leaves the client running, as it does any command.
- On Windows a login shell that is a batch file (`shell.bat`) is started through `cmd.exe` with the quoting Rust's
  standard library gives a batch file, so `|`, `&`, `"` and `%` in the command reach the shell as written.
- On Windows `deskptyd` is a GUI subsystem binary, so no console window opens in the session.
- Commands run with the server's rights, never the ssh login's. An administrator's ssh login on Windows is elevated;
  a server started in the desktop session has the user's own, filtered token, so its commands are not. `deskptyd`
  refuses to start elevated, in session 0 (where a Windows ssh login is), or as root.

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
