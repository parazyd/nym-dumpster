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

/// Nym Client
pub mod client;
pub use client::NymClient;

/// Nym Server
pub mod server;
pub use server::NymServer;

/// Message types that are being sent and recieved. We use this in order
/// to be able to open and close cconnections, and to send binary data.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MessageType {
    Open = 0x00,
    Data = 0x01,
    Close = 0x02,
}

impl TryFrom<u8> for MessageType {
    type Error = io::Error;

    fn try_from(b: u8) -> Result<Self, Self::Error> {
        match b {
            0x00 => Ok(Self::Open),
            0x01 => Ok(Self::Data),
            0x02 => Ok(Self::Close),
            _ => Err(io_error("Invalid MessageType")),
        }
    }
}

/// Internal function to translate tungstenite errors to io errors.
#[inline]
fn ws_to_io_error(e: async_tungstenite::tungstenite::Error) -> io::Error {
    match e {
        async_tungstenite::tungstenite::Error::Io(err) => err,
        err => io_error(&err.to_string()),
    }
}

/// Internal function for cleaner use of `io::ErrorKind::Other`
#[inline]
fn io_error(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}
