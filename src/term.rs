//! The client's own terminal: its size, and raw mode while a command runs, so every key goes to the command's
//! terminal as typed and nothing is echoed twice. Raw mode is put back when the guard drops.

use crate::proto::Size;

pub use os::{Raw, is_tty, size, stderr_is_tty, stdout_is_tty};

#[cfg(unix)]
mod os {
    use super::Size;

    pub fn is_tty() -> bool {
        unsafe { libc::isatty(0) == 1 }
    }

    pub fn stdout_is_tty() -> bool {
        unsafe { libc::isatty(1) == 1 }
    }

    pub fn stderr_is_tty() -> bool {
        unsafe { libc::isatty(2) == 1 }
    }

    pub fn size() -> Option<Size> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        for fd in [1, 0, 2] {
            if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } == 0
                && ws.ws_row > 0
                && ws.ws_col > 0
            {
                return Some(Size {
                    rows: ws.ws_row,
                    cols: ws.ws_col,
                });
            }
        }
        None
    }

    pub struct Raw(libc::termios);

    impl Raw {
        pub fn enter() -> Option<Raw> {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
                return None;
            }
            let saved = t;
            unsafe { libc::cfmakeraw(&mut t) };
            if unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) } != 0 {
                return None;
            }
            Some(Raw(saved))
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
        }
    }
}

#[cfg(windows)]
mod os {
    use super::Size;
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
        ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleScreenBufferInfo,
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    pub fn is_tty() -> bool {
        let mut mode: CONSOLE_MODE = 0;
        unsafe { GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut mode) != 0 }
    }

    pub fn stdout_is_tty() -> bool {
        let mut mode: CONSOLE_MODE = 0;
        unsafe { GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut mode) != 0 }
    }

    pub fn stderr_is_tty() -> bool {
        let mut mode: CONSOLE_MODE = 0;
        unsafe { GetConsoleMode(GetStdHandle(STD_ERROR_HANDLE), &mut mode) != 0 }
    }

    pub fn size() -> Option<Size> {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) } == 0 {
            return None;
        }
        let w = info.srWindow;
        let (rows, cols) = ((w.Bottom - w.Top + 1) as u16, (w.Right - w.Left + 1) as u16);
        (rows > 0 && cols > 0).then_some(Size { rows, cols })
    }

    /// keys arrive as the sequences a unix terminal sends, and output is read as one: the command's console speaks
    /// VT both ways
    pub struct Raw {
        input: CONSOLE_MODE,
        output: CONSOLE_MODE,
    }

    impl Raw {
        pub fn enter() -> Option<Raw> {
            let (hin, hout) = unsafe {
                (
                    GetStdHandle(STD_INPUT_HANDLE),
                    GetStdHandle(STD_OUTPUT_HANDLE),
                )
            };
            let (mut input, mut output) = (0, 0);
            if unsafe { GetConsoleMode(hin, &mut input) } == 0
                || unsafe { GetConsoleMode(hout, &mut output) } == 0
            {
                return None;
            }
            let raw_in = (input
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            let raw_out = output | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;
            unsafe {
                SetConsoleMode(hin, raw_in);
                SetConsoleMode(hout, raw_out);
            }
            Some(Raw { input, output })
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.input);
                SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), self.output);
            }
        }
    }
}
