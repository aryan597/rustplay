//! CDP over `--remote-debugging-pipe`.
//!
//! Chrome reads commands from fd 3 and writes responses/events to fd 4, each
//! message a JSON document terminated by a NUL byte. No socket is created, so
//! no other local process can attach to the browser.
//!
//! This module provides:
//! - [`ChildPipes`]: the two pipe pairs, and the async-signal-safe `pre_exec`
//!   step that installs Chrome's ends on fds 3/4 in the child.
//! - [`PipeTransport`]: rustplay's ends, adapted to chromiumoxide's
//!   `CdpTransport` (patched in `vendor/chromiumoxide`).

use chromiumoxide::conn::{WsError, WsMessage};
use futures::channel::mpsc;
use futures::{Sink, SinkExt, Stream, StreamExt};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::task::JoinHandle;

/// Largest single CDP message accepted (full-page screenshots are the big ones).
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;

/// Both pipes. Chrome's ends are moved into the child; rustplay keeps the rest.
pub struct ChildPipes {
    /// Chrome reads commands here (becomes fd 3 in the child).
    child_cmd_read: OwnedFd,
    /// Chrome writes responses here (becomes fd 4 in the child).
    child_resp_write: OwnedFd,
    /// rustplay writes commands.
    cmd_write: OwnedFd,
    /// rustplay reads responses.
    resp_read: OwnedFd,
}

impl ChildPipes {
    pub fn new() -> io::Result<Self> {
        // std::io::pipe creates both ends with O_CLOEXEC, so nothing leaks
        // into the child except what pre_exec deliberately installs.
        let (cmd_r, cmd_w) = std::io::pipe()?;
        let (resp_r, resp_w) = std::io::pipe()?;
        Ok(Self {
            child_cmd_read: cmd_r.into(),
            child_resp_write: resp_w.into(),
            cmd_write: cmd_w.into(),
            resp_read: resp_r.into(),
        })
    }

    /// Raw fds for the child-side step. Valid until `into_parent_ends`.
    pub fn child_fds(&self) -> (RawFd, RawFd) {
        (self.child_cmd_read.as_raw_fd(), self.child_resp_write.as_raw_fd())
    }

    /// After spawn: close Chrome's ends in this process (so EOF propagates when
    /// Chrome exits) and wrap ours for tokio.
    pub fn into_transport(self) -> io::Result<PipeTransport> {
        drop(self.child_cmd_read);
        drop(self.child_resp_write);
        let tx = pipe::Sender::from_owned_fd(self.cmd_write)?;
        let rx = pipe::Receiver::from_owned_fd(self.resp_read)?;
        Ok(PipeTransport::spawn(rx, tx))
    }
}

/// Runs in the forked child before exec. Only async-signal-safe calls.
///
/// Installs `cmd_read` as fd 3 and `resp_write` as fd 4. Both sources are first
/// duplicated to fds >= 10 so that a source already numbered 3 or 4 cannot be
/// clobbered by the other `dup2` (the classic swap bug). `dup2` clears
/// FD_CLOEXEC on the targets, so exactly fds 3 and 4 survive exec.
///
/// # Safety
/// Must only be called in the child between fork and exec.
pub unsafe fn install_child_fds(cmd_read: RawFd, resp_write: RawFd) -> io::Result<()> {
    let a = libc::fcntl(cmd_read, libc::F_DUPFD_CLOEXEC, 10);
    if a < 0 {
        return Err(io::Error::last_os_error());
    }
    let b = libc::fcntl(resp_write, libc::F_DUPFD_CLOEXEC, 10);
    if b < 0 {
        return Err(io::Error::last_os_error());
    }
    if libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
        return Err(io::Error::last_os_error());
    }
    libc::close(a);
    libc::close(b);
    Ok(())
}

