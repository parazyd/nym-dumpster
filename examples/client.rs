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

use async_tungstenite::async_std::connect_async;
use async_tungstenite::tungstenite::protocol::Message;
use futures::{AsyncReadExt, AsyncWriteExt, SinkExt, StreamExt};
use log::info;
use nym_sphinx::addressing::clients::Recipient;
use nym_websocket::{requests::ClientRequest, responses::ServerResponse};

use nym_dumpster::NymClient;

async fn get_self_addr() -> Recipient {
    let (mut stream, _) = connect_async("ws://127.0.0.1:1977").await.unwrap();

    let addr_message = Message::Binary(ClientRequest::SelfAddress.serialize());
    stream.send(addr_message).await.unwrap();
    let response = match stream.next().await.unwrap().unwrap() {
        Message::Binary(payload) => ServerResponse::deserialize(&payload).unwrap(),
        _ => unreachable!(),
    };
    let listen_addr = match response {
        ServerResponse::SelfAddress(recipient) => *recipient,
        _ => unreachable!(),
    };

    stream.close(None).await.unwrap();

    listen_addr
}

#[async_attributes::main]
async fn main() {
    env_logger::init();

    let rcpt = get_self_addr().await;

    let mut client = NymClient::connect(rcpt, None).await.unwrap();

    let buf = vec![0xde, 0xad, 0xbe, 0xef];
    info!("Client #0 WRITE: {:?}", buf);
    client.write_all(&buf).await.unwrap();

    let mut rep = vec![0u8; 4];
    client.read_exact(&mut rep).await.unwrap();
    info!("Client #0 READ: {:?}", rep);
    assert_eq!(buf, rep);

    client.shutdown().await.unwrap();
}
