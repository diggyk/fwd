use bytes::Bytes;
use message::{Message, MessageReader, MessageWriter};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::process;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

mod connection;
mod message;
mod refresh;

#[derive(Debug)]
pub enum Error {
    Protocol,
    ProtocolVersion,
    IO(tokio::io::Error),
    MessageIncomplete,
    MessageUnknown,
    MessageCorrupt,
    ConnectionReset,
    ProcFs(String),
    NotSupported,
}

impl PartialEq for Error {
    fn eq(&self, other: &Error) -> bool {
        use Error::*;
        match self {
            Protocol => match other {
                Protocol => true,
                _ => false,
            },
            ProtocolVersion => match other {
                ProtocolVersion => true,
                _ => false,
            },
            IO(s) => match other {
                IO(o) => s.kind() == o.kind(),
                _ => false,
            },
            MessageIncomplete => match other {
                MessageIncomplete => true,
                _ => false,
            },
            MessageUnknown => match other {
                MessageUnknown => true,
                _ => false,
            },
            MessageCorrupt => match other {
                MessageCorrupt => true,
                _ => false,
            },
            ConnectionReset => match other {
                ConnectionReset => true,
                _ => false,
            },
            ProcFs(a) => match other {
                ProcFs(b) => a == b,
                _ => false,
            },
            NotSupported => match other {
                NotSupported => true,
                _ => false,
            },
        }
    }
}

async fn pump_write<T: AsyncWrite + Unpin>(
    messages: &mut mpsc::Receiver<Message>,
    writer: &mut MessageWriter<T>,
) -> Result<(), Error> {
    while let Some(msg) = messages.recv().await {
        writer.write(msg).await?;
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Server

struct Connection {
    connected: Option<oneshot::Sender<()>>,
    data: mpsc::Sender<Bytes>,
}

struct ConnectionTableState {
    next_id: u64,
    connections: HashMap<u64, Connection>,
}

#[derive(Clone)]
struct ConnectionTable {
    connections: Arc<Mutex<ConnectionTableState>>,
}

impl ConnectionTable {
    fn new() -> ConnectionTable {
        ConnectionTable {
            connections: Arc::new(Mutex::new(ConnectionTableState {
                next_id: 0,
                connections: HashMap::new(),
            })),
        }
    }

    fn alloc(self: &mut Self, connected: oneshot::Sender<()>, data: mpsc::Sender<Bytes>) -> u64 {
        let mut tbl = self.connections.lock().unwrap();
        let id = tbl.next_id;
        tbl.next_id += 1;
        tbl.connections.insert(
            id,
            Connection {
                connected: Some(connected),
                data,
            },
        );
        id
    }

    fn add(self: &mut Self, id: u64, data: mpsc::Sender<Bytes>) {
        let mut tbl = self.connections.lock().unwrap();
        tbl.connections.insert(
            id,
            Connection {
                connected: None,
                data,
            },
        );
    }

    fn connected(self: &mut Self, id: u64) {
        let connected = {
            let mut tbl = self.connections.lock().unwrap();
            if let Some(c) = tbl.connections.get_mut(&id) {
                c.connected.take()
            } else {
                None
            }
        };

        if let Some(connected) = connected {
            _ = connected.send(());
        }
    }

    async fn receive(self: &Self, id: u64, buf: Bytes) {
        let data = {
            let tbl = self.connections.lock().unwrap();
            if let Some(connection) = tbl.connections.get(&id) {
                Some(connection.data.clone())
            } else {
                None
            }
        };

        if let Some(data) = data {
            _ = data.send(buf).await;
        }
    }

    fn remove(self: &mut Self, id: u64) {
        let mut tbl = self.connections.lock().unwrap();
        tbl.connections.remove(&id);
    }
}

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
) -> Result<(), Error> {
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
) -> Result<(), Error> {
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

async fn client_sync<T: AsyncRead + Unpin>(reader: &mut T) -> Result<(), Error> {
    eprintln!("> Waiting for synchronization marker...");
    let mut seen = 0;
    while seen < 8 {
        let byte = match reader.read_u8().await {
            Ok(b) => b,
            Err(e) => return Err(Error::IO(e)),
        };
        seen = if byte == 0 { seen + 1 } else { 0 };
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
) -> Result<(), Error> {
    loop {
        let listener = match TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await {
            Ok(t) => t,
            Err(e) => return Err(Error::IO(e)),
        };
        loop {
            // The second item contains the IP and port of the new
            // connection, but we don't care.
            let (mut socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => return Err(Error::IO(e)),
            };

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
) -> Result<(), Error> {
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
) -> Result<(), Error> {
    // Wait for the server's announcement.
    if let Message::Hello(major, minor, _) = reader.read().await? {
        if major != 0 || minor > 1 {
            return Err(Error::ProtocolVersion);
        }
    } else {
        return Err(Error::Protocol);
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

async fn spawn_ssh(server: &str) -> Result<tokio::process::Child, Error> {
    let mut cmd = process::Command::new("ssh");
    cmd.arg("-T").arg(server).arg("fwd").arg("--server");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::piped());
    match cmd.spawn() {
        Ok(t) => Ok(t),
        Err(e) => Err(Error::IO(e)),
    }
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
