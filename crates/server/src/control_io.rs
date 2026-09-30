// SPDX-License-Identifier: MPL-2.0

//! 控制面可中断 IO：非阻塞 socket + 系统就绪事件。
//! 不设置 SO_RCVTIMEO；每次等待至多 100 ms，检查关闭标志和总认证截止时间。

use polling::{Event, Events, Poller};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const CANCEL_POLL: Duration = Duration::from_millis(100);

pub(crate) struct IoControl {
    sock: TcpStream,
    closed: AtomicBool,
    auth_deadline: Mutex<Option<Instant>>,
}

impl IoControl {
    pub fn new(sock: TcpStream, auth_deadline: Option<Instant>) -> Self {
        Self {
            sock,
            closed: AtomicBool::new(false),
            auth_deadline: Mutex::new(auth_deadline),
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.sock.shutdown(Shutdown::Both);
    }

    pub fn authenticated(&self) {
        *self.auth_deadline.lock().expect("deadline poisoned") = None;
    }

    fn check(&self) -> io::Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "连接已关闭",
            ));
        }
        if self
            .auth_deadline
            .lock()
            .expect("deadline poisoned")
            .is_some_and(|d| Instant::now() >= d)
        {
            self.close();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "连接认证超时"));
        }
        Ok(())
    }
}

pub(crate) struct SocketIo {
    sock: TcpStream,
    pub control: Arc<IoControl>,
    poller: Poller,
    events: Events,
    write_deadline: Option<Instant>,
}

impl SocketIo {
    pub fn new(sock: TcpStream, control: Arc<IoControl>) -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        let poller = Poller::new()?;
        // SAFETY: sock 由本对象持有，Drop 先 delete，再释放 socket。
        unsafe {
            poller.add(&sock, Event::none(0))?;
        }
        Ok(Self {
            sock,
            control,
            poller,
            events: Events::new(),
            write_deadline: None,
        })
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Self::new(self.sock.try_clone()?, Arc::clone(&self.control))
    }

    pub fn begin_write(&mut self) {
        self.write_deadline = Some(Instant::now() + WRITE_TIMEOUT);
    }

    fn check(&self, writing: bool) -> io::Result<()> {
        self.control.check()?;
        if writing && self.write_deadline.is_some_and(|d| Instant::now() >= d) {
            self.control.close();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "控制消息写入超时"));
        }
        Ok(())
    }

    fn wait(&mut self, writing: bool) -> io::Result<()> {
        self.check(writing)?;
        self.poller.modify(
            &self.sock,
            if writing {
                Event::writable(0)
            } else {
                Event::readable(0)
            },
        )?;
        self.events.clear();
        match self.poller.wait(&mut self.events, Some(CANCEL_POLL)) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
            result => result.map(|_| ()),
        }
    }
}

impl Drop for SocketIo {
    fn drop(&mut self) {
        let _ = self.poller.delete(&self.sock);
    }
}

impl Read for SocketIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check(false)?;
            match self.sock.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(false)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}

impl Write for SocketIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            self.check(true)?;
            match self.sock.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(true)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc::channel;

    fn pair() -> (SocketIo, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (sock, _) = listener.accept().unwrap();
        let control = Arc::new(IoControl::new(sock.try_clone().unwrap(), None));
        (SocketIo::new(sock, control).unwrap(), client)
    }

    #[test]
    fn closing_interrupts_a_pending_read_without_peer_activity() {
        let (mut io, _client) = pair();
        let control = Arc::clone(&io.control);
        let (started, ready) = channel();
        let (done, finished) = channel();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            done.send(io.read(&mut [0; 1])).unwrap();
        });
        ready.recv().unwrap();
        control.close();
        assert!(matches!(
            finished.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(0) | Err(_)
        ));
        thread.join().unwrap();
    }

    #[test]
    fn a_nonreading_peer_hits_the_total_write_deadline() {
        let (mut io, _nonreading_client) = pair();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            use windows_sys::Win32::Networking::WinSock::{
                setsockopt, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF,
            };
            let size: i32 = 4096;
            for (sock, option) in [(&io.sock, SO_SNDBUF), (&_nonreading_client, SO_RCVBUF)] {
                // SAFETY: socket 和 i32 缓冲区在同步调用期间有效。
                assert_eq!(
                    unsafe {
                        setsockopt(
                            sock.as_raw_socket() as usize,
                            SOL_SOCKET,
                            option,
                            (&size as *const i32).cast(),
                            4,
                        )
                    },
                    0
                );
            }
        }
        io.begin_write();
        let started = Instant::now();
        let chunk = [0; 64 * 1024];
        let error = loop {
            if let Err(error) = io.write_all(&chunk) {
                break error;
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < WRITE_TIMEOUT + Duration::from_secs(1));
    }
}
