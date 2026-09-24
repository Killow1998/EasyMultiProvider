//! Command line entry and dispatch; desktop launch is the native package default.
use crate::VERSION;
use crate::error::AppError;
use crate::lifecycle::run_server;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::process::ExitCode;
mod control;
pub(crate) mod desktop;
mod help;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cli {
    Version,
    Help(Option<String>),
    Control(control::Control),
    ApplyUpdate(PathBuf),
    Serve {
        config: Option<PathBuf>,
        host: Option<String>,
        port: Option<u16>,
        open_browser: bool,
    },
}

pub(crate) fn parse_cli<I>(arguments: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Ok(Cli::Serve {
            config: None,
            host: None,
            port: None,
            open_browser: true,
        });
    };
    if command == "--version" {
        return Ok(Cli::Version);
    }
    if command == "--emp-apply-update" {
        let path = arguments
            .next()
            .ok_or("--emp-apply-update requires a plan")?;
        if arguments.next().is_some() {
            return Err("--emp-apply-update requires one plan".into());
        }
        return Ok(Cli::ApplyUpdate(PathBuf::from(path)));
    }
    if matches!(command.as_str(), "--help" | "-h") {
        return Ok(Cli::Help(None));
    }
    let arguments = arguments.collect::<Vec<_>>();
    if matches!(command.as_str(), "serve" | "doctor" | "restore")
        && arguments
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return Ok(Cli::Help(Some(command)));
    }
    if matches!(command.as_str(), "doctor" | "restore") {
        return control::Control::parse(&command, arguments.into_iter()).map(Cli::Control);
    }
    if command != "serve" {
        return Err(format!("unknown command: {command}"));
    }
    let mut config = None;
    let mut host = None;
    let mut port = None;
    let mut open_browser = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let (name, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value.to_owned()))
            });
        let mut next_value = || {
            inline
                .clone()
                .or_else(|| arguments.next())
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match name {
            "--config" => config = Some(PathBuf::from(next_value()?)),
            "--host" => host = Some(next_value()?),
            "--port" => {
                let raw = next_value()?;
                port = Some(
                    raw.parse::<u16>()
                        .map_err(|_| format!("invalid port: {raw}"))?,
                );
            }
            "--open-browser" if inline.is_none() => open_browser = true,
            _ => return Err(format!("unknown serve option: {argument}")),
        }
    }
    Ok(Cli::Serve {
        config,
        host,
        port,
        open_browser,
    })
}

pub(crate) fn is_loopback(host: IpAddr) -> bool {
    host == IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn serve(
    config: Option<PathBuf>,
    host: Option<String>,
    port: Option<u16>,
    open_browser: bool,
) -> Result<(), AppError> {
    let configured = emp_state::load_configuration(config.as_deref())?;
    let host = host
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| configured["host"].as_str().unwrap_or("127.0.0.1"));
    if host != "127.0.0.1" {
        return Err(AppError::HostNotLoopback);
    }
    let port = port.unwrap_or_else(|| configured["port"].as_u64().unwrap_or(4200) as u16);
    run_server(
        config.as_deref(),
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
        open_browser,
    )
}
fn serve_error(error: AppError) -> String {
    match error {
        AppError::ServiceOwned => "another EMP service owns this configuration",
        AppError::Config(_) | AppError::HostNotLoopback => "EMP configuration is invalid",
        AppError::Io(_) | AppError::Filesystem(_) => {
            "EMP integration state is not readable or writable"
        }
        _ => "EMP integration operation failed",
    }
    .to_owned()
}

pub(crate) fn run() -> Result<ExitCode, String> {
    let command = match parse_cli(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("usage: EMP [-h] [--version] COMMAND ...\nEMP: error: {error}");
            return Ok(ExitCode::from(2));
        }
    };
    match command {
        Cli::Version => print!("EMP {VERSION}{}", if cfg!(windows) { "\r\n" } else { "\n" }),
        Cli::Help(command) => {
            let message = help::text(command.as_deref());
            if cfg!(windows) {
                print!("{}", message.replace('\n', "\r\n"));
            } else {
                print!("{message}");
            }
        }
        Cli::Control(command) => return command.run(),
        Cli::ApplyUpdate(path) => {
            return emp_state::update::worker::run(&path)
                .map(ExitCode::from)
                .map_err(|error| error.to_string());
        }
        Cli::Serve {
            config,
            host,
            port,
            open_browser,
        } => serve(config, host, port, open_browser).map_err(serve_error)?,
    }
    Ok(ExitCode::SUCCESS)
}
