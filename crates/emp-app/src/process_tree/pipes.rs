use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::{Errno, ioctl_fionread},
};
use std::{
    io::{self, PipeReader, PipeWriter, Read, Write},
    os::fd::AsFd,
    sync::Arc,
};

pub(crate) struct Reader {
    pipe: PipeReader,
    shutdown: Arc<PipeReader>,
    drain_remaining: Option<u64>,
}

pub(crate) struct Writer {
    pipe: PipeWriter,
    shutdown: Arc<PipeReader>,
}

pub(crate) fn interruptible(
    reader: PipeReader,
    writer: PipeWriter,
) -> io::Result<(Reader, Writer, PipeWriter)> {
    for pipe in [reader.as_fd(), writer.as_fd()] {
        fcntl_setfl(pipe, fcntl_getfl(pipe)? | OFlags::NONBLOCK)?;
    }
    let (shutdown, signal) = io::pipe()?;
    let shutdown = Arc::new(shutdown);
    Ok((
        Reader {
            pipe: reader,
            shutdown: shutdown.clone(),
            drain_remaining: None,
        },
        Writer {
            pipe: writer,
            shutdown,
        },
        signal,
    ))
}

fn poll_pipe(
    pipe: &impl AsFd,
    interest: PollFlags,
    shutdown: &PipeReader,
    timeout: Option<&Timespec>,
) -> io::Result<(bool, bool)> {
    let mut fds = [
        PollFd::new(pipe, interest),
        PollFd::new(shutdown, PollFlags::IN),
    ];
    loop {
        match poll(&mut fds, timeout) {
            Ok(_) => return Ok((!fds[0].revents().is_empty(), !fds[1].revents().is_empty())),
            Err(Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.drain_remaining.is_none() {
            let (_, shutdown) = poll_pipe(&self.pipe, PollFlags::IN, &self.shutdown, None)?;
            if shutdown {
                let (readable, _) = poll_pipe(
                    &self.pipe,
                    PollFlags::IN,
                    &self.shutdown,
                    Some(&Timespec::default()),
                )?;
                self.drain_remaining = Some(if readable {
                    ioctl_fionread(&self.pipe)?
                } else {
                    0
                });
                break;
            }
            match self.pipe.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
        let remaining = self.drain_remaining.as_mut().unwrap();
        if *remaining == 0 {
            return Ok(0);
        }
        let limit = (buffer.len() as u64).min(*remaining) as usize;
        let read = self.pipe.read(&mut buffer[..limit])?;
        *remaining -= read as u64;
        Ok(read)
    }
}

impl Write for Writer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let (_, shutdown) = poll_pipe(&self.pipe, PollFlags::OUT, &self.shutdown, None)?;
            if shutdown {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            match self.pipe.write(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()
    }
}
