use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::proto::{Size, Spawn};

pub struct Reader {
    file: File,
    stop: Arc<AtomicBool>,
}

pub struct Writer(Option<File>);

pub struct Control {
    master: File,
    pid: i32,
    child: Mutex<Option<Child>>,
    done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

fn winsize(size: Size) -> libc::winsize {
    libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn cloexec(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn spawn(s: &Spawn) -> io::Result<(Vec<Reader>, Writer, Control)> {
    let mut ws = winsize(s.size);
    let (mut m, mut sl) = (-1, -1);
    if unsafe {
        libc::openpty(
            &mut m,
            &mut sl,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // linux takes the size as const, macos as mut
            std::ptr::addr_of_mut!(ws) as _,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(m) };
    let slave = unsafe { OwnedFd::from_raw_fd(sl) };
    cloexec(m)?;
    cloexec(sl)?;

    let mut cmd = Command::new(&s.argv[0]);
    cmd.args(&s.argv[1..]);
    let cwd = if !s.cwd.is_empty() && Path::new(&s.cwd).is_dir() {
        s.cwd.clone().into()
    } else {
        crate::sock::home()
    };
    cmd.current_dir(cwd);
    cmd.envs(s.env.iter().map(|(k, v)| (k, v)));
    let mut input = None;
    if s.stdin_tty {
        cmd.stdin(Stdio::from(slave.try_clone()?));
    } else {
        let (read, write) = std::io::pipe()?;
        cmd.stdin(read);
        input = Some(File::from(OwnedFd::from(write)));
    }
    let mut piped_out = None;
    if s.stdout_tty {
        cmd.stdout(Stdio::from(slave.try_clone()?));
    } else {
        let (read, write) = std::io::pipe()?;
        cmd.stdout(write);
        piped_out = Some(File::from(OwnedFd::from(read)));
    }
    cmd.stderr(Stdio::from(slave));
    // its own session, with the pty as its controlling terminal: job control, ^C and SIGWINCH reach it as they would
    // from a terminal window. stderr is the pty by the time this runs, whatever stdin and stdout are
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(2, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // the slave's copies go with the command: once the child's are gone too, a read of the master ends
    drop(cmd);

    let master = File::from(master);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = |file| Reader {
        file,
        stop: stop.clone(),
    };
    let mut readers = vec![reader(master.try_clone()?)];
    readers.extend(piped_out.map(reader));
    let writer = Writer(Some(match input {
        Some(pipe) => pipe,
        None => master.try_clone()?,
    }));
    let pid = child.id() as i32;
    Ok((
        readers,
        writer,
        Control {
            master,
            pid,
            child: Mutex::new(Some(child)),
            done: Arc::new(AtomicBool::new(false)),
            stop,
        },
    ))
}

impl Read for Reader {
    /// a read that waits a tenth of a second at a time, so a stop ends it even with nothing to read
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let mut fd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 100) } == 0 {
                continue;
            }
            return match self.file.read(buf) {
                // linux ends a master whose slave is closed with EIO rather than a 0
                Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(0),
                r => r,
            };
        }
    }
}

impl Writer {
    /// the end of the command's stdin, when it is a pipe; a pty's has no end but its close
    pub fn close(&mut self) {
        self.0 = None;
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.as_mut() {
            Some(f) => f.write(buf),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.as_mut().map_or(Ok(()), |f| f.flush())
    }
}

impl Control {
    pub fn resize(&self, size: Size) -> io::Result<()> {
        let ws = winsize(size);
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// the exit code, or 128 + the signal that ended it, as a shell reports one
    pub fn wait(&self) -> io::Result<i32> {
        let mut child = self
            .child
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| io::Error::other("already waited"))?;
        let status = child.wait()?;
        self.done.store(true, Ordering::SeqCst);
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }

    /// what a closed terminal does: a hangup to its session, then a kill for one that ignores it
    pub fn kill(&self) {
        unsafe { libc::kill(-self.pid, libc::SIGHUP) };
        let pid = self.pid;
        let done = self.done.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            if !done.load(Ordering::SeqCst) {
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        });
    }

    /// a unix pty has nothing to close beyond its fds
    pub fn close(&self) {}

    /// the readers end, though something the command left behind still holds the terminal: once they and the server
    /// let go of the master, that holder gets the hangup a closed terminal gives, as sshd's sessions do
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
