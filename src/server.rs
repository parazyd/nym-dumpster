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
use nym_sphinx::anonymous_replies::requests::AnonymousSenderTag;
use nym_websocket::{requests::ClientRequest, responses::ServerResponse};
use rand::{rngs::OsRng, Rng};

use super::{io_error, ws_to_io_error, MessageType};

/// A Nym listener implementing [`AsyncRead`] and [`AsyncWrite`]
pub struct NymServer {
    /// The underlying websocket stream connecting to nym-client
    stream: WebSocketStream<TcpStream>,
    /// Nym sender tag of the party who is communicating with us
    /// If this is `None`, we also assume that no connection is open.
    sender_tag: Option<AnonymousSenderTag>,
    /// The connection ID representing this stream.
    /// Only the messages containing this ID will actually be read
    /// by this instance. Overridden when we get an `Open` request.
    conn_id: u64,
}

impl NymServer {
    /// Start accepting Nym connections. Takes optional `ws_host` string pointing
    /// to where nym-client is listening. If `None`, uses the default address.
    /// Returns `NymServer`, which acts as a stream and implements the async IO
    /// methods [`AsyncRead`] and [`AsyncWrite`].
    pub async fn accept(ws_host: Option<&str>) -> io::Result<(Self, Recipient)> {
        // Connect to nym-client with websocket
        let (mut stream, _) = match connect_async(ws_host.unwrap_or("ws://127.0.0.1:1977")).await {
            Ok(s) => s,
            Err(e) => return Err(ws_to_io_error(e)),
        };

        // Let's find our address so we can return it.
        let addr_message = Message::Binary(ClientRequest::SelfAddress.serialize());
        if let Err(e) = stream.send(addr_message).await {
            return Err(ws_to_io_error(e));
        }

        // Send the SelfAddress request to the websocket endpoint.
        let response = match stream.next().await.unwrap().unwrap() {
            Message::Binary(payload) => ServerResponse::deserialize(&payload).unwrap(),
            _ => return Err(io_error("Failed to retrieve listener addr")),
        };

        // Parse response
        let listen_addr = match response {
            ServerResponse::SelfAddress(recipient) => *recipient,
            _ => return Err(io_error("Failed to retrieve listener addr")),
        };

        // Generate a random connection ID. This will get overridden when we
        // get an `Open`. It's not set to 0 because of potential DoS attacks.
        let conn_id = OsRng::gen::<u64>(&mut OsRng);

        Ok((
            Self {
                stream,
                sender_tag: None,
                conn_id,
            },
            listen_addr,
        ))
    }

