//! `delish <host>`: ssh to the host and run `delish proxy` there, which joins ssh's stdin and stdout to the host's
//! `delishd`, so the client speaks to that server as it does to its own. ssh only carries the frames: it is run
//! without a tty, and the command travels in the spawn frame rather than in a command line for the host's shell.

/// ssh's options that take a value (ssh(1)): the host is the first word that is neither an option nor one's value
const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// what to run on the host, unless DELISH_REMOTE_COMMAND says otherwise
pub const REMOTE_COMMAND: &str = "delish proxy";

#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// ssh's own options, as given
    pub options: Vec<String>,
    pub host: String,
    /// the words after the host, joined with spaces for the login shell as ssh joins them; None for an interactive one
    pub command: Option<String>,
}

pub fn parse(args: &[String]) -> Option<Target> {
    let mut options = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() < 2 {
            break;
        }
        options.push(a.clone());
        // a value is the rest of the word after its letter, or the next word
        if let Some(at) = a[1..].find(|c| WITH_VALUE.contains(c))
            && at + 2 == a.len()
        {
            i += 1;
            options.push(args.get(i)?.clone());
        }
        i += 1;
    }
    let host = args.get(i)?.clone();
    let words = &args[i + 1..];
    Some(Target {
        options,
        host,
        command: (!words.is_empty()).then(|| words.join(" ")),
    })
}

/// the ssh command line for the host: no tty, which would mangle the frames, whatever the options or config say
pub fn argv(t: &Target, remote: &str) -> Vec<String> {
    let mut argv = t.options.clone();
    argv.extend(["-T".into(), "--".into(), t.host.clone(), remote.into()]);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_host_alone_is_an_interactive_shell() {
        let t = parse(&args(&["stone"])).unwrap();
        assert_eq!(
            (t.options.len(), t.host.as_str(), t.command),
            (0, "stone", None)
        );
    }

    #[test]
    fn words_after_the_host_are_one_command_as_ssh_joins_them() {
        let t = parse(&args(&["stone", "ls", "-la", "|", "head"])).unwrap();
        assert_eq!(t.command.as_deref(), Some("ls -la | head"));
    }

    #[test]
    fn options_and_their_values_come_before_the_host() {
        let t = parse(&args(&[
            "-p",
            "2222",
            "-A",
            "-oBatchMode=yes",
            "-vJ",
            "jump",
            "u@glass",
            "git",
            "status",
        ]))
        .unwrap();
        assert_eq!(
            t.options,
            args(&["-p", "2222", "-A", "-oBatchMode=yes", "-vJ", "jump"])
        );
        assert_eq!(t.host, "u@glass");
        assert_eq!(t.command.as_deref(), Some("git status"));
        assert_eq!(
            argv(&t, REMOTE_COMMAND),
            args(&[
                "-p",
                "2222",
                "-A",
                "-oBatchMode=yes",
                "-vJ",
                "jump",
                "-T",
                "--",
                "u@glass",
                "delish proxy"
            ])
        );
    }

    #[test]
    fn a_double_dash_ends_the_options() {
        let t = parse(&args(&["-4", "--", "-odd-host"])).unwrap();
        assert_eq!(
            (t.options, t.host),
            (args(&["-4"]), "-odd-host".to_string())
        );
    }

    #[test]
    fn no_host_is_no_target() {
        assert_eq!(parse(&args(&[])), None);
        assert_eq!(parse(&args(&["-p"])), None);
        assert_eq!(parse(&args(&["-p", "22"])), None);
    }
}
