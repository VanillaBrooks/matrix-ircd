// Copyright 2016 Openmarket
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::ConnectionContext;

use futures3::stream::Stream;
use futures3::task::Poll;

use std::boxed::Box;
use std::fmt::Write;
use std::io::{self, Cursor};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Context;

use super::protocol::{IrcCommand, Numeric};

use tokio::io::{AsyncRead, AsyncWrite};

pub struct IrcServerConnection<S>
where
    S: AsyncRead + AsyncWrite,
{
    conn: Pin<Box<S>>,
    read_buffer: Vec<u8>,
    inner: Arc<Mutex<IrcServerConnectionInner>>,
    closed: bool,
    ctx: ConnectionContext,
    server_name: String,
}

impl<S> IrcServerConnection<S>
where
    S: AsyncWrite + AsyncRead,
{
    pub fn new(conn: S, server_name: String, context: ConnectionContext) -> IrcServerConnection<S> {
        IrcServerConnection {
            conn: Box::pin(conn),
            read_buffer: Vec::with_capacity(1024),
            inner: Arc::new(Mutex::new(IrcServerConnectionInner::new())),
            closed: false,
            ctx: context,
            server_name,
        }
    }

    pub fn write_line(&mut self, line: &str, cx: &mut Context<'_>) {
        {
            let mut inner = self.inner.lock().unwrap();

            trace!(self.ctx.logger, "Writing line"; "line" => line);

            {
                let v = inner.write_buffer.get_mut();
                v.extend_from_slice(line.as_bytes());
                v.push(b'\n');
            }
        }

        // ignore the result of the poll
        match self.poll_write(cx) {
            _ => (),
        }
    }

    pub fn write_invalid_password(&mut self, nick: &str, cx: &mut Context) {
        self.write_numeric(Numeric::ErrPasswdmismatch, nick, ":Invalid password", cx);
    }

    pub fn write_password_required(&mut self, nick: &str, cx: &mut Context) {
        self.write_numeric(
            Numeric::ErrNeedmoreparams,
            nick,
            "PASS :Password required",
            cx,
        );
    }

    pub fn write_numeric(
        &mut self,
        numeric: Numeric,
        nick: &str,
        rest_of_line: &str,
        cx: &mut Context,
    ) {
        let line = format!(
            ":{} {} {} {}",
            &self.server_name,
            numeric.as_str(),
            nick,
            rest_of_line
        );
        self.write_line(&line, cx);
    }

    pub fn welcome(&mut self, nick: &str, cx: &mut Context) {
        self.write_numeric(
            Numeric::RplWelcome,
            nick,
            ":Welcome to the Matrix Internet Relay Network",
            cx,
        );

        let motd_start = format!(":- {} Message of the day -", self.server_name);
        self.write_numeric(Numeric::RplMotdstart, nick, &motd_start, cx);
        self.write_numeric(Numeric::RplMotd, nick, ":-", cx);
        self.write_numeric(
            Numeric::RplMotd,
            nick,
            ":- This is a bridge into Matrix",
            cx,
        );
        self.write_numeric(Numeric::RplMotd, nick, ":-", cx);
        self.write_numeric(Numeric::RplEndofmotd, nick, ":End of MOTD", cx);
    }

    pub fn write_join(&mut self, nick: &str, channel: &str, cx: &mut Context) {
        let line = format!(":{} JOIN {}", nick, channel);
        self.write_line(&line, cx);
    }

    pub fn write_topic(&mut self, nick: &str, channel: &str, topic: &str, cx: &mut Context) {
        self.write_numeric(
            Numeric::RplTopic,
            nick,
            &format!("{} :{}", channel, topic),
            cx,
        );
    }

    pub fn write_names(
        &mut self,
        nick: &str,
        channel: &str,
        names: &[(&String, bool)],
        cx: &mut Context,
    ) {
        for iter in names.chunks(10) {
            let mut line = format!("@ {} :", channel);
            for &(nick, op) in iter {
                write!(line, "{}{} ", if op { "@" } else { "" }, &nick).unwrap();
            }
            let line = line.trim();
            self.write_numeric(Numeric::RplNamreply, nick, &line, cx);
        }
        self.write_numeric(
            Numeric::RplEndofnames,
            nick,
            &format!("{} :End of /NAMES", channel),
            cx,
        );
    }

    fn poll_read(&mut self, cx: &mut Context) -> Poll<Result<IrcCommand, io::Error>> {
        loop {
            while let Some(pos) = self.read_buffer.iter().position(|&c| c == b'\n') {
                let to_return = self.read_buffer.drain(..pos + 1).collect();
                match String::from_utf8(to_return) {
                    Ok(line) => {
                        let line = line.trim_end().to_string();
                        if let Ok(irc_line) = line.parse() {
                            trace!(self.ctx.logger, "Got IRC line"; "line" => line);
                            return Poll::Ready(Ok(irc_line));
                        } else {
                            warn!(self.ctx.logger, "Invalid IRC line"; "line" => line);
                        }
                    }
                    Err(_) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Invalid UTF-8",
                        )))
                    }
                }
            }

            let start_len = self.read_buffer.len();
            if start_len >= 2048 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Line too long",
                )));
            }
            self.read_buffer.resize(2048, 0);
            match self
                .conn
                .as_mut()
                .poll_read(cx, &mut self.read_buffer[start_len..])
            {
                Poll::Ready(Ok(0)) => {
                    debug!(self.ctx.logger, "Closed");
                    self.closed = true;
                    self.read_buffer.resize(start_len, 0);
                    return Poll::Pending;
                }
                Poll::Ready(Ok(bytes_read)) => {
                    self.read_buffer.resize(start_len + bytes_read, 0);
                }
                Poll::Ready(Err(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.read_buffer.resize(start_len, 0);
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => return Poll::Pending,
            };
        }
    }

    fn poll_write(&mut self, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        loop {
            let mut inner = self.inner.lock().unwrap();

            if inner.write_buffer.get_ref().is_empty() {
                return Poll::Ready(Ok(()));
            }

            let pos = inner.write_buffer.position() as usize;
            if inner.write_buffer.get_ref().len() - pos == 0 {
                inner.write_buffer.get_mut().clear();
                inner.write_buffer.set_position(0);
                return Poll::Ready(Ok(()));
            }

            let bytes_written = {
                let to_write = &inner.write_buffer.get_ref()[pos..];

                match self.conn.as_mut().poll_write(cx, to_write)? {
                    Poll::Ready(bytes_written) => bytes_written,
                    Poll::Pending => return Poll::Pending,
                }
            };

            inner
                .write_buffer
                .set_position((pos + bytes_written) as u64);

            match self.conn.as_mut().poll_flush(cx)? {
                Poll::Ready(_) => (),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite> Stream for IrcServerConnection<S> {
    type Item = Result<IrcCommand, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        trace!(self.ctx.logger, "IRC Polled");

        if self.closed {
            return Poll::Ready(None);
        }

        match self.poll_write(cx)? {
            Poll::Ready(_) => (),
            Poll::Pending => return Poll::Pending,
        };

        if let Poll::Ready(line) = self.poll_read(cx)? {
            //return Ok(Async::Ready(Some(line)));
            return Poll::Ready(Some(Ok(line)));
        }

        if self.closed {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug, Clone)]
struct IrcServerConnectionInner {
    write_buffer: Cursor<Vec<u8>>,
}

impl IrcServerConnectionInner {
    pub fn new() -> IrcServerConnectionInner {
        IrcServerConnectionInner {
            write_buffer: Cursor::new(Vec::with_capacity(1024)),
        }
    }
}
