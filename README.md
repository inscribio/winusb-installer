# winusb-installer

Library for automated WinUSB driver installation from an application running without admin privileges.

This project uses [pbatard/libwdi](https://github.com/pbatard/libwdi) to install WinUSB driver on a Windows machine.
Driver installation requires admin privileges. To avoid having to always run your application
with elevated privileges this package implements the following approach:

* Start your main application.
* Create winusb-installer `Server` and start installation process.
* Spawn a subprocess (`Client`) with elevated privileges using Windows "runas".
* Connect `Server` and `Client` via IPC (Windows named pipes).
* Use a custom protocol to coordinate installation process between `Server` and `Client`.
* Retrieve installation results and stop `Client`.
