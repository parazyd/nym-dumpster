/*
 * Copyright (C) 2023 parazyd <parazyd@dyne.org>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_std::net::TcpStream;
use async_tungstenite::async_std::connect_async;
use async_tungstenite::{tungstenite::protocol::Message, WebSocketStream};
use futures::{AsyncRead, AsyncWrite, FutureExt, SinkExt, StreamExt};
use nym_sphinx::addressing::clients::Recipient;
use nym_websocket::{requests::ClientRequest, responses::ServerResponse};
use rand::{rngs::OsRng, Rng};

use super::{ws_to_io_error, MessageType};

/// A Nym client implementing [`AsyncRead`] and [`AsyncWrite`]
pub struct NymClient {
    /// The underlying websocket stream connecting to nym-client
    stream: WebSocketStream<TcpStream>,
    /// The actual recipient we want to send data to
    endpoint: Recipient,
    /// The connection ID representing this stream.
    /// Only the messages containing this ID will actually be read by
    /// this instance. Other messages should be read by other instances.
    conn_id: u64,
}

impl NymClient {
    pub async fn connect(endpoint: Recipient) -> io::Result<Self> {
        // Connect to nym-client with websocket
        let (mut stream, _) = match connect_async("ws://127.0.0.1:1977").await {
            Ok(s) => s,
            Err(e) => return Err(ws_to_io_error(e)),
        };

        // Generate a new connection ID
        let conn_id = OsRng::gen::<u64>(&mut OsRng);

        // We will send the receiver a message telling them we opened a connection
        let payload = Message::Binary(Self::msg_open(endpoint, conn_id).serialize());
        if let Err(e) = stream.send(payload).await {
            return Err(ws_to_io_error(e));
        }

        Ok(Self {
            stream,
            endpoint,
            conn_id,
        })
    }

    /// Attempt to cleanly close the active connection
    pub async fn close(&mut self) -> io::Result<()> {
        // We will send the receiver a message telling them we're closing our connection
        let payload = Message::Binary(Self::msg_close(self.endpoint, self.conn_id).serialize());
        if let Err(e) = self.stream.send(payload).await {
            return Err(ws_to_io_error(e));
        }

        // Close the websocket stream cleanly
        if let Err(e) = self.stream.close(None).await {
            return Err(ws_to_io_error(e));
        }

        Ok(())
    }

    /// Return the inner connection ID
    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    /// Construct an `Open` message. Used for instantiating a connection.
    fn msg_open(recipient: Recipient, conn_id: u64) -> ClientRequest {
        let mut message = Vec::with_capacity(9);
        message.push(MessageType::Open as u8);
        message.extend_from_slice(&conn_id.to_be_bytes());

        ClientRequest::SendAnonymous {
            recipient,
            message,
            reply_surbs: 5,
            connection_id: Some(conn_id),
        }
    }

    /// Construct a `Close` message. Used when closing the connection.
    fn msg_close(recipient: Recipient, conn_id: u64) -> ClientRequest {
        let mut message = Vec::with_capacity(9);
        message.push(MessageType::Close as u8);
        message.extend_from_slice(&conn_id.to_be_bytes());

        ClientRequest::SendAnonymous {
            recipient,
            message,
            reply_surbs: 5,
            connection_id: Some(conn_id),
        }
    }

    /// Construct a `Data` message. Used when sending data into the connection.
    fn msg_data(recipient: Recipient, conn_id: u64, data: &[u8]) -> ClientRequest {
        let mut message = Vec::with_capacity(9 + data.len());
        message.push(MessageType::Data as u8);
        message.extend_from_slice(&conn_id.to_be_bytes());
        message.extend_from_slice(data);

        ClientRequest::SendAnonymous {
            recipient,
            message,
            reply_surbs: 5,
            connection_id: Some(conn_id),
        }
    }
}

impl AsyncRead for NymClient {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let message = futures::ready!(self.stream.next().poll_unpin(cx));

        let payload = match message {
            Some(Ok(Message::Binary(data))) => data,
            Some(Ok(_)) => return Poll::Pending, // ignore non-binary messages
            Some(Err(e)) => return Poll::Ready(Err(ws_to_io_error(e))),
            None => return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into())),
        };

        // We got _some_ data. Let's see what to do with it.
        let response = match ServerResponse::deserialize(&payload) {
            Ok(resp) => resp,
            Err(e) => return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e.to_string()))),
        };

        // Now we see what to do with the deserialized response. We actually
        // just want to read a payload that's meant for us. This should be a
        // ReconstructedMessage, and should contain MessageType::Data along
        // with any number of bytes. Anything else will be ignored and nothing
        // will be reported as read. Here we also don't care about the sender
        // tag since we're a client. We do care about it in the server-side
        // since that's how we know where to send stuff.
        let payload_data = match response {
            ServerResponse::Received(m) => m.message,
            ServerResponse::Error(e) => {
                return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e.to_string())))
            }
            _ => return Poll::Pending,
        };

        // Make sure we have at least 9 bytes to read (type + conn_id)
        if payload_data.len() < 9 {
            return Poll::Pending;
        }

        // Ignore if not MessageType::Data
        let msg_type = match MessageType::try_from(payload_data[0]) {
            Ok(MessageType::Data) => MessageType::Data,
            Ok(MessageType::Close) => MessageType::Close,
            _ => return Poll::Pending,
        };

        // Check if this is actually data for us
        if u64::from_be_bytes(payload_data[1..9].try_into().unwrap()) != self.conn_id {
            return Poll::Pending;
        }

        // The endpoint told us that the connection is closing. Propagate it.
        if msg_type == MessageType::Close {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection closed by server",
            )));
        }

        // Finally read it into the buffer
        let data = &payload_data[9..];
        let length = std::cmp::min(buf.len(), data.len());
        buf[..length].copy_from_slice(&data[..length]);
        Poll::Ready(Ok(length))
    }
}

impl AsyncWrite for NymClient {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Construct the `Data` message
        let message = Message::Binary(Self::msg_data(self.endpoint, self.conn_id, buf).serialize());

        // Just fucking send it
        match self.as_mut().stream.send(message).poll_unpin(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(ws_to_io_error(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).flush().poll_unpin(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(ws_to_io_error(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).close().poll_unpin(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(ws_to_io_error(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}
