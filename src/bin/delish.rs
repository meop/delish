use delish::USAGE;

fn main() {
    let mut args = std::env::args().skip(1);
    let code = match args.next().as_deref() {
        Some("run") => {
            let mut argv: Vec<String> = args.collect();
            if argv.first().map(String::as_str) == Some("--") {
                argv.remove(0);
            }
            if argv.is_empty() {
                eprintln!("{USAGE}");
                2
            } else {
                delish::client::run(argv, false)
            }
        }
        Some("shell") => {
            let rest: Vec<String> = args.collect();
            match rest.as_slice() {
                [] => delish::client::run(vec![], true),
                [c, cmd] if c == "-c" => delish::client::run(vec![cmd.clone()], true),
                _ => {
                    eprintln!("{USAGE}");
                    2
                }
            }
        }
        Some("status") => delish::client::status(),
        Some("-V" | "--version") => {
            println!("delish {}", delish::proto::VERSION);
            0
        }
        Some(
            c @ ("key" | "hosts" | "trust" | "forget" | "peers" | "authorize" | "revoke" | "ping"),
        ) => delish::manage::run(c, &args.collect::<Vec<_>>()),
        None | Some("-h" | "--help") => {
            eprintln!("{USAGE}");
            2
        }
        // anything else is a host, or `--` before one named like a command
        Some(first) => {
            let mut words: Vec<String> = std::iter::once(first.to_string()).chain(args).collect();
            if words[0] == "--" {
                words.remove(0);
            }
            match words.split_first() {
                Some((host, command)) => {
                    // the words after the host are joined with spaces for its login shell, as ssh joins them
                    let command = (!command.is_empty()).then(|| command.join(" "));
                    delish::client::remote(host.clone(), command)
                }
                None => {
                    eprintln!("{USAGE}");
                    2
                }
            }
        }
    };
    // the command's own code: unix keeps its low byte, windows all of it
    std::process::exit(code)
}
