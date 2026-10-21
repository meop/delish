use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
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
pub struct Writer(Owned);

pub struct Control {
    process: Owned,
    console: Mutex<Option<Console>>,
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

/// the command runs with the server's own environment: it is the desktop session's, which is the point. what the
/// client sends (its TERM) is for a unix terminal and is not applied here
pub fn spawn(s: &Spawn) -> io::Result<(Reader, Writer, Control)> {
    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;
    let mut console: HPCON = 0 as HPCON;
    let hr = unsafe { CreatePseudoConsole(coord(s.size), in_read.0, out_write.0, 0, &mut console) };
    if hr < 0 {
        return Err(io::Error::from_raw_os_error(hr));
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
    // invalid handles of its own, so the child never takes the server's stdio for its terminal
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
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
            0,
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

    Ok((
        Reader(out_read),
        Writer(in_write),
        Control {
            process: Owned(pi.hProcess),
            console: Mutex::new(Some(console)),
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

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut n = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        if unsafe { WriteFile(self.0.0, buf.as_ptr(), len, &mut n, ptr::null_mut()) } == 0 {
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
                return Err(io::Error::from_raw_os_error(hr));
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

    /// the console's host flushes what is left and exits, which ends the reader. it can wait on that output being
    /// read, so the reader keeps going while this runs
    pub fn close(&self) {
        if let Some(c) = self.console.lock().unwrap().take() {
            unsafe { ClosePseudoConsole(c.0) };
        }
    }
}