    /// Attempt to cleanly close the active connection
    pub async fn close(&mut self) -> io::Result<()> {
        if self.sender_tag.is_some() {
            // We assume we have an open connection, so we'll tell them we're shutting down.
            let payload = Message::Binary(
                Self::msg_close(self.sender_tag.unwrap(), self.conn_id).serialize(),
            );

            if let Err(e) = self.stream.send(payload).await {
                return Err(ws_to_io_error(e));
            }
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

    /// Construct a `Close` message. Used when closing the connection.
    fn msg_close(sender_tag: AnonymousSenderTag, conn_id: u64) -> ClientRequest {
        let mut message = Vec::with_capacity(9);
        message.push(MessageType::Close as u8);
        message.extend_from_slice(&conn_id.to_be_bytes());

        ClientRequest::Reply {
            sender_tag,
            message,
            connection_id: Some(conn_id),
        }
    }

    /// Construct a `Data` message. Used when sending data into the connection.
    fn msg_data(sender_tag: AnonymousSenderTag, conn_id: u64, data: &[u8]) -> ClientRequest {
        let mut message = Vec::with_capacity(9 + data.len());
        message.push(MessageType::Data as u8);
        message.extend_from_slice(&conn_id.to_be_bytes());
        message.extend_from_slice(data);

        ClientRequest::Reply {
            sender_tag,
            message,
            connection_id: Some(conn_id),
        }
    }
}

impl AsyncRead for NymServer {
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
            Err(e) => return Poll::Ready(Err(io_error(&e.to_string()))),
        };

        // We need to check what's contained in the deserialized response.
        // There's two cases we have to handle here:
        //
        // 1. If our `sender_tag` is None, this means we don't consider this
        //    connection instantiated. In this case, we only care about the
        //    `Open` message.
        //
        // 2. If our `sender_tag` is _not_ None, this means we already have
        //    a "connection". In this case, we care about `Data` or `Close`.
        //    In case we get another `Open` for the same `conn_id`, we will
        //    return a `ConnectionReset` error. This is a bit of an attack
        //    vector since the `conn_id` can be bruteforced, but the entire
        //    `conn_id` should be more difficult to guess. Need to strike a
        //    good balance between bandwidth and security.

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

        // Retrieve the connection ID
        let conn_id = u64::from_be_bytes(payload_data[1..9].try_into().unwrap());

        match MessageType::try_from(payload_data[0]) {
            // Connection Open request
            Ok(MessageType::Open) => {
                if self.sender_tag.is_none() {
                    // Someone opened a connection with us
                    // Now we consider this connection opened
                    // This is the endpoint of [`NymClient::connect`].
                    self.sender_tag = Some(sender_tag);
                    self.conn_id = conn_id;
                    return Poll::Pending;
                }

                if self.sender_tag.is_some() && self.conn_id == conn_id {
                    // This is where we return the ConnectionReset
                    // because we have a sender tag, so we shouldn't
                    // be getting an `Open` again.
                    return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
                }

                if self.sender_tag.is_some() && self.conn_id != conn_id {
                    // This is another client because conn_id doesn't match.
                    return Poll::Pending;
                }
            }
            // Connection Close request
            Ok(MessageType::Close) => {
                if self.sender_tag.is_none() {
                    // Not sure what to do here, maybe also close?
                    // We don't have an open connection so we're just
                    // ignoring it for now.
                    return Poll::Pending;
                }

                if self.sender_tag.is_some() && self.conn_id == conn_id {
                    // The client wants to close the connection
                    return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
                }

                if self.sender_tag.is_some() && self.conn_id != conn_id {
                    // This is another client because conn_id doesn't match.
                    return Poll::Pending;
                }
            }
            // Connection Data
            Ok(MessageType::Data) => {
                if self.sender_tag.is_none() {
                    // We don't know the sender tag, so we ignore this.
                    return Poll::Pending;
                }

                if self.sender_tag.is_some() && self.conn_id == conn_id {
                    // We got some data. Update the `sender_tag`.
                    // This is the only place where this match statement
                    // should pass and the data should be read.
                    // TODO: I don't know if this sender_tag dance is either
                    //       dangerous or necessary.
                    println!("OLD SENDER TAG: {:?}", self.sender_tag);
                    println!("NEW SENDER TAG: {:?}", Some(sender_tag));
                    self.sender_tag = Some(sender_tag);
                }

                if self.sender_tag.is_some() && self.conn_id != conn_id {
                    // This is another client because conn_id doesn't match.
                    return Poll::Pending;
                }
            }

            Err(_) => return Poll::Pending,
        }

        // The above `match` should only pass if the connection is open
        // (we have a `sender_tag`), we got a `Data` message, and the
        // connection ID matches.
        let data = &payload_data[9..];
        let length = std::cmp::min(buf.len(), data.len());
        buf[..length].copy_from_slice(&data[..length]);
        Poll::Ready(Ok(length))
    }
}

impl AsyncWrite for NymServer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Here we have to check if we actually consider our connection "open".
        // We do this by checking if we have a `sender_tag`.
        if self.sender_tag.is_none() {
            // TODO: Not sure if ConnectionRefused is the correct error here.
            return Poll::Ready(Err(io::ErrorKind::ConnectionRefused.into()));
        }

        // Construct the `Data` message
        let message = Message::Binary(
            Self::msg_data(self.sender_tag.unwrap(), self.conn_id, buf).serialize(),
        );

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

#[cfg(test)]
mod tests {
    use super::*;

    #[async_std::test]
    async fn server_bind() {
        let (mut server, addr) = NymServer::bind().await.unwrap();
        println!("{}", addr);
        server.close().await.unwrap();
    }
}
