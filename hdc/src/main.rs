//! HDC Host Tool

mod auth;
mod client;
mod net_discover;
mod parser;
mod server;
mod transfer;
mod usb;

#[cfg(target_os = "windows")]
mod usb_hotplug_windows;

use std::io::ErrorKind;
use tracing::{debug, error, info};

#[tokio::main]
async fn main() {
    // Setup tracing, use local timezone for timestamps
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
        .init();

    let parsed_cmd = match parser::parse_command(std::env::args()) {
        Ok(cmd) => cmd,
        Err(e) => {
            if e.kind() == ErrorKind::Other {
                println!("{}", e.to_string());
            } else {
                eprintln!("Parse error: {e}");
            }
            return;
        }
    };

    debug!("parsed cmd: {:#?}", parsed_cmd);

    if parsed_cmd.run_in_server {
        info!("Running in server mode on {}", parsed_cmd.server_addr);
        server::set_forward_listen_ip(parsed_cmd.forward_listen_ip.clone());
        let connect_map = server::ConnectMap::new();
        let tcp_map = server::TcpMap::new();
        let usb_map = server::UsbMap::new();
        if let Err(e) = server::run_server_mode(&parsed_cmd.server_addr, connect_map, tcp_map, usb_map).await {
            error!("Server error: {e}");
        }
    } else {
        debug!(
            "in client mode, cmd: {:?}, parameters: {:?}",
            parsed_cmd.command, parsed_cmd.parameters
        );

        if parsed_cmd.command.is_none() {
            println!("Unknown operation command...");
            println!("{}", parser::usage());
            return;
        }

        if let Err(e) = client::run_client_mode(parsed_cmd).await {
            match e.kind() {
                ErrorKind::Other => println!("[Fail]{}", e),
                _ => {
                    debug!("client exit with err: {e:?}");
                }
            }
        }
    }
}
