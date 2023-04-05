use std::io;
use std::num::{NonZeroU64, NonZeroU8};

use libwdi as wdi;
use serde::{Serialize, Deserialize};
use windows::Win32::Foundation::{LPARAM, HWND, BOOL};
use windows::Win32::System::Threading;
use windows::Win32::UI::WindowsAndMessaging;

pub type Result<T> = wdi::Result<T>;

pub type DeviceFilter = dyn Fn(&Device) -> bool + Send;

/// List of detected USB devices for driver installation
pub struct Devices {
    list: wdi::DevicesList,
    filter: Box<DeviceFilter>,
}

/// Device information. Owned version of [`libwdi::DeviceInfo`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    pub vid: u16,
    pub pid: u16,
    pub is_composite: bool,
    pub mi: Option<NonZeroU8>,
    pub driver_version: Option<NonZeroU64>,
    pub desc: String,
    pub driver: Option<String>,
    pub device_id: Option<String>,
    pub hardware_id: Option<String>,
    pub compatible_id: Option<String>,
    pub upper_filter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallConfig {
    /// Name that will be visible as the "Manufacturer" device property in device manager
    pub vendor: String,
    /// The directory where the .inf and driver files should be crated, e.g. `C:\usb_driver`
    pub driver_path: String,
    /// The name of the .inf file to generate (includeing the .inf extension)
    pub inf_name: String,
}

impl Devices {
    pub fn new(filter: Box<DeviceFilter>) -> wdi::Result<Self> {
        setup_logs();
        let list = wdi::CreateListOptions::new()
            .list_all(true)
            .create_list()?;
        Ok(Self {
            list,
            filter,
        })
    }


    fn candidates_ref(&self) -> impl Iterator<Item = wdi::DeviceInfo<'_>> {
        self.list.iter()
            .filter(|dev| (self.filter)(&Device::from(dev)))
    }

    pub fn candidates(&self) -> impl Iterator<Item = Device> + '_ {
        self.candidates_ref()
            .map(|dev| Device::from(&dev))
            .inspect(|dev| log::trace!("Candidate device: {:#?}", dev))
    }

    pub fn is_install_needed(&self) -> bool {
        self.candidates_ref().count() > 0
    }

    // pub fn install_all(&self, config: &InstallConfig) -> wdi::Result<()> {
    //     for dev in self.candidates() {
    //         install_winusb(dev, config)?;
    //     }
    //     Ok(())
    // }

    pub fn install_iter<'a>(&'a self, config: &'a InstallConfig) -> impl Iterator<Item = (Device, wdi::Result<()>)> + '_ {
        self.candidates_ref()
            .inspect(|dev| log::debug!("Installing for: {:#?}", Device::from(dev)))
            .map(|dev| (Device::from(&dev), install_winusb(dev, config)))
    }

    // /// Install for all while processing results. Return `false` from `f` to stop immediatelly.
    // pub fn install_for_each(&mut self, mut f: impl FnMut(Device, wdi::Result<()>) -> bool) {
    //     for dev in self.candidates() {
    //         let device = Device::from(&dev);
    //         if !f(device, install_winusb(dev)) {
    //             break;
    //         }
    //     }
    // }
}

pub struct LogReceiver {
    window: HWND,
    buf: Box<[u8; 8192]>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Window(isize);

impl LogReceiver {
    /// Initialize log receiver on the server side
    pub fn new() -> io::Result<Self> {
        let windows = get_current_proc_windows();
        if let Some(window) = windows.get(0).cloned() {
            Ok(Self {
                window,
                buf: Box::new([0; 8192]),
            })
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "No windows are associated with this process"))
        }
    }

    pub fn get(&mut self) -> io::Result<Option<String>> {
        match wdi::read_logger(&mut self.buf[..]) {
            Ok(n) if n == 0 => Ok(None),
            Ok(n) => Ok(Some(String::from_utf8_lossy(&self.buf[..n]).to_string())),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
        }
    }

    pub fn window(&self) -> Window {
        Window(self.window.0)
    }

    /// Setup logging on the installer side
    pub fn client_setup(window: Window) -> io::Result<()> {
        wdi::set_log_level(wdi::LogLevel::Info)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        unsafe {
            wdi::register_logger(window.0 as *mut _, 1, 0)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
        }
    }
}

impl Device {
    /// Convenience method for checking if device has WinUSB driver installed
    pub fn has_winusb(&self) -> bool {
        self.driver.as_ref().map_or(false, |driver| driver.to_lowercase() == "winusb")
    }
}

