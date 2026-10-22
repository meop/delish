const USAGE: &str = "usage: deskpty [ssh options] <host> [command]
       deskpty run [--] <command> [args...]
       deskpty shell [-c <command>]
       deskpty status
       deskpty proxy";

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
                deskpty::client::run(argv, false)
            }
        }
        Some("shell") => {
            let rest: Vec<String> = args.collect();
            match rest.as_slice() {
                [] => deskpty::client::run(vec![], true),
                [c, cmd] if c == "-c" => deskpty::client::run(vec![cmd.clone()], true),
                _ => {
                    eprintln!("{USAGE}");
                    2
                }
            }
        }
        Some("status") => match deskpty::sock::connect() {
            Ok(_) => {
                println!(
                    "deskptyd is listening on {}",
                    deskpty::sock::path().display()
                );
                0
            }
            Err(e) => {
                println!("no deskptyd on {}: {e}", deskpty::sock::path().display());
                1
            }
        },
        Some("proxy") => deskpty::proxy::run(),
        Some(deskpty::client::HOP) => {
            let mut rest: Vec<String> = args.collect();
            let remote = if rest.first().map(String::as_str)
                == Some(deskpty::client::REMOTE_COMMAND_OPTION)
                && rest.len() > 1
            {
                rest.drain(..2).nth(1).unwrap_or_default()
            } else {
                deskpty::client::remote_command()
            };
            match deskpty::ssh::parse(&rest) {
                Some(target) => deskpty::client::hop(target, remote),
                None => {
                    eprintln!("{USAGE}");
                    2
                }
            }
        }
        None | Some("-h" | "--help") => {
            eprintln!("{USAGE}");
            2
        }
        // anything else is ssh's: a host, after its options
        Some(first) => {
            let all: Vec<String> = std::iter::once(first.to_string()).chain(args).collect();
            match deskpty::ssh::parse(&all) {
                Some(target) => deskpty::client::remote(target),
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
