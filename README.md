# delish

DEsktop LogIn SHell. Run a command inside your logged-in desktop session from somewhere that is not, like an ssh
login, with a real terminal streamed between the two.

An ssh login is its own session: on Windows it cannot start rustup's symlinked proxies (os error 448), on macOS
`utmctl` and other Apple Events tools refuse to run, and on Linux the desktop's env (`WAYLAND_DISPLAY`,
`XDG_CURRENT_DESKTOP`) is missing. `delishd` runs inside the desktop session, and runs what `delish` sends it there.

## Binaries

| Binary     | Purpose                                                                                           |
| ---------- | ------------------------------------------------------------------------------------------------- |
| `delishd` | the server: started in the desktop session, runs each command on a pty of its own                 |
| `delish`  | the client: `delish run -- <cmd> [args...]` is that command's terminal, and exits with its code |

`delish shell [-c <command>]` runs your login shell there instead, so your whole profile loads in the session: on
Linux and macOS the account's shell with `-l` (`zsh -l -c '<command>'`); on Windows, which has no login shell, the
shell OpenSSH is set to start (`HKLM\SOFTWARE\OpenSSH\DefaultShell`, with its `DefaultShellCommandOption`, `-c`
unless set), or `cmd.exe` when there is none. It is the shell an ssh login would get, in the desktop session.

`delish status` says whether a server is listening, and which version it is: it exits 0 when it is this delish's
version, 1 with no server, and 3 with another, the case after an upgrade until the server is restarted.
`delish --version` prints the client's.

A client and a server speak a numbered protocol, and the server says its own and its version first: a pair that do not
match stops at once and says which side to update, rather than misreading each other.

### Another host's desktop, over ssh

```sh
delish [ssh options] <host> [command]
```

is `ssh <host> [command]`, run in the host's desktop session: with no command, an interactive login shell there, in
this terminal; with one, the login shell runs it (the words after the host are joined with spaces, as ssh joins them)
and its exit code is this one's. ssh's options go before the host (`delish -p 2222 -J jump user@glass`), and its
config applies as it does to ssh.

It runs `ssh -T <host> delish proxy`: `delish proxy` on the host joins ssh's stdin and stdout to the user's
`delishd` there, and the frames go through ssh untouched. So the host needs `delish` on the PATH an ssh command gets
(`DELISH_REMOTE_COMMAND` replaces `delish proxy`) and a `delishd` running in its desktop session. The command
travels in the frames, not as a command line for the host's shell, so nothing in it is quoted twice.

A host named `run`, `shell`, `status` or `proxy` is written `user@host`, or after `--`.

### Links

When a local `delishd` is running, `delish <host>` asks it for the session, from a desktop terminal or from inside an
ssh login, on every OS, and the server reaches the host over a **link**: one ssh connection to that host's `delishd`
that carries every session this server has there. Thirty sessions to a host are one ssh connection and one
`delish proxy`, not thirty, and a session to a host that already has a link starts without a new connection. The link's
ssh starts from the desktop session, with its ssh agent, keys and PATH, and nested logins go as far as they like.

- A link that drops comes back by itself, trying again after half a second, then a second, two, four, up to thirty,
  and its sessions carry on where they were: both ends keep what they sent until the other says it arrived, and send
  it again on the new link, so nothing is lost or seen twice. A session's command that writes faster than a link
  carries it, or while it is down, is paused, as a slow terminal pauses it, rather than losing output.
