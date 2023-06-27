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

use async_std::{channel, task};
use futures::{AsyncReadExt, AsyncWriteExt};

use nym_dumpster::NymServer;

#[async_std::main]
async fn main() {
    let (stop_tx, stop_rx) = channel::bounded::<()>(1);

    let (mut stream, addr) = NymServer::accept(None).await.unwrap();

    task::spawn(async move {
        loop {
            let mut buf = vec![0_u8; 1024];

            let n = match stream.read(&mut buf).await {
                Ok(n) if n == 0 => {
                    println!("Listener #0 closed connection for ID {}", stream.conn_id());
                    break;
                }

                Ok(n) => n,

                Err(e) => {
                    println!("Listener #0 failed reading: {}", e);
                    break;
                }
            };

            println!("Listener #0 READ: {:?}", &buf[..n]);
            stream.write_all(&buf[..n]).await.unwrap();
        }

        stream.close().await.unwrap();
        stop_tx.send(()).await.unwrap();
    });

    println!("Listener #0 listening at {}", addr);

    stop_rx.recv().await.unwrap();
}
