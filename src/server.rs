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
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use async_std::net::TcpStream;
use async_tungstenite::async_std::connect_async;
use async_tungstenite::{tungstenite::protocol::Message, WebSocketStream};
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt, FutureExt, SinkExt, StreamExt};
use log::debug;
use nym_sphinx::addressing::clients::Recipient;
use nym_sphinx::anonymous_replies::requests::AnonymousSenderTag;
use nym_websocket::{requests::ClientRequest, responses::ServerResponse};

use super::{io_error, ws_to_io_error, MessageType};

/// A Nym listener that is able to accept incoming connections
/// and set up anonymous async streams implementing [`AsyncRead`]
/// and [`AsyncWrite`]
#[derive(Debug)]
pub struct NymServer {
    /// The address of the nym-client where websocket clients connect
    ws_host: String,
}

impl NymServer {
    /// Bind server for Nym connections. Takes optional `ws_host` string
    /// pointing to where nym-client is listening. If `None`, uses the
    /// default address. Returns [`NymServer`] which implements `accept()`
    /// for actually accepting incoming connections.
    ///
    /// ```no_run
    /// let listener = NymServer::bind(Some("ws://127.0.0.1:1977")).await?;
    /// ```
    pub async fn bind(ws_host: Option<&str>) -> io::Result<Self> {
        // Test if nym client is alive
        let ws_host = ws_host.unwrap_or("ws://127.0.0.1:1977");
        debug!("Connecting to nym-client @ {}", ws_host);
        let (mut stream, _) = match connect_async(ws_host).await {
            Ok(s) => s,
            Err(e) => return Err(ws_to_io_error(e)),
        };

        if let Err(e) = stream.close(None).await {
            return Err(ws_to_io_error(e));
        }

        Ok(Self {
            ws_host: ws_host.to_string(),
        })
    }

    /// Accepts a new incoming connection to this listener.
    /// When a connection is established, the corresponding stream and
    /// address will be returned.
    ///
    /// ```no_run
    /// let listener = NymServer::bind(None).await?;
    /// let (stream, addr) = listener.accept().await?;
    /// ```
    pub async fn accept(&self) -> io::Result<(NymStream, Recipient)> {
        debug!("Connecting to nym-client @ {}", &self.ws_host);
        let (mut stream, _) = match connect_async(&self.ws_host).await {
            Ok(s) => s,
            Err(e) => return Err(ws_to_io_error(e)),
        };

        // Fetch our address
        let addr_message = Message::Binary(ClientRequest::SelfAddress.serialize());
        if let Err(e) = stream.send(addr_message).await {
            return Err(ws_to_io_error(e));
        }

        // Send the SelfAddress request to the websocket endpoint
        let response = match stream.next().await.unwrap().unwrap() {
            Message::Binary(payload) => ServerResponse::deserialize(&payload).unwrap(),
            _ => return Err(io_error("Failed to retrieve listener addr")),
        };

        // Parse response
        let listen_addr = match response {
            ServerResponse::SelfAddress(recipient) => *recipient,
            _ => return Err(io_error("Failed to retrieve listener addr")),
        };

        // Now wait for a MessageType::Open
        loop {
            // Get message from websocket
            let message = stream.next().await;

            let payload = match message {
                Some(Ok(Message::Binary(data))) => data,
                Some(Ok(_)) => continue, // ignore non-binary messages
                Some(Err(e)) => return Err(ws_to_io_error(e)),
                None => return Err(io::ErrorKind::UnexpectedEof.into()),
            };

            // Parse response
            let response = match ServerResponse::deserialize(&payload) {
                Ok(resp) => resp,
                Err(e) => return Err(io_error(&e.to_string())),
            };

            let (payload_data, sender_tag) = match response {
                ServerResponse::Received(m) => (m.message, m.sender_tag),
                ServerResponse::Error(e) => return Err(io_error(&e.to_string())),
                _ => continue,
            };

            // We want a sender tag
            let Some(sender_tag) = sender_tag else {
                continue;
            };

            // Make sure we have at least 9 bytes to read (type + conn_id)
            if payload_data.len() < 9 {
                continue;
            }

            // Retrieve the connection ID
            let conn_id = u64::from_be_bytes(payload_data[1..9].try_into().unwrap());

            // We're only interested in MessageType::Open
            match MessageType::try_from(payload_data[0]) {
                Ok(MessageType::Open) => {
                    debug!(
                        "Got MessageType::Open from {} with conn_id {}",
                        sender_tag, conn_id
                    );
                }

                _ => continue,
            };

            // Someone wants to open a connection. Let's fork a new websocket
            // connection and return the NymStream.
            let (stream, _) = match connect_async(&self.ws_host).await {
                Ok(s) => s,
                Err(e) => return Err(ws_to_io_error(e)),
            };

            return Ok((
                NymStream {
                    stream,
                    sender_tag,
                    conn_id,
                    conn_open: AtomicBool::new(true),
                },
                listen_addr,
            ));
        }
    }
}

