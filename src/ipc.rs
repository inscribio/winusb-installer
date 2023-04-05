//! Client/server interprocess communication using Windows named pipes

use std::io;
use std::marker::PhantomData;
use std::time::Duration;

use futures::future::BoxFuture;
use windows::Win32::Foundation::ERROR_PIPE_BUSY;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::windows::named_pipe::{NamedPipeServer, NamedPipeClient, ServerOptions, ClientOptions};
use tokio_util::codec;
use tokio_serde::formats::Bincode;

/// Server that must wait for client connection to be used
pub struct Server<Source, Sink> {
    inner: NamedPipeServer,
    _source: PhantomData<Source>,
    _sink: PhantomData<Sink>,
}

impl<Source, Sink> Server<Source, Sink> {
    pub async fn connect(self) -> io::Result<Channel<NamedPipeServer, Source, Sink>> {
        self.inner.connect().await?;
        Ok(channel(self.inner))
    }
}

/// Result of an attempt to connect client to a server
pub type ClientConnectFuture<'a, ServerMsg, ClientMsg> = BoxFuture<'a, io::Result<Channel<NamedPipeClient, ServerMsg, ClientMsg>>>;

/// Protocol between server and client
pub trait Protocol {
    /// Messages sent by the server
    type ServerMsg;

    /// Messages sent by the client
    type ClientMsg;

    /// Create a server on given Windows pipe
    fn server(pipe_name: &str) -> io::Result<Server<Self::ClientMsg, Self::ServerMsg>> {
        server_create(pipe_name)
            .map(|s| Server { inner: s, _source: PhantomData, _sink: PhantomData })
    }

    /// Try connecting to a server on given Windows pipe with a timeout
    fn client(pipe_name: &str, timeout: Duration) -> ClientConnectFuture<'_, Self::ServerMsg, Self::ClientMsg> {
        Box::pin(async move {
            client_connect::<Self::ServerMsg, Self::ClientMsg>(pipe_name, timeout).await
        })
    }
}

/// Helper trait that provides type aliases for the return types in [`Protocol`]
pub trait ProtocolTypes {
    type Server;
    type ServerChannel;
    type ClientChannel;
}

impl<T: Protocol> ProtocolTypes for T {
    type Server = Server<T::ClientMsg, T::ServerMsg>;
    type ServerChannel = Channel<NamedPipeServer, T::ClientMsg, T::ServerMsg>;
    type ClientChannel = Channel<NamedPipeClient, T::ServerMsg, T::ClientMsg>;
}


// Combines length delimiting and serde
type Channel<IO, Source, Sink> =
    Serde<LengthDelimited<IO>, Source, Sink>;

// At lowest level framing is done by length delimiting
type LengthDelimited<IO> = codec::Framed<IO, codec::LengthDelimitedCodec>;

// Transforms raw bytes channel into message channel
type Serde<InnerIo, SourceItem, SinkItem> =
    tokio_serde::Framed<InnerIo, SourceItem, SinkItem, MsgCodec<SourceItem, SinkItem>>;

// Messages are encoded using bincode
type MsgCodec<SourceItem, SinkItem> = Bincode<SourceItem, SinkItem>;


fn length_delimited<T: AsyncRead + AsyncWrite>(io: T) -> LengthDelimited<T> {
    codec::Framed::new(io, codec::LengthDelimitedCodec::new())
}

fn channel<IO: AsyncWrite + AsyncRead, Source, Sink>(io: IO) -> Channel<IO, Source, Sink> {
    tokio_serde::Framed::new(length_delimited(io), Bincode::default())
}

pub fn server_create(pipe_name: &str) -> io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(true)
        // .pipe_mode(named_pipe::PipeMode::Message)
        .create(pipe_name)
}

async fn client_connect<Source, Sink>(pipe_name: &str, timeout: Duration) -> io::Result<Channel<NamedPipeClient, Source, Sink>> {
    let poll_period = Duration::from_millis(50);
    tokio::time::timeout(timeout, async {
        loop {
            tokio::time::sleep(poll_period).await;
            match client_open(pipe_name) {
                Ok(client) => break Ok(client),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => (),
                Err(e) => break Err(e),
            };
        }
    }).await?
        .map(channel)
}

fn client_open(pipe_name: &str) -> io::Result<NamedPipeClient> {
    ClientOptions::new()
        // .pipe_mode(named_pipe::PipeMode::Message)
        .open(pipe_name)
}
