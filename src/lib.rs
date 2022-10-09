use anyhow::{bail, Result};
use connection::ConnectionTable;
use message::{Message, MessageReader, MessageWriter};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::process;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

mod connection;
mod message;
mod refresh;

// ----------------------------------------------------------------------------
// Write Management

/// Gathers writes from an mpsc queue and writes them to the specified
/// writer.
///
/// This is kind of an odd function. It raises a lot of questions.
///
/// *Why can't this just be a wrapper function on top of MessageWriter that
/// everybody calls?* Well, we could do that, but we also need to synchronize
/// writes to the underlying stream.
///
/// *Why not use an async mutex?* Because this function has a nice side
/// benefit: if it ever quits, we're *either* doing an orderly shutdown
/// (because the last write end of this channel closed) *or* the remote
/// connection has closed. [client_main] uses this fact to its advantage to
/// detect when the connection has failed.
///
/// At some point we may even automatically reconnect in response!
///
async fn pump_write<T: AsyncWrite + Unpin>(
    messages: &mut mpsc::Receiver<Message>,
    writer: &mut MessageWriter<T>,
) -> Result<()> {
    while let Some(msg) = messages.recv().await {
        writer.write(msg).await?;
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Server

async fn server_handle_connection(
    channel: u64,
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
) {
    let mut connections = connections;
    if let Ok(mut stream) = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await {
        let (send_data, mut data) = mpsc::channel(32);
        connections.add(channel, send_data);
        if let Ok(_) = writer.send(Message::Connected(channel)).await {
            let mut writer = writer.clone();
            connection::process(channel, &mut stream, &mut data, &mut writer).await;

            eprintln!("< Done server!");
        }
    }
}

async fn server_read<T: AsyncRead + Unpin>(
    reader: &mut MessageReader<T>,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
) -> Result<()> {
    eprintln!("< Processing packets...");
    loop {
        let message = reader.read().await?;

        use Message::*;
        match message {
            Ping => (),
            Connect(channel, port) => {
                let (writer, connections) = (writer.clone(), connections.clone());
                tokio::spawn(async move {
                    server_handle_connection(channel, port, writer, connections).await;
                });
            }
            Close(channel) => {
                let mut connections = connections.clone();
                tokio::spawn(async move {
                    // Once we get a close the connection becomes  unreachable.
                    //
                    // NOTE: If all goes well the 'data' channel gets dropped
                    // here, and we close the write half of the socket.
                    connections.remove(channel);
                });
            }
            Data(channel, buf) => {
                let connections = connections.clone();
                tokio::spawn(async move {
                    connections.receive(channel, buf).await;
                });
            }
            Refresh => {
                let writer = writer.clone();
                tokio::spawn(async move {
                    let ports = match refresh::get_entries() {
                        Ok(ports) => ports,
                        Err(e) => {
                            eprintln!("< Error scanning: {:?}", e);
                            vec![]
                        }
                    };
                    if let Err(e) = writer.send(Message::Ports(ports)).await {
                        // Writer has been closed for some reason, we can just quit.... I hope everything is OK?
                        eprintln!("< Warning: Error sending: {:?}", e);
                    }
                });
            }
            _ => panic!("Unsupported: {:?}", message),
        };
    }
}

async fn server_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    reader: &mut MessageReader<Reader>,
    writer: &mut MessageWriter<Writer>,
) -> Result<()> {
    let connections = ConnectionTable::new();

    // The first message we send must be an announcement.
    writer.write(Message::Hello(0, 1, vec![])).await?;

    // Jump into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);
    let writing = pump_write(&mut msg_receiver, writer);
    let reading = server_read(reader, msg_sender, connections);
    tokio::pin!(reading);
    tokio::pin!(writing);

    let (mut done_writing, mut done_reading) = (false, false);
    loop {
        tokio::select! {
            result = &mut writing, if !done_writing => {
                done_writing = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
            result = &mut reading, if !done_reading => {
                done_reading = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
        }
    }
}

async fn client_sync<T: AsyncRead + Unpin>(reader: &mut T) -> Result<()> {
    // TODO: While we're waiting here we should be echoing everything we read.
    //       We should also be proxying *our* stdin to the processes stdin,
    //       and turn that off when we've synchronized. That way we can
    //       handle passwords and the like for authentication.
    eprintln!("> Waiting for synchronization marker...");
    let mut seen = 0;
    while seen < 8 {
        let byte = reader.read_u8().await?;
        if byte == 0 {
            seen += 1;
        } else {
            tokio::io::stdout().write_u8(byte).await?;
        }
    }
    Ok(())
}

async fn client_handle_connection(
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
    socket: &mut TcpStream,
) {
    let mut connections = connections;
    let (send_connected, connected) = oneshot::channel();
    let (send_data, mut data) = mpsc::channel(32);
    let channel = connections.alloc(send_connected, send_data);

    if let Ok(_) = writer.send(Message::Connect(channel, port)).await {
        if let Ok(_) = connected.await {
            let mut writer = writer.clone();
            connection::process(channel, socket, &mut data, &mut writer).await;

            eprintln!("> Done client!");
        } else {
            eprintln!("> Failed to connect to remote");
        }
    }
}

async fn client_listen(
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
) -> Result<()> {
    loop {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
        loop {
            // The second item contains the IP and port of the new
            // connection, but we don't care.
            let (mut socket, _) = listener.accept().await?;

            let (writer, connections) = (writer.clone(), connections.clone());
            tokio::spawn(async move {
                client_handle_connection(port, writer, connections, &mut socket).await;
            });
        }
    }
}

async fn client_read<T: AsyncRead + Unpin>(
    reader: &mut MessageReader<T>,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
) -> Result<()> {
    let mut listeners: HashMap<u16, oneshot::Sender<()>> = HashMap::new();

    eprintln!("> Processing packets...");
    loop {
        let message = reader.read().await?;

        use Message::*;
        match message {
            Ping => (),
            Connected(channel) => {
                let mut connections = connections.clone();
                tokio::spawn(async move {
                    connections.connected(channel);
                });
            }
            Close(channel) => {
                let mut connections = connections.clone();
                tokio::spawn(async move {
                    connections.remove(channel);
                });
            }
            Data(channel, buf) => {
                let connections = connections.clone();
                tokio::spawn(async move {
                    connections.receive(channel, buf).await;
                });
            }
            Ports(ports) => {
                let mut new_listeners = HashMap::new();

                println!("The following ports are available:");
                for port in ports {
                    println!("  {}: {}", port.port, port.desc);

                    let port = port.port;
                    if let Some(l) = listeners.remove(&port) {
                        if !l.is_closed() {
                            // `l` here is, of course, the channel that we
                            // use to tell the listener task to stop (see the
                            // spawn call below). If it isn't closed then
                            // that means a spawn task is still running so we
                            // should just let it keep running and re-use the
                            // existing listener.
                            new_listeners.insert(port, l);
                        }
                    }

                    if !new_listeners.contains_key(&port) {
                        let (l, stop) = oneshot::channel();
                        new_listeners.insert(port, l);

                        let (writer, connections) = (writer.clone(), connections.clone());
                        tokio::spawn(async move {
                            let result = tokio::select! {
                                r = client_listen(port, writer, connections) => r,
                                _ = stop => Ok(()),
                            };
                            if let Err(e) = result {
                                eprintln!("> Error listening on port {}: {:?}", port, e);
                            }
                        });
                    }
                }

                listeners = new_listeners;
            }
            _ => panic!("Unsupported: {:?}", message),
        };
    }
}

async fn client_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    reader: &mut MessageReader<Reader>,
    writer: &mut MessageWriter<Writer>,
) -> Result<()> {
    // Wait for the server's announcement.
    if let Message::Hello(major, minor, _) = reader.read().await? {
        if major != 0 || minor > 1 {
            bail!("Unsupported remote protocol version {}.{}", major, minor);
        }
    } else {
        bail!("Expected a hello message from the remote server");
    }

    // Kick things off with a listing of the ports...
    eprintln!("> Sending initial list command...");
    writer.write(Message::Refresh).await?;

    let connections = ConnectionTable::new();

    // And now really get into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);
    let writing = pump_write(&mut msg_receiver, writer);
    let reading = client_read(reader, msg_sender, connections);
    tokio::pin!(reading);
    tokio::pin!(writing);

    let (mut done_writing, mut done_reading) = (false, false);
    loop {
        tokio::select! {
            result = &mut writing, if !done_writing => {
                done_writing = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
            result = &mut reading, if !done_reading => {
                done_reading = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
        }
    }
}

/////

pub async fn run_server() {
    let reader = BufReader::new(tokio::io::stdin());
    let mut writer = BufWriter::new(tokio::io::stdout());

    // Write the 8-byte synchronization marker.
    eprintln!("< Writing marker...");
    writer
        .write_u64(0x00_00_00_00_00_00_00_00)
        .await
        .expect("Error writing marker");

    if let Err(e) = writer.flush().await {
        eprintln!("Error writing sync marker: {:?}", e);
        return;
    }
    eprintln!("< Done!");

    let mut writer = MessageWriter::new(writer);
    let mut reader = MessageReader::new(reader);
    if let Err(e) = server_main(&mut reader, &mut writer).await {
        eprintln!("Error: {:?}", e);
    }
}

async fn spawn_ssh(server: &str) -> Result<tokio::process::Child, std::io::Error> {
    let mut cmd = process::Command::new("ssh");
    cmd.arg("-T").arg(server).arg("fwd").arg("--server");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::piped());
    cmd.spawn()
}

pub async fn run_client(remote: &str) {
    // TODO: Drive a reconnect loop
    let mut child = spawn_ssh(remote).await.expect("failed to spawn");

    let mut writer = MessageWriter::new(BufWriter::new(
        child
            .stdin
            .take()
            .expect("child did not have a handle to stdout"),
    ));

    let mut reader = BufReader::new(
        child
            .stdout
            .take()
            .expect("child did not have a handle to stdout"),
    );

    if let Err(e) = client_sync(&mut reader).await {
        eprintln!("Error synchronizing: {:?}", e);
        return;
    }

    let mut reader = MessageReader::new(reader);
    if let Err(e) = client_main(&mut reader, &mut writer).await {
        eprintln!("Error: {:?}", e);
    }
}