/// Splits a byte stream into NUL-terminated frames.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// Feed bytes; returns complete frames. Errors on oversize or non-UTF-8.
    pub fn push(&mut self, chunk: &[u8]) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|b| *b == 0) {
            self.buf.extend_from_slice(&rest[..pos]);
            let frame = std::mem::take(&mut self.buf);
            out.push(String::from_utf8(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?);
            rest = &rest[pos + 1..];
        }
        self.buf.extend_from_slice(rest);
        if self.buf.len() > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "CDP message exceeds 256 MiB"));
        }
        Ok(out)
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// rustplay's side of the pipes, as a chromiumoxide transport.
///
/// Two small tasks do the I/O; the transport itself is just channels, which
/// keeps it `Send + Unpin` and lets the Handler poll it like a websocket.
/// Dropping the transport aborts both tasks and closes the pipes, which makes
/// Chrome exit.
pub struct PipeTransport {
    incoming: mpsc::Receiver<Result<WsMessage, WsError>>,
    outgoing: mpsc::Sender<WsMessage>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl PipeTransport {
    fn spawn(mut rx: pipe::Receiver, mut tx: pipe::Sender) -> Self {
        let (mut in_tx, incoming) = mpsc::channel::<Result<WsMessage, WsError>>(64);
        let (outgoing, mut out_rx) = mpsc::channel::<WsMessage>(64);

        let reader = tokio::spawn(async move {
            let mut dec = FrameDecoder::default();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match rx.read(&mut buf).await {
                    Ok(0) => {
                        tracing::debug!(pending = dec.pending(), "cdp pipe: browser closed fd 4");
                        break; // channel drop => stream ends => Handler sees disconnect
                    }
                    Ok(n) => match dec.push(&buf[..n]) {
                        Ok(frames) => {
                            for f in frames {
                                if in_tx.send(Ok(WsMessage::text(f))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = in_tx.send(Err(WsError::Io(e))).await;
                            return;
                        }
                    },
                    Err(e) => {
                        let _ = in_tx.send(Err(WsError::Io(e))).await;
                        return;
                    }
                }
            }
        });

        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.next().await {
                let bytes = match msg {
                    WsMessage::Text(t) => t.as_str().as_bytes().to_vec(),
                    WsMessage::Binary(b) => b.to_vec(),
                    WsMessage::Close(_) => break,
                    _ => continue,
                };
                if bytes.contains(&0) {
                    tracing::error!("cdp pipe: refusing to send a message containing NUL");
                    continue;
                }
                if tx.write_all(&bytes).await.is_err() || tx.write_all(&[0]).await.is_err() {
                    break;
                }
            }
            // Dropping `tx` closes fd 3 in Chrome: it shuts down.
        });

        Self { incoming, outgoing, reader, writer }
    }
}

impl Drop for PipeTransport {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

impl Stream for PipeTransport {
    type Item = Result<WsMessage, WsError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.incoming.poll_next_unpin(cx)
    }
}

fn closed() -> WsError {
    WsError::ConnectionClosed
}

impl Sink<WsMessage> for PipeTransport {
    type Error = WsError;
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        self.outgoing.poll_ready_unpin(cx).map_err(|_| closed())
    }
    fn start_send(mut self: Pin<&mut Self>, item: WsMessage) -> Result<(), WsError> {
        self.outgoing.start_send_unpin(item).map_err(|_| closed())
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        self.outgoing.poll_flush_unpin(cx).map_err(|_| closed())
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        self.outgoing.poll_close_unpin(cx).map_err(|_| closed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_handles_split_and_batched_frames() {
        let mut d = FrameDecoder::default();
        assert!(d.push(br#"{"id":1,"res"#).unwrap().is_empty());
        let f = d.push(b"ult\":{}}\0{\"id\":2}\0{\"id\"").unwrap();
        assert_eq!(f, vec![r#"{"id":1,"result":{}}"#.to_string(), r#"{"id":2}"#.to_string()]);
        assert_eq!(d.pending(), 5);
        assert_eq!(d.push(b":3}\0").unwrap(), vec![r#"{"id":3}"#.to_string()]);
        assert_eq!(d.pending(), 0);
    }

    #[test]
    fn decoder_rejects_bad_utf8() {
        let mut d = FrameDecoder::default();
        assert!(d.push(&[0xff, 0xfe, 0]).is_err());
    }

    #[tokio::test]
    async fn transport_round_trip_over_real_pipes() {
        // Play the browser's role on the child ends.
        let pipes = ChildPipes::new().unwrap();
        let (cr, cw) = pipes.child_fds();
        // Duplicate the child ends so they survive `into_transport` closing them.
        let browser_read: std::fs::File = unsafe { std::os::fd::FromRawFd::from_raw_fd(libc::dup(cr)) };
        let browser_write: std::fs::File = unsafe { std::os::fd::FromRawFd::from_raw_fd(libc::dup(cw)) };
        let mut t = pipes.into_transport().unwrap();

        t.send(WsMessage::text(r#"{"id":7,"method":"Browser.getVersion"}"#)).await.unwrap();
        let got = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut r = browser_read;
            let mut buf = Vec::new();
            let mut b = [0u8; 1];
            while r.read(&mut b).unwrap() == 1 && b[0] != 0 {
                buf.push(b[0]);
            }
            String::from_utf8(buf).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(got, r#"{"id":7,"method":"Browser.getVersion"}"#);

        {
            use std::io::Write;
            let mut w = &browser_write;
            w.write_all(b"{\"id\":7,\"result\":{}}\0{\"method\":\"X\"}\0").unwrap();
        }
        let a = t.next().await.unwrap().unwrap();
        let b = t.next().await.unwrap().unwrap();
        assert_eq!(a.into_text().unwrap().as_str(), r#"{"id":7,"result":{}}"#);
        assert_eq!(b.into_text().unwrap().as_str(), r#"{"method":"X"}"#);

        drop(browser_write); // browser exits
        assert!(t.next().await.is_none(), "EOF must end the stream");
    }
}
