//! Command line entry and dispatch.

use crate::VERSION;
use crate::error::AppError;
use crate::lifecycle::run_server;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cli {
    Version,
    Serve {
        config: Option<PathBuf>,
        host: IpAddr,
        port: u16,
    },
}

pub(crate) fn parse_cli<I>(arguments: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err(
            "usage: EMP [--version] | serve [--config PATH] --host HOST --port PORT".to_string(),
        );
    };
    if command == "--version" {
        return Ok(Cli::Version);
    }
    if command != "serve" {
        return Err(format!("unknown command: {command}"));
    }

    let mut config = None;
    let mut host = None;
    let mut port = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config" => {
                if config.is_some() {
                    return Err("--config was provided more than once".to_string());
                }
                config = Some(PathBuf::from(
                    arguments.next().ok_or("--config requires a value")?,
                ));
            }
            "--host" => {
                if host.is_some() {
                    return Err("--host was provided more than once".to_string());
                }
                host = Some(arguments.next().ok_or("--host requires a value")?);
            }
            "--port" => {
                if port.is_some() {
                    return Err("--port was provided more than once".to_string());
                }
                let raw = arguments.next().ok_or("--port requires a value")?;
                port = Some(
                    raw.parse::<u16>()
                        .map_err(|_| format!("invalid port: {raw}"))?,
                );
            }
            unknown => return Err(format!("unknown serve option: {unknown}")),
        }
    }

    let host = host.ok_or("serve requires --host")?;
    let port = port.ok_or("serve requires --port")?;
    let host = host
        .parse::<IpAddr>()
        .map_err(|_| format!("invalid host: {host}"))?;
    if !is_loopback(host) {
        return Err(AppError::HostNotLoopback.to_string());
    }
    Ok(Cli::Serve { config, host, port })
}

pub(crate) fn is_loopback(host: IpAddr) -> bool {
    host == IpAddr::V4(Ipv4Addr::LOCALHOST)
        || matches!(host, IpAddr::V6(address) if address.is_loopback())
}

fn print_version() {
    println!("EMP {VERSION}");
}

pub(crate) fn run() -> Result<(), String> {
    match parse_cli(std::env::args().skip(1))? {
        Cli::Version => print_version(),
        Cli::Serve { config, host, port } => {
            run_server(config.as_deref(), host, port).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
