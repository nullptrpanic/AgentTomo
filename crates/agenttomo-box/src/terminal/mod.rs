//! Unix PTY primitives shared by the collector's terminal input and output.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{OpenptyResult, Winsize, openpty};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

nix::ioctl_write_ptr_bad!(set_size, nix::libc::TIOCSWINSZ, Winsize);

pub fn open() -> io::Result<OpenptyResult> {
    let pair = openpty(
        Some(&Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )?;
    for fd in [&pair.master, &pair.slave] {
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    Ok(pair)
}

/// Called only inside pre_exec, after stdin has become the PTY slave.
pub fn acquire_controlling_terminal() -> io::Result<()> {
    nix::unistd::setsid()?;
    // TIOCSCTTY takes an integer argument, not a memory pointer.
    let result = unsafe { nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) };
    nix::errno::Errno::result(result)?;
    Ok(())
}

#[derive(Clone)]
pub struct Pty(Arc<AsyncFd<OwnedFd>>);

impl Pty {
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let flags = OFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFL)?);
        fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        // OwnedFd keeps the descriptor open and stable until AsyncFd is dropped.
        Ok(Self(Arc::new(unsafe { AsyncFd::register(fd)? })))
    }

    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        if !(1..=1000).contains(&rows) || !(1..=1000).contains(&cols) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal dimensions must be between 1 and 1000",
            ));
        }
        let size = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            set_size(self.as_raw_fd(), &size)?;
        }
        Ok(())
    }
}

impl AsRawFd for Pty {
    fn as_raw_fd(&self) -> RawFd {
        self.0.get_ref().as_raw_fd()
    }
}

impl AsyncRead for Pty {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            match guard.try_io(|fd| {
                match nix::unistd::read(fd.get_ref(), buffer.initialize_unfilled()) {
                    // Linux reports EIO when the last PTY slave closes.
                    Err(nix::errno::Errno::EIO) => Ok(0),
                    result => result.map_err(io::Error::from),
                }
            }) {
                Ok(result) => {
                    buffer.advance(result?);
                    return Poll::Ready(Ok(()));
                }
                Err(_) => continue,
            }
        }
    }
}

impl AsyncWrite for Pty {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;
            match guard
                .try_io(|fd| nix::unistd::write(fd.get_ref(), bytes).map_err(io::Error::from))
            {
                Ok(result) => return Poll::Ready(result),
                Err(_) => continue,
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