- A link that hears nothing for 45 seconds is taken for dropped, which a connection that went away without closing
  (a laptop asleep, a network gone) needs. One down for longer than its grace (`DELISH_LINK_GRACE`, 600 seconds, in
  the server's environment) ends its sessions with why; on the host, a session left with no link for that long ends
  as a closed terminal ends its command.
- A link outlives its last session by `DELISH_LINK_IDLE` (600 seconds), so the next starts at once.
- A client that goes away (its terminal closed) ends its session on the host at once, as with `ssh -t`.
- The link's ssh has no terminal to ask on: it runs with `BatchMode=yes`, so the host's key must be known and the
  login must not need a password (an agent, or a key without one). A link that cannot start says what ssh said.
- `DELISH_SSH` names the program the server runs for ssh.

With no local server, ssh runs from where it was typed, one connection per session, except in a Windows ssh login
(session 0), where Windows' `ssh.exe` never reads a pipe for its stdin and would hang: there it refuses. An agent
forwarded with `ssh -A` into a login is not the one a link uses, when a local server is running.

## How it works

- `delishd` is one long-running process per user, started at login inside the desktop session; only a process
  already there can start others with the session's env, display and token, and nothing from an ssh login can
  start one later. It serves every connection at once, a thread for each, and never forks itself: each connection
  gets a pty of its own and one child process on it, the command or the interactive login shell — to the server
  they are the same, run until it exits. A session from another host reaches it through ssh and a `delish proxy`
  of its own on that host, which joins ssh to the local socket. If `delishd` exits, every session it runs ends with
  it, as a terminal's tabs do; run zellij or tmux inside a session that has to outlive a restart.
- `delishd` listens on a unix socket at `~/.delish/delishd.sock` (`%USERPROFILE%` on Windows), in a directory only
  its user can open (a directory `DELISH_SOCKET` moves it to is left as it is). It logs to `~/.delish/delishd.log`,
  which names each command's program but never its arguments, and starts again past a megabyte, the last one kept as
  `delishd.log.1`.
- `delish run` sends the command, its cwd, its terminal size and `TERM`, then relays keys and resizes one way and
  output the other. Its exit code is the command's.
- The command runs on a pty: `openpty` on Linux and macOS, the console's own ConPTY on Windows. It gets the server's
  environment, which is the desktop session's.
- Each of the client's stdin, stdout and stderr that is not a terminal (`delish host cmd < in > out 2> err`) is a
  pipe for the command too, as with `ssh` without `-t`: piped input ends when the client's does and is not echoed
  back, output to a file is byte for byte, with no `\r` before each `\n`, and stderr comes back on the client's
  stderr. The streams that are terminals share the command's pty, whose bytes come back on the client's terminal; with
  none, there is no pty at all (and on Windows no ConPTY), as with `ssh -T`.
- When the command has exited, its terminal is closed after at most two seconds, as sshd closes a session's: a
  background job still holding it gets the hangup a closed window gives, rather than keeping the session open.
- A client that goes away takes its command with it, as a closed terminal does. Over `ssh -t`, dropping the
  connection ends the command; without `-t`, sshd leaves the client running, as it does any command.
- On Windows a login shell that is a batch file (`shell.bat`) is started through `cmd.exe` with the quoting Rust's
  standard library gives a batch file, so `|`, `&`, `"` and `%` in the command reach the shell as written.
- On Windows `delishd` is a GUI subsystem binary, so no console window opens in the session.
- Commands run with the server's rights, never the ssh login's. An administrator's ssh login on Windows is elevated;
  a server started in the desktop session has the user's own, filtered token, so its commands are not. `delishd`
  refuses to start elevated, in session 0 (where a Windows ssh login is), or as root.

## Notes

- On Windows, ConPTY writes its own terminal setup sequences (a screen clear, the window title, mouse and focus
  modes) to the console: they reach the client's terminal, never a stream it redirected.
- In a Windows ssh login, OpenSSH's own tools (`ssh`, `ssh-keygen`) can lose their output, as rustup's proxies fail to
  start: run them with `delish shell -c`, in the desktop session.
- PowerShell's `-c` exits 1 for any native command that failed, whatever its code: end a remote PowerShell command
  with `; exit $LASTEXITCODE` to pass the code through a Windows host.
- On macOS, `delishd` runs in the Aqua session, where privacy permissions apply to it: macOS asks once for Local
  Network access (any command reaching the LAN), and once per app it controls (Automation, e.g. UTM). Installed by
  ghpm, the permission belongs to the shim, which stays the same file across delish upgrades.

## Starting the server in the session

It starts with the desktop session and stays: only a process already in the session can start others with its env,
display and token. Stopping it leaves programs a session started and left running alone, as closing a terminal does.

| OS      | at login                                                                                                      | after an upgrade                                    |
| ------- | ------------------------------------------------------------------------------------------------------------- | --------------------------------------------------- |
| Linux   | a systemd user unit, `WantedBy=graphical-session.target`, `KillMode=process`; `systemctl --user enable --now` | `systemctl --user restart delishd`                 |
| macOS   | a LaunchAgent with `RunAtLoad`, `LimitLoadToSessionType` `Aqua` and `AbandonProcessGroup`                     | `launchctl kickstart -k gui/$UID/<label>`           |
| Windows | a scheduled task at logon, interactive, `RunLevel` limited, no time limit (`Register-ScheduledTask`)          | `Stop-Process -Name delishd`, `Start-ScheduledTask` |

An upgrade replaces the binary, not the server already running; until it is restarted, `delish status` exits 3.

To roll a new version out: install it on each machine, by whatever put delish there, then restart its server with the
command above; `delish status` says which still run an older one. A restart ends every session the server runs, the
one it was typed in too, so have the service manager do it (`systemctl`, `launchctl kickstart`, a process started
apart from the session on Windows), which finishes the restart even when the session that asked for it ends. A client
and a server on different protocols stop at once and say which to update.

## Building

```sh
cargo build --release
```

Produces `target/release/delish` and `target/release/delishd`.

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
