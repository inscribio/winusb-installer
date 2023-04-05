//! WinUSB driver installer using libwdi
//!
//! Implements WinUSB driver installation from non-privileged process by spawning a separate
//! process with elevated permissions and communicating via IPC to perform the installation
//! process.
//!
//! The [`Server`] is started in the parent (non-privileged) process. It then uses Windows "runas"
//! command to spawn the client executable (by default the same executable). Client executable's
//! job is to create and run [`Client`]. It is assumed that client/server are identified by the
//! number of process arguments - server has no arguments and client receives a single argument
//! which indicates the name of Windows pipe used for IPC.

use std::{io, env};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::prelude::*;
use serde::{Serialize, Deserialize};

pub mod ipc;
pub mod runas;
pub mod winusb;

use ipc::{Protocol, ProtocolTypes};
use tokio::sync::{oneshot, mpsc};

pub use winusb::{Device, InstallConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ServerMsg {
    /// Request driver installation
    Install(InstallConfig, Vec<Device>),
    /// Configure logging
    Logging { window: winusb::Window },
    /// Request client process to exit
    Exit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ClientMsg {
    /// Result of installing drivers for single device
    DeviceInstall(Device, Result<(), String>),
    /// Other error
    Error(String),
    /// Installation request handling started
    InstallStarted,
    /// Installation request handling done
    InstallDone,
    /// Sent during installation to indicate that client is alive
    Heatbeat,
}

struct Installation;

impl ipc::Protocol for Installation {
    type ServerMsg = ServerMsg;
    type ClientMsg = ClientMsg;
}

fn pipe_name(pipe_id: &str) -> String {
    assert!(!pipe_id.starts_with(r"\\."));
    String::from(r"\\.\pipe\") + pipe_id
}

pub enum Mode {
    Server(Server),
    Client(Client),
}

/// Initialize the installer module
///
/// Depending on program env::args this will resolve either to a server or a client.
/// Server is the one that spawns the client (with elevated privilege) and initiates
/// all operations.
pub fn init() -> Mode {
    match env::args().nth(1) {
        Some(pipe_name) => Mode::Client(Client::new(pipe_name)),
        None => Mode::Server(Server::new()),
    }
}

#[derive(Default)]
pub struct Server {
    pipe_id: Option<String>,
    client_executable: Option<PathBuf>,
    show_child_window: bool,
    child: Option<runas::Child>,
}

pub struct Client {
    pipe_name: String,
    connection_timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Progress {
    /// Installation process started (client communication established)
    Started,
    /// Installation for given device done
    Device(Device, Result<(), String>),
}

impl Server {
    pub const DEFAULT_PIPE_ID: &str = "winusb-driver-installer";

    pub fn new() -> Self {
        Self {
            pipe_id: None,
            client_executable: None,
            child: None,
            show_child_window: false,
        }
    }

    /// Set pipe id other than [`Server::DEFAULT_PIPE_ID`]
    pub fn pipe_id(&mut self, pipe_id: &str) -> &mut Self {
        self.pipe_id = Some(pipe_id.to_string());
        self
    }

    fn get_pipe_name(&self) -> String {
        pipe_name(
            self.pipe_id.as_deref()
                .unwrap_or(Self::DEFAULT_PIPE_ID)
        )
    }

    /// Make the spawned client window visible during installation, defaults to `false`
    pub fn show_child_window(&mut self, show: bool) -> &mut Self {
        self.show_child_window = show;
        self
    }

    /// Set path to client executable. By default [`std::env::current_exe`] is used.
    pub fn client_executable(&mut self, executable: impl AsRef<OsStr>) -> &mut Self {
        self.client_executable = Some(executable.as_ref().into());
        self
    }

    /// List all visible devices.
    pub fn visible_devices(&self) -> io::Result<Vec<Device>> {
        winusb::Devices::new(Box::new(|_| true))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
            .map(|devices| devices.candidates().collect())
    }

    fn spawn_client(&mut self) -> io::Result<runas::Child> {
        if let Some(mut child) = self.child.take() {
            log::debug!("Killing child process");
            child.kill()?;
        }
        let exe = if let Some(exe) = self.client_executable.clone() {
            exe
        } else {
            env::current_exe()?
        };
        runas::Command::new(exe)
            .arg(self.get_pipe_name())
            .hide(!self.show_child_window)
            .spawn()
    }

    async fn wait_for_start(io: &mut <Installation as ipc::ProtocolTypes>::ServerChannel) -> io::Result<()> {
        loop {
            while let Some(msg) = io.next().await.transpose()? {
                match msg {
                    ClientMsg::Heatbeat => {},
                    ClientMsg::Error(err) => log::error!("Client error: {}", err),
                    ClientMsg::InstallStarted => return Ok(()),
                    other => return Err(io::Error::new(io::ErrorKind::Other,
                        format!("Unexpected message: {:?}", other))),
                }
            }
        }
    }

    async fn wait_installation(
        io: &mut <Installation as ipc::ProtocolTypes>::ServerChannel,
        heartbeat_timeout: Duration,
        mut on_progress: impl FnMut(Progress)
    ) -> io::Result<usize> {
        let mut last_heatbeat = Instant::now();
        let mut installed = 0;
        loop {
            // Check heartbeat timeout
            if last_heatbeat.elapsed() > heartbeat_timeout {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "No heatbeat from client"));
            }
            let result = match tokio::time::timeout(Duration::from_millis(100), io.next()).await {
                Ok(result) => result,
                Err(_) => continue, //
            };

            if let Some(msg) = result.transpose()? {
                log::trace!("Received {:?}", msg);
                match msg {
                    ClientMsg::Heatbeat | ClientMsg::InstallStarted => last_heatbeat = Instant::now(),
                    ClientMsg::InstallDone => break,
                    ClientMsg::Error(err) => log::error!("Client error: {:?}", err),
                    ClientMsg::DeviceInstall(dev, result) => {
                        log::info!("Installation of {:04x}:{:04x}: {:?}", dev.vid, dev.pid, result);
                        if result.is_ok() {
                            installed += 1;
                        }
                        on_progress(Progress::Device(dev, result));
                    },
                }
            }
        }

        Ok(installed)
    }

    /// Perform installation for given list of devices
    ///
    /// Devices should be obtained using [`Self::visible_devices`] and will be used to filter
    /// the devices for installation. Note that some devices may disappear between the moment
    /// server used [`Self::visible_devices`] to find them and the moment client starts
    /// installation.
    pub async fn install(
        &mut self,
        config: InstallConfig,
        devices: &[Device],
        mut on_progress: impl FnMut(Progress),
    ) -> io::Result<()> {
        if devices.len() == 0 {
            log::warn!("No candidate devices found");
            return Ok(());
        }
        log::info!("Preparing for driver installation for {} devices.", devices.len());

        let pipe_name = self.get_pipe_name();
        let server = Installation::server(&pipe_name)?;

        log::info!("Server running, spawning child.");
        self.child = Some(self.spawn_client()?);

        log::info!("Waiting for client to connect");
        let mut server = server.connect().await?;

        // Rely on the fact that if tx is dropped then rx receives RecvError
        let (log_end_tx, mut log_end_rx) = oneshot::channel();
        if let Ok(logger) = winusb::LogReceiver::new() {
            server.send(ServerMsg::Logging { window: logger.window() }).await?;

            // FIXME: for some reason it doesn't work and we have rx permission error
            tokio::spawn(async move {
                let mut logger = logger;
                sleep_ms(400).await;
                loop {
                    sleep_ms(100).await;
                    // Check if the task should end
                    match log_end_rx.try_recv() {
                        Ok(_) => return,
                        Err(oneshot::error::TryRecvError::Closed) => return,
                        Err(oneshot::error::TryRecvError::Empty) => {},
                    };
                    match logger.get() {
                        Ok(Some(msg)) => log::info!("Received log: {}", msg),
                        Ok(None) => {},
                        Err(err) => {
                            log::error!("Log rx error: {}", err);
                            return;
                        }
                    }
                }
            });
        } else {
            log::warn!("Could not initialize logging, current process may not have any windows open");
        };

        log::info!("Starting installation");
        server.send(ServerMsg::Install(config, devices.to_vec())).await?;

        // Wait until client starts installation
        tokio::time::timeout(Duration::from_secs(30), Self::wait_for_start(&mut server)).await??;
        on_progress(Progress::Started);

        // libwdi should exit after 5 minutes
        let install_timeout = Duration::from_secs(6 * 60);
        // client should send heartbeat each second
        let heartbeat_timeout = Duration::from_secs(5);

        let install = Self::wait_installation(&mut server, heartbeat_timeout, on_progress);
        let installed = match tokio::time::timeout(install_timeout, install).await {
            Ok(installed) => installed?,
            Err(e) => {
                log::error!("Installation timed out");
                server.send(ServerMsg::Exit).await.ok();
                return Err(e.into());
            },
        };

        if server.send(ServerMsg::Exit).await.is_err() {
            log::warn!("Could not send Exit to client");
        }

        if installed == devices.len() {
            log::info!("Installed drivers for {}/{} devices.", installed, devices.len());
        } else {
            log::warn!("Installed drivers for {}/{} devices.", installed, devices.len());
        }

        // Just to satisfy compiler needing message type. We generally rely on Drop.
        log_end_tx.send(()).ok();

        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            log::debug!("Killing child process");
            child.kill().ok();
        }
    }
}

impl Client {
    pub fn new(pipe_name: String) -> Self {
        Self {
            pipe_name,
            connection_timeout: Duration::from_secs(10),
        }
    }

    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    pub fn connection_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.connection_timeout = timeout;
        self
    }

    fn install_sync(io: mpsc::UnboundedSender<ClientMsg>, config: InstallConfig, devices: Vec<Device>) {
        let match_device = move |device: &Device| {
            devices.iter().any(|dev| dev == device)
        };
        match winusb::Devices::new(Box::new(match_device)) {
            Err(err) => {
                log::error!("Could not create device list: {:?}", err);
                io.send(ClientMsg::Error(err.to_string())).unwrap();
            }
            Ok(devices) => {
                log::info!("Found {} installation candidates", devices.candidates().count());

                for (dev, result) in devices.install_iter(&config) {
                    log::info!("Installation for device {:04x}:{:04x}: {:?}", dev.vid, dev.pid, result);
                    let result = result.map_err(|err| err.to_string());
                    io.send(ClientMsg::DeviceInstall(dev, result)).unwrap();
                }
            },
        };
    }

    async fn install(
        &mut self,
        io: &mut <Installation as ProtocolTypes>::ClientChannel,
        config: InstallConfig,
        devices: Vec<Device>,
    ) -> io::Result<()> {
        // Create a separate thread for installation because it uses blocking calls to libwdi
        // This thread will send messages to current task which will send these and heartbeats to server.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let installer = tokio::task::spawn_blocking(move || {
            log::trace!("Started blocking installation thread");
            Self::install_sync(tx, config, devices);
        });

        log::trace!("Started heatbeat");
        loop {
            io.send(ClientMsg::Heatbeat).await.unwrap();
            match tokio::time::timeout(Duration::from_millis(1000), rx.recv()).await {
                Ok(Some(msg)) => io.send(msg).await?,
                Ok(None) => break, // Channel closed which means that thread finished
                Err(_) => {}, // loop timed out, just send next heartbeat
            }
        }

        installer.await?;

        Ok(())
    }

    /// Serve the installation (this is client in the sense of IPC, but a server in terms of
    /// installation process).
    pub async fn serve(&mut self) -> io::Result<()> {
        let mut client = Installation::client(&self.pipe_name, self.connection_timeout).await?;

        loop {
            if let Some(msg) = client.try_next().await? {
                log::trace!("Received {:?}", msg);

                match msg {
                    ServerMsg::Exit => break,
                    ServerMsg::Logging { window } => winusb::LogReceiver::client_setup(window)?,
                    ServerMsg::Install(config, devices) => {
                        log::debug!("Got driver installation request");
                        client.send(ClientMsg::InstallStarted).await?;
                        self.install(&mut client, config, devices).await?;
                        client.send(ClientMsg::InstallDone).await?;
                    },
                }
            }
        }

        Ok(())
    }
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}