#[derive(Debug)]
pub struct NymStream {
    /// The underlying websocket stream connecting to nym-client
    stream: WebSocketStream<TcpStream>,
    /// Nym sender tag of the party who is communicating with us.
    sender_tag: AnonymousSenderTag,
    /// The connection ID representing this stream.
    conn_id: u64,
    /// Connection open
    conn_open: AtomicBool,
}

impl NymStream {
    /// Shut down the stream. Will attempt to send a `Close` message to the
    /// other party.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        if !self.conn_open.load(Ordering::SeqCst) {
            return Ok(());
        }

        *self.conn_open.get_mut() = false;

        let payload = Message::Binary(self.msg_close().serialize());
        if let Err(e) = self.stream.send(payload).await {
            return Err(ws_to_io_error(e));
        }

        self.close().await
    }

    /// Return the connection ID
    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    /// Construct a `Close` message. Used when closing the connection.
    fn msg_close(&self) -> ClientRequest {
        let mut message = Vec::with_capacity(9);
        message.push(MessageType::Close as u8);
        message.extend_from_slice(&self.conn_id.to_be_bytes());

        ClientRequest::Reply {
            sender_tag: self.sender_tag,
            message,
            connection_id: Some(self.conn_id),
        }
    }

    /// Construct a `Data` message. Used when sending data into the connection.
    fn msg_data(&self, data: &[u8]) -> ClientRequest {
        let mut message = Vec::with_capacity(9 + data.len());
        message.push(MessageType::Data as u8);
        message.extend_from_slice(&self.conn_id.to_be_bytes());
        message.extend_from_slice(data);

        ClientRequest::Reply {
            sender_tag: self.sender_tag,
            message,
            connection_id: Some(self.conn_id),
        }
    }
}

impl Unpin for NymStream {}

impl AsyncRead for NymStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if !self.conn_open.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionRefused.into()));
        }

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
            Err(e) => return Poll::Ready(Err(io_error(&e.to_string()))),
        };

        // We need to check what's contained in the deserialized response.
        // We're only interested in `Data` and `Close`, and that the conn_id
        // actually matches.
        let (payload_data, sender_tag) = match response {
            ServerResponse::Received(m) => (m.message, m.sender_tag),
            ServerResponse::Error(e) => return Poll::Ready(Err(io_error(&e.to_string()))),
            _ => return Poll::Pending,
        };

        // We want a sender tag
        let Some(sender_tag) = sender_tag else {
            return Poll::Pending;
        };

        // Make sure we have at least 9 bytes to read (type + conn_id)
        if payload_data.len() < 9 {
            return Poll::Pending;
        }

        // Compare the connection ID
        let conn_id = u64::from_be_bytes(payload_data[1..9].try_into().unwrap());
        if conn_id != self.conn_id {
            return Poll::Pending;
        }

        match MessageType::try_from(payload_data[0]) {
            Ok(MessageType::Close) => {
                // The client wants to close the connection
                debug!(
                    "Closed connection from {} with conn_id {}",
                    sender_tag, conn_id
                );
                *self.conn_open.get_mut() = false;
                return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
            }

            Ok(MessageType::Data) => {
                // The client sent us some data
                // I don't know if this sender_tag dance is either
                // dangerous or necessary.
                debug!("OLD SENDER_TAG: {:?}", self.sender_tag);
                debug!("NEW SENDER_TAG: {:?}", sender_tag);

                // Update the sender tag
                self.sender_tag = sender_tag;
            }

            Ok(MessageType::Open) => {
                // We got another Open for the same conn_id, so we'll close this
                // connection.
                debug!(
                    "Closed connection from {} with conn_id {}",
                    sender_tag, conn_id
                );
                *self.conn_open.get_mut() = false;
                return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
            }

            Err(_) => return Poll::Pending,
        }

        // Now we actually read the data
        let data = &payload_data[9..];
        let length = std::cmp::min(buf.len(), data.len());
        buf[..length].copy_from_slice(&data[..length]);
        Poll::Ready(Ok(length))
    }
}

impl AsyncWrite for NymStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.conn_open.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionRefused.into()));
        }

        // Construct the `Data` message
        let message = Message::Binary(self.msg_data(buf).serialize());

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
