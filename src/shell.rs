//! The user's login shell, found the way sshd finds it: on unix the account's shell, started with `-l` so the whole
//! profile loads; on windows, which has no login shell, the one OpenSSH is set to start (`DefaultShell`), which for
//! pwsh loads its profile as it is.

/// the command line for the login shell, interactive or running `cmd`
pub fn login_argv(cmd: Option<&str>) -> Vec<String> {
    #[cfg(unix)]
    {
        unix_argv(&account_shell(), cmd)
    }
    #[cfg(windows)]
    {
        let (shell, option) = default_shell();
        windows_argv(&shell, &option, cmd)
    }
}

pub fn unix_argv(shell: &str, cmd: Option<&str>) -> Vec<String> {
    let mut argv = vec![shell.to_string(), "-l".to_string()];
    if let Some(c) = cmd {
        argv.push("-c".into());
        argv.push(c.into());
    }
    argv
}

/// a batch file needs no wrapping here: the command line for it is made to go through cmd.exe (`cmdline`)
pub fn windows_argv(shell: &str, option: &str, cmd: Option<&str>) -> Vec<String> {
    let mut argv = vec![shell.to_string()];
    if let Some(c) = cmd {
        argv.push(option.into());
        argv.push(c.into());
    }
    argv
}

/// the account's entry, then SHELL, then sh
#[cfg(unix)]
fn account_shell() -> String {
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if !pw.is_null() {
        let shell = unsafe { (*pw).pw_shell };
        if !shell.is_null() {
            let s = unsafe { std::ffi::CStr::from_ptr(shell) }
                .to_string_lossy()
                .into_owned();
            if !s.is_empty() {
                return s;
            }
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// OpenSSH's DefaultShell and the option it runs a command with (`-c` unless set), or the command interpreter
#[cfg(windows)]
fn default_shell() -> (String, String) {
    match registry_string("DefaultShell") {
        Some(shell) => (
            shell,
            registry_string("DefaultShellCommandOption").unwrap_or_else(|| "-c".into()),
        ),
        None => (
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            "/c".into(),
        ),
    }
}

#[cfg(windows)]
fn registry_string(value: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

    let wide = |s: &str| {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>()
    };
    let (key, value) = (wide(r"SOFTWARE\OpenSSH"), wide(value));
    let mut bytes = 0u32;
    let found = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if found != 0 || bytes < 2 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if read != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..end]);
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unix_login_shell_takes_l_and_a_command_after_c() {
        assert_eq!(unix_argv("/bin/zsh", None), ["/bin/zsh", "-l"]);
        assert_eq!(
            unix_argv("/bin/zsh", Some("wut p s")),
            ["/bin/zsh", "-l", "-c", "wut p s"]
        );
    }

    #[test]
    fn a_windows_shell_takes_its_option_and_the_command() {
        let bat = r"C:\ProgramData\ssh\shell.bat";
        assert_eq!(windows_argv(bat, "-c", None), [bat]);
        assert_eq!(
            windows_argv(bat, "-c", Some("wut p s")),
            [bat, "-c", "wut p s"]
        );
    }
}
