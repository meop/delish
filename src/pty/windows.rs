use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::proto::{Size, Spawn};

/// a handle closed when dropped; a raw HANDLE is a pointer, so it is not Send without saying so
struct Owned(HANDLE);

unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct Console(HPCON);

unsafe impl Send for Console {}

pub struct Reader(Owned);
pub struct Writer(Option<Owned>);

pub struct Control {
    process: Owned,
    console: Mutex<Option<Console>>,
    // the console's input, kept open while the command's stdin is a pipe instead
    _console_input: Option<Owned>,
}

fn coord(size: Size) -> COORD {
    COORD {
        X: size.cols as i16,
        Y: size.rows as i16,
    }
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

fn pipe() -> io::Result<(Owned, Owned)> {
    let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((Owned(read), Owned(write)))
}

/// a pipe one end of which the child inherits, as its stdin (`child_reads`) or its stdout
fn child_pipe(child_reads: bool) -> io::Result<(Owned, Owned)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, &sa, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let (child, ours) = if child_reads {
        (read, write)
    } else {
        (write, read)
    };
    unsafe { SetHandleInformation(ours, HANDLE_FLAG_INHERIT, 0) };
    Ok((Owned(child), Owned(ours)))
}

/// an HRESULT as the error it carries: a FACILITY_WIN32 one is a Win32 error code in its low word
fn hresult(hr: i32) -> io::Error {
    if (hr as u32) & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr as u32 & 0xFFFF) as i32)
    } else {
        io::Error::other(format!("HRESULT {:#010x}", hr as u32))
    }
}

/// the command runs with the server's own environment: it is the desktop session's, which is the point. what the
/// client sends (its TERM) is for a unix terminal and is not applied here
pub fn spawn(s: &Spawn) -> io::Result<(Vec<Reader>, Writer, Control)> {
    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;
    let mut console: HPCON = 0 as HPCON;
    let hr = unsafe { CreatePseudoConsole(coord(s.size), in_read.0, out_write.0, 0, &mut console) };
    if hr < 0 {
        return Err(hresult(hr));
    }
    let console = Console(console);
    // the console holds its own ends now
    drop(in_read);
    drop(out_write);

    let mut size = 0usize;
    unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
    let mut attrs = vec![0u8; size];
    let list = attrs.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let set = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            console.0 as *const c_void,
            std::mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null(),
        )
    };
    if set == 0 {
        let e = io::Error::last_os_error();
        unsafe { DeleteProcThreadAttributeList(list) };
        return Err(e);
    }

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // a pipe where the client has no terminal, and otherwise an invalid handle, which the child takes the console
    // for, so it never takes the server's stdio
    let stdin_pipe = (!s.stdin_tty).then(|| child_pipe(true)).transpose()?;
    let stdout_pipe = (!s.stdout_tty).then(|| child_pipe(false)).transpose()?;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdin_pipe
        .as_ref()
        .map_or(INVALID_HANDLE_VALUE, |(child, _)| child.0);
    si.StartupInfo.hStdOutput = stdout_pipe
        .as_ref()
        .map_or(INVALID_HANDLE_VALUE, |(child, _)| child.0);
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    let inherit = (stdin_pipe.is_some() || stdout_pipe.is_some()) as i32;
    si.lpAttributeList = list;

    let mut cmdline = wide(&crate::cmdline::command_line(&s.argv)?);
    let cwd = if !s.cwd.is_empty() && Path::new(&s.cwd).is_dir() {
        s.cwd.clone()
    } else {
        crate::sock::home().to_string_lossy().into_owned()
    };
    let cwd = wide(&cwd);
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let made = unsafe {
        CreateProcessW(
            ptr::null(),
            cmdline.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            inherit,
            EXTENDED_STARTUPINFO_PRESENT,
            ptr::null(),
            cwd.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    let made_err = io::Error::last_os_error();
    unsafe { DeleteProcThreadAttributeList(list) };
    if made == 0 {
        unsafe { ClosePseudoConsole(console.0) };
        return Err(made_err);
    }
    drop(Owned(pi.hThread));

    let mut readers = vec![Reader(out_read)];
    let (writer, console_input) = match stdin_pipe {
        Some((_child, ours)) => (Writer(Some(ours)), Some(in_write)),
        None => (Writer(Some(in_write)), None),
    };
    if let Some((_child, ours)) = stdout_pipe {
        readers.push(Reader(ours));
    }
    Ok((
        readers,
        writer,
        Control {
            process: Owned(pi.hProcess),
            console: Mutex::new(Some(console)),
            _console_input: console_input,
        },
    ))
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        if unsafe { ReadFile(self.0.0, buf.as_mut_ptr(), len, &mut n, ptr::null_mut()) } == 0 {
            // the console closed its end: the same as a 0
            return Ok(0);
        }
        Ok(n as usize)
    }
}

impl Writer {
    /// the end of the command's stdin, when it is a pipe
    pub fn close(&mut self) {
        self.0 = None;
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(h) = self.0.as_ref() else {
            return Err(io::ErrorKind::BrokenPipe.into());
        };
        let mut n = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        if unsafe { WriteFile(h.0, buf.as_ptr(), len, &mut n, ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Control {
    pub fn resize(&self, size: Size) -> io::Result<()> {
        if let Some(c) = self.console.lock().unwrap().as_ref() {
            let hr = unsafe { ResizePseudoConsole(c.0, coord(size)) };
            if hr < 0 {
                return Err(hresult(hr));
            }
        }
        Ok(())
    }

    pub fn wait(&self) -> io::Result<i32> {
        if unsafe { WaitForSingleObject(self.process.0, INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0u32;
        if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code as i32)
    }

    pub fn kill(&self) {
        unsafe { TerminateProcess(self.process.0, 1) };
    }

    /// the console's own output ends when it closes: there is nothing more to stop
    pub fn stop(&self) {}

    /// the console's host flushes what is left and exits, which ends the reader. it can wait on that output being
    /// read, so the reader keeps going while this runs
    pub fn close(&self) {
        if let Some(c) = self.console.lock().unwrap().take() {
            unsafe { ClosePseudoConsole(c.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_hresult_for_a_win32_error_is_that_error() {
        let e = hresult(0x8007_0005_u32 as i32);
        assert_eq!(e.raw_os_error(), Some(5));
        assert!(
            hresult(0x8000_4005_u32 as i32)
                .to_string()
                .contains("0x80004005")
        );
    }
}