impl<'a> From<&wdi::DeviceInfo<'a>> for Device {
    fn from(dev: &wdi::DeviceInfo<'a>) -> Self {
        Self {
            vid: dev.vid(),
            pid: dev.pid(),
            is_composite: dev.is_composite(),
            mi: dev.mi(),
            driver_version: dev.driver_version(),
            desc: dev.desc().to_string(),
            driver: dev.driver().map(|s| s.to_string()),
            device_id: dev.device_id().map(|s| s.to_string()),
            hardware_id: dev.hardware_id().map(|s| s.to_string()),
            compatible_id: dev.compatible_id().map(|s| s.to_string()),
            upper_filter: dev.upper_filter().map(|s| s.to_string()),
        }
    }
}

fn install_winusb(dev: wdi::DeviceInfo<'_>, config: &InstallConfig) -> wdi::Result<()> {
    let opts = wdi::PrepareDriverOptions::new()
        .driver_type(wdi::DriverType::WinUsb)
        .vendor_name(&config.vendor).unwrap();

    let driver = opts.prepare_driver(dev, &config.driver_path, &config.inf_name)?;
    driver.install_driver()
}

// fn needs_install(dev: &wdi::DeviceInfo) -> bool {
//     let is_bootloader = (dev.vid(), dev.pid()) == (STM32_BOOTLOADER_VID, STM32_BOOTLOADER_PID);
//     let has_winusb = dev.driver().map_or(false, |driver| driver.to_lowercase() == "winusb");
//     is_bootloader && !has_winusb
// }

fn setup_logs() {
    if wdi::set_log_level(wdi::LogLevel::Info).is_err() {
        log::error!("Could not set libwdi log level");
    }
}

// Trampoline of type `EnumWindowsProc` to pass closures to C
// See: https://stackoverflow.com/a/32270215
unsafe extern "system" fn enum_windows_callback(window: HWND, param: LPARAM) -> BOOL {
    // Transform the user param into ref to the colsure
    let closure: &mut &mut EnumWindowsCallback = std::mem::transmute(param);
    closure(window).into()
}

type EnumWindowsCallback = dyn FnMut(HWND) -> bool;

// BOOL EnumWindows(WNDENUMPROC lpEnumFunc, LPARAM lParam)
fn enum_windows(mut f: impl FnMut(HWND) -> bool) -> bool {
    let mut f: &mut dyn FnMut(HWND) -> bool = &mut f;
    let f = &mut f;
    let param = LPARAM(f as *mut _ as isize);
    unsafe {
        WindowsAndMessaging::EnumWindows(Some(enum_windows_callback), param).into()
    }
}

fn get_current_proc_windows() -> Vec<HWND> {
    let pid = unsafe { Threading::GetCurrentProcessId() };

    let mut windows = Vec::new();
    let on_window = |window| {
        let mut win_pid: u32 = 0;
        let result = unsafe {
            WindowsAndMessaging::GetWindowThreadProcessId(window, Some(&mut win_pid as *mut _))
        };
        if result != 0 && win_pid == pid {
            windows.push(window);
        }
        true
    };

    if enum_windows(on_window) {
        windows
    } else {
        vec![]
    }
}

#[allow(dead_code)]
fn supported_drivers() {
    use wdi::DriverType::*;
    let types = [WinUsb, LibUsb0, LibUsbK, Cdc, User];
    log::info!("Supported drivers");
    for typ in types {
        if let Some(info) = wdi::is_driver_supported(typ) {
            log::info!("{:?}: supported, DriverInfo {{
  dwSignature: {},
  dwStrucVersion: {},
  dwFileVersionMS: {},
  dwFileVersionLS: {},
  dwProductVersionMS: {},
  dwProductVersionLS: {},
  dwFileFlagsMask: {},
  dwFileFlags: {},
  dwFileOS: {},
  dwFileType: {},
  dwFileSubtype: {},
  dwFileDateMS: {},
  dwFileDateLS: {},
}}",
            typ,
            info.0.dwSignature,
            info.0.dwStrucVersion,
            info.0.dwFileVersionMS,
            info.0.dwFileVersionLS,
            info.0.dwProductVersionMS,
            info.0.dwProductVersionLS,
            info.0.dwFileFlagsMask,
            info.0.dwFileFlags,
            info.0.dwFileOS,
            info.0.dwFileType,
            info.0.dwFileSubtype,
            info.0.dwFileDateMS,
            info.0.dwFileDateLS,
        );
        } else {
            log::info!("{:?}: not supported", typ);
        }
    }
}
