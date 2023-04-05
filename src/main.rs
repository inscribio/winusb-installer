use std::io::Write;

use winusb_installer::{Mode, InstallConfig};

fn init_logging(name: &str) {
    let name = name.to_string();
    env_logger::builder()
        .filter_level(log::LevelFilter::Trace)
        .format_timestamp(None)
        .format(move |buf, record| {
            // writeln!(buf, "[{} {} {}] {}",
            //     record.level(), name, record.module_path().unwrap_or("-"), record.args())
            writeln!(buf, "[{} {}] {}",
                record.level(), name, record.args())
        })
        .init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mode = winusb_installer::init();
    match mode {
        Mode::Server(mut server) => {
            init_logging("parent");
            log::info!("Starting");

            server.show_child_window(true);

            let devices: Vec<_> = server
                .visible_devices().unwrap()
                .into_iter()
                .filter(|dev| dev.driver.is_none())
                .collect();

            log::debug!("Devices = {devices:#?}");

            let config = InstallConfig {
                vendor: "my-vendor".to_string(),
                driver_path: "C:\\usb_driver".to_string(),
                inf_name: "MyWinUSB.inf".to_string(),
            };

            if !devices.is_empty() {
                log::info!("Driver installation needed, installing.");
                server.install(config, &devices, |_| {}).await.unwrap();
            } else {
                log::info!("Driver installation not needed.");
            }
        },
        Mode::Client(mut client) => {
            init_logging("child");
            log::info!("Starting with: {}", client.pipe_name());
            client.serve().await.unwrap();
        },
    }
}
