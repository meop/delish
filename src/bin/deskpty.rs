const USAGE: &str = "usage: deskpty run [--] <command> [args...]\n       deskpty shell [-c <command>]\n       deskpty status";

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
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    // the command's own code: unix keeps its low byte, windows all of it
    std::process::exit(code)
}
