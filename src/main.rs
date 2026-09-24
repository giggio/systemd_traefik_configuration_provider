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

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use std::sync::Arc;

#[tokio::main]
async fn main() -> std::result::Result<(), String> {
    let args = args::Cli::parse();
    let _logger_handle = logger::start(args.verbosity.log_level_filter(), args.log_hide_date)
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
    let (watched, watch_join_handles, rx_watch_events) = dbus.load_and_watch_units().await?;
    if log_enabled!(log::Level::Info) {
        let read = watched.read().await;
        let watched_units = read.keys().cloned().collect::<Vec<_>>();
        if watched_units.is_empty() {
            info!("No units initially being watched. They might all be stopped.");
        } else {
            info!("Initial watched units: {}", watched_units.join(", "));
        }
    }
    if let Err(e) = reconcile(&dbus, &watched, fs.as_ref(), &traefik_dir).await {
        error!("initial reconcile error: {:#}", e);
    }

    let (tx_new_job_event, process_msgs_join_handle) =
        process_service_change_messages(watched.clone(), dbus.clone(), fs.clone(), &traefik_dir)
            .await?;
    let background_tasks = watch_join_handles
        .into_iter()
        .chain([process_msgs_join_handle])
        .collect();
    supervise(
        dbus.get_messages(tx_new_job_event, watched, rx_watch_events),
        background_tasks,
    )
    .await?;
    trace!("Shutting down");
    Ok(())
}

/// Runs the main loop until it ends, failing if any background task ends first: they are all
/// meant to live as long as the process, and one that stops means updates silently stop too.
/// Failing lets systemd restart the service.
async fn supervise(
    main_loop: impl Future<Output = Result<()>>,
    background_tasks: Vec<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    if background_tasks.is_empty() {
        return main_loop.await;
    }
    let abort_handles = background_tasks
        .iter()
        .map(|task| task.abort_handle())
        .collect::<Vec<_>>();
    let result = tokio::select! {
        biased;
        result = main_loop => result,
        (task_result, _, _) = futures::future::select_all(background_tasks) => match task_result {
            Ok(()) => Err(anyhow!("a background task stopped unexpectedly")),
            Err(e) => Err(anyhow!("a background task failed: {e}")),
        },
    };
    for abort_handle in abort_handles {
        abort_handle.abort();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ctor::ctor(unsafe)]
    static LOGGER: flexi_logger::LoggerHandle = {
        let logger_handle_result = logger::start(log::LevelFilter::Off, false);
        match logger_handle_result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error starting logger: {e}");
                panic!("Error starting logger: {e}");
            }
        }
    };

    #[tokio::test]
    async fn test_supervise_fails_when_a_background_task_panics() {
        let panicking_task = tokio::spawn(async { panic!("boom") });
        let error = supervise(std::future::pending(), vec![panicking_task])
            .await
            .unwrap_err();
        assert!(
            error.to_string().starts_with("a background task failed"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn test_supervise_fails_when_a_background_task_returns() {
        let returning_task = tokio::spawn(async {});
        let error = supervise(std::future::pending(), vec![returning_task])
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "a background task stopped unexpectedly");
    }

    #[tokio::test]
    async fn test_supervise_stops_background_tasks_when_main_loop_ends() {
        let (tx_alive, rx_alive) = tokio::sync::oneshot::channel::<()>();
        let long_running_task = tokio::spawn(async move {
            let _tx_alive = tx_alive;
            std::future::pending::<()>().await
        });
        supervise(async { Ok(()) }, vec![long_running_task])
            .await
            .unwrap();
        assert!(
            rx_alive.await.is_err(),
            "the background task should have been aborted"
        );
    }
}
