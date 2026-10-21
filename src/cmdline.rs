//! A Windows command line from argv. A program reads its own back with the C runtime's rules; a batch file is read by
//! cmd.exe, whose rules are its own, so it gets the quoting the standard library gives one (after CVE-2024-24576):
//! the whole line quoted once more, each argument quoted with its quotes doubled, and `%` kept from expanding.
//! These are plain string functions, so their tests run on every platform.

use std::io;

/// a program's command line, or a batch file's through cmd.exe
pub fn command_line(argv: &[String]) -> io::Result<String> {
    let program = argv.first().map(String::as_str).unwrap_or_default();
    if is_batch(program) {
        return batch_command_line(program, &argv[1..]);
    }
    let mut out = String::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        quote(a, &mut out);
    }
    Ok(out)
}

pub fn is_batch(program: &str) -> bool {
    let lower = program.to_ascii_lowercase();
    lower.ends_with(".bat") || lower.ends_with(".cmd")
}

/// one argument as the C runtime's parser reads it back: quoted where it has to be, a quote escaped with a
/// backslash, and the backslashes before a quote (or the closing one) doubled
fn quote(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
}

fn batch_command_line(script: &str, args: &[String]) -> io::Result<String> {
    let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidInput, msg.to_string());
    if script.contains('"') || script.ends_with('\\') {
        return Err(invalid(
            "a batch file's name may not contain `\"` or end with `\\`",
        ));
    }
    let mut out = format!("cmd.exe /e:ON /v:OFF /d /c \"\"{script}\"");
    for arg in args {
        // a line break would end the command there
        if arg.contains(['\r', '\n']) {
            return Err(invalid(
                "a batch file's arguments may not contain a line break",
            ));
        }
        out.push(' ');
        batch_arg(arg, &mut out);
    }
    out.push('"');
    Ok(out)
}

fn batch_arg(arg: &str, out: &mut String) {
    // anything outside a short list of safe ascii is quoted, rather than finding every symbol cmd acts on
    const UNQUOTED: &str = r"#$*+-./:?@\_";
    let quoted = arg.is_empty()
        || arg.ends_with('\\')
        || arg.chars().any(|c| {
            (c.is_ascii() && !(c.is_ascii_alphanumeric() || UNQUOTED.contains(c))) || c.is_control()
        });
    if quoted {
        out.push('"');
    }
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push('"');
                slashes = 0;
            }
            // `%cd:~,%` is an empty substring of the always-set cd: it keeps cmd from reading `%x%` as a variable
            '%' => {
                out.push_str("%%cd:~,");
                slashes = 0;
            }
            _ => slashes = 0,
        }
        out.push(c);
    }
    if quoted {
        out.extend(std::iter::repeat_n('\\', slashes));
        out.push('"');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments_quote_as_the_c_runtime_reads_them() {
        let a = argv(&[
            "nu",
            "-c",
            "print \"a b\"",
            r"C:\dir with space\",
            "",
            r"x\\y",
        ]);
        assert_eq!(
            command_line(&a).unwrap(),
            r#"nu -c "print \"a b\"" "C:\dir with space\\" "" x\\y"#
        );
    }

    #[test]
    fn a_batch_file_goes_through_cmd_with_its_symbols_quoted() {
        let a = argv(&[
            r"C:\ProgramData\ssh\shell.bat",
            "-c",
            r#"Get-Command wut | Select "Name"; exit 6"#,
        ]);
        assert_eq!(
            command_line(&a).unwrap(),
            r#"cmd.exe /e:ON /v:OFF /d /c ""C:\ProgramData\ssh\shell.bat" -c "Get-Command wut | Select ""Name""; exit 6"""#
        );
    }

    #[test]
    fn a_batch_argument_keeps_percent_from_expanding() {
        let a = argv(&["x.cmd", "%PATH%"]);
        assert_eq!(
            command_line(&a).unwrap(),
            r#"cmd.exe /e:ON /v:OFF /d /c ""x.cmd" "%%cd:~,%PATH%%cd:~,%"""#
        );
    }

    #[test]
    fn a_batch_argument_with_a_line_break_is_refused() {
        assert!(command_line(&argv(&["x.bat", "a\nb"])).is_err());
        assert!(command_line(&argv(&["x.BAT", "ok"])).is_ok());
    }
}
