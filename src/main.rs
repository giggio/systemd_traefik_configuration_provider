mod args;
mod dbus;
mod generation_engine;
mod helpers;
mod infra;
mod logger;
// auto-generated with: zbus-xmlgen system org.freedesktop.systemd1 /org/freedesktop/systemd1
#[allow(clippy::all)]
mod manager;
// auto-generated with: zbus-xmlgen system org.freedesktop.systemd1 /org/freedesktop/systemd1/unit/sleep_2eservice
#[allow(clippy::all)]
mod service;
// auto-generated with: zbus-xmlgen system org.freedesktop.systemd1 /org/freedesktop/systemd1/unit/sleep_2eservice
#[allow(clippy::all)]
mod unit;
mod yaml;

#[macro_use]
extern crate log;
use crate::{
    dbus::DBusContext,
    generation_engine::{process_service_change_messages, reconcile},
    infra::{FileSystem, RealFileSystem},
};

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;

#[tokio::main]
async fn main() -> std::result::Result<(), String> {
    let args = args::Cli::parse();
    let _logger_handle = logger::start(args.verbosity.log_level_filter())
        .map_err(|e| format!("Error starting logger: {e}"))?;
    if let Err(e) = run(args.traefik_out_dir).await.map_err(|e| e.to_string()) {
        error!("Got an error: {}", e);
        eprintln!("Got an error: {}", e);
        return Err(e);
    }
    Ok(())
}

async fn run(traefik_dir: std::path::PathBuf) -> Result<()> {
    let fs = Arc::new(RealFileSystem);
    fs.create_dir_all(&traefik_dir)
        .context("creating traefik dynamic output dir")?;
    info!("Traefik dynamic output dir: {}", traefik_dir.display());

    let dbus = DBusContext::new().await?;
    let watched = dbus.list_units().await?;
    if log_enabled!(log::Level::Info) {
        let read = watched.read().await;
        let watched_units = read.keys().cloned().collect::<Vec<_>>();
        if watched_units.is_empty() {
            info!("No units initially being watched. They might all be stopped.");
        } else {
            info!("Initial watched units: {}", watched_units.join(", "));
        }
    }
    let (watch_join_handles, rx_new_unit) = dbus.watch_units(watched.clone()).await;

    if let Err(e) = reconcile(&dbus, &watched, fs.as_ref(), &traefik_dir).await {
        error!("initial reconcile error: {:#}", e);
    }

    let (tx_new_job_event, process_msgs_join_handle) =
        process_service_change_messages(watched.clone(), dbus.clone(), fs.clone(), &traefik_dir)
            .await?;
    dbus.get_messages(tx_new_job_event, watched, rx_new_unit)
        .await?; // will block

    trace!("Shutting down");
    for handle in watch_join_handles
        .into_iter()
        .chain([process_msgs_join_handle])
    {
        handle.abort();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ctor::ctor]
    static LOGGER: flexi_logger::LoggerHandle = {
        let logger_handle_result = logger::start(log::LevelFilter::Off);
        match logger_handle_result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error starting logger: {e}");
                panic!("Error starting logger: {e}");
            }
        }
    };
}
