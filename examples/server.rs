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

use async_std::{sync::Arc, task};
use futures::{AsyncReadExt, AsyncWriteExt};
use log::{error, info};

use nym_dumpster::NymServer;

#[async_attributes::main]
async fn main() {
    env_logger::init();

    let listener = Arc::new(NymServer::bind(None).await.unwrap());
    info!("Bound listener");

    loop {
        let (mut stream, addr) = listener.clone().accept().await.unwrap();
        info!("Nym Address: {}", addr);
        info!("Accepted connection for ID {}", stream.conn_id());
        task::spawn(async move {
            loop {
                let mut buf = vec![0u8; 1024];

                let n = match stream.read(&mut buf).await {
                    Ok(n) if n == 0 => {
                        info!("Closed connection for ID {}", stream.conn_id());
                        break;
                    }

                    Ok(n) => n,

                    Err(e) => {
                        error!("Failed reading for ID {}: {}", stream.conn_id(), e);
                        break;
                    }
                };

                info!("Listener READ: {:?}", &buf[..n]);
                stream.write_all(&buf[..n]).await.unwrap();
            }

            stream.shutdown().await.unwrap();
            info!("Closed listener for ID {}", stream.conn_id());
        });
    }
}
