use std::{
    collections::{HashMap, HashSet},
    path::Path,
    pin::Pin,
    sync::Arc,
};

use crate::{helpers::*, infra::FileSystem};

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tokio::sync::RwLock;
use zbus::Connection;

#[derive(Clone)]
pub struct DBusContext<'a> {
    #[allow(dead_code)] // the connection is held by the manager, so we don't have to leak it
    conn: Option<Box<Connection>>,
    manager: Arc<dyn SystemdManager + 'a + Send + Sync>,
    fs: Arc<dyn FileSystem>,
}

/// Values are `Arc`s so a unit can be used after the lock is released, since every use
/// involves D-Bus calls and holding the lock across them would block the unit watcher.
pub type UnitList = Arc<RwLock<HashMap<String, Arc<UnitData>>>>;
pub struct UnitData {
    proxy: Box<dyn SystemdUnit>,
    pub name: String,
}

#[cfg(test)]
impl UnitData {
    pub fn new_test(name: impl Into<String>, proxy: Box<dyn SystemdUnit>) -> Self {
        Self {
            proxy,
            name: name.into(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct JobEvent {
    pub unit_name: String,
    pub started: bool,
}

/// What the unit watcher asks the main loop to do.
#[derive(Debug, PartialEq)]
pub enum WatchEvent {
    /// Follow the ActiveState of a newly tracked unit.
    Watch(String),
    /// Generate or remove a unit's yaml without an ActiveState change, e.g. after a reload.
    Job(JobEvent),
}

enum UnitSignal {
    New(NewUnitArgs),
    Reloaded,
}

pub struct NewUnitArgs {
    id: String,
    unit: String,
}

enum UnitCreation {
    /// The unit has Traefik config and should be watched.
    Tracked(UnitData),
    /// The unit was inspected and has no Traefik config; it is safe to stop probing it.
    Untracked,
    /// Inspecting the unit failed; the error may be transient, so the result is not cached.
    Failed,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait SystemdManager: Send + Sync {
    #[allow(clippy::type_complexity)]
    async fn list_units(
        &self,
    ) -> Result<
        Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            zbus::zvariant::OwnedObjectPath,
            u32,
            String,
            zbus::zvariant::OwnedObjectPath,
        )>,
    >;
    async fn subscribe(&self) -> Result<()>;
    async fn receive_unit_new(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<NewUnitArgs>> + Send>>>;
    async fn receive_reloading(&self) -> Result<Pin<Box<dyn Stream<Item = Result<bool>> + Send>>>;
    async fn load_unit(&self, name: &str) -> Result<String>;
    async fn get_unit(&self, path: String) -> Result<Box<dyn SystemdUnit>>;
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait SystemdUnit: Send + Sync {
    async fn drop_in_paths(&self) -> Result<Vec<String>>;
    async fn fragment_path(&self) -> Result<String>;
    async fn active_state(&self) -> Result<String>;
    async fn receive_active_state_changed(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>>;
}

impl DBusContext<'static> {
    /// Subscribes to UnitNew before listing the loaded units, so a unit loaded while the list
    /// is being built is still announced, instead of falling in the gap between both.
    pub async fn load_and_watch_units(
        &self,
    ) -> Result<(
        UnitList,
        Vec<tokio::task::JoinHandle<()>>,
        tokio::sync::mpsc::Receiver<WatchEvent>,
    )> {
        // systemd only emits unit signals on the system bus while some client is subscribed.
        self.manager
            .subscribe()
            .await
            .context("subscribing to systemd signals")?;
        let (drain_handle, rx_unit_signals) = self.drain_unit_signals().await?;
        let units_lock = match self.list_units().await {
            Ok(units_lock) => units_lock,
            Err(e) => {
                drain_handle.abort();
                return Err(e);
            }
        };
        let (process_handle, rx_watch_events) =
            self.watch_units(units_lock.clone(), rx_unit_signals);
        Ok((
            units_lock,
            vec![drain_handle, process_handle],
            rx_watch_events,
        ))
    }

    /// zbus stops reading the socket (method replies included) while any signal stream's
    /// queue is full, so the signal streams must never wait on D-Bus calls made while
    /// inspecting a unit, or a burst of UnitNew (e.g. on daemon-reload) deadlocks the
    /// connection. They are drained into an unbounded channel on their own task instead.
    async fn drain_unit_signals(
        &self,
    ) -> Result<(
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::UnboundedReceiver<UnitSignal>,
    )> {
        let unit_new_stream = self
            .manager
            .receive_unit_new()
            .await
            .context("receiving unit new stream")?
            .map(|unit_res| unit_res.map(UnitSignal::New));
        let reloaded_stream = self
            .manager
            .receive_reloading()
            .await
            .context("receiving reloading stream")?
            .filter_map(|reloading_res| {
                std::future::ready(match reloading_res {
                    // the units are only consistent once the reload finishes
                    Ok(true) => None,
                    Ok(false) => Some(Ok(UnitSignal::Reloaded)),
                    Err(e) => Some(Err(e)),
                })
            });
        let mut signals = futures::stream::select(unit_new_stream, reloaded_stream);
        let (tx_unit_signals, rx_unit_signals) =
            tokio::sync::mpsc::unbounded_channel::<UnitSignal>();
        let drain_handle = tokio::spawn(async move {
            while let Some(signal_res) = signals.next().await {
                match signal_res {
                    Ok(signal) => {
                        if tx_unit_signals.send(signal).is_err() {
                            trace!("Unit signals channel closed");
                            return;
                        }
                    }
                    Err(e) => error!("Error getting unit signal: {:#}", e),
                }
            }
        });
        Ok((drain_handle, rx_unit_signals))
    }

    fn watch_units(
        &self,
        units_lock: UnitList,
        mut rx_unit_signals: tokio::sync::mpsc::UnboundedReceiver<UnitSignal>,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::Receiver<WatchEvent>,
    ) {
        let (tx_watch_events, rx_watch_events) = tokio::sync::mpsc::channel::<WatchEvent>(100);
        let self_clone = self.clone();
        let process_handle = tokio::spawn(async move {
            // Names already inspected and found to carry no Traefik config. systemd emits a
            // fresh UnitNew every time a not-found unit is re-materialized, and inspecting it
            // re-materializes it, so without this cache a single ghost reference would make the
            // daemon probe it forever in a tight loop.
            let mut ignored: HashSet<String> = HashSet::new();
            while let Some(signal) = rx_unit_signals.recv().await {
                match signal {
                    UnitSignal::New(args) => {
                        self_clone
                            .track_new_unit(args, &units_lock, &mut ignored, &tx_watch_events)
                            .await
                    }
                    UnitSignal::Reloaded => {
                        info!("systemd reloaded, checking units again");
                        self_clone
                            .resync_units(&units_lock, &mut ignored, &tx_watch_events)
                            .await
                    }
                }
            }
        });
        (process_handle, rx_watch_events)
    }

    async fn track_new_unit(
        &self,
        args: NewUnitArgs,
        units_lock: &UnitList,
        ignored: &mut HashSet<String>,
        tx_watch_events: &tokio::sync::mpsc::Sender<WatchEvent>,
    ) {
        let name = args.id;
        if ignored.contains(&name) {
            return;
        }
        if units_lock.read().await.contains_key(&name) {
            trace!("Already watching unit {}", name);
            return;
        }
        trace!("Watching unit {}", name);
        match self.create_unit(name.clone(), args.unit).await {
            UnitCreation::Tracked(unit_data) => {
                trace!("Adding unit {} to watched list", name);
                units_lock
                    .write()
                    .await
                    .insert(name.clone(), Arc::new(unit_data));
                send_watch_event(tx_watch_events, WatchEvent::Watch(name)).await;
            }
            UnitCreation::Untracked => {
                trace!("Did not create unit {}", name);
                ignored.insert(name);
            }
            UnitCreation::Failed => {
                trace!(
                    "Did not create unit {} (inspection failed, will retry)",
                    name
                );
            }
        }
    }

    /// A daemon-reload can add or remove the Traefik config of any unit, or change its labels,
    /// without an ActiveState change, so every loaded or tracked unit is inspected again.
    async fn resync_units(
        &self,
        units_lock: &UnitList,
        ignored: &mut HashSet<String>,
        tx_watch_events: &tokio::sync::mpsc::Sender<WatchEvent>,
    ) {
        ignored.clear();
        let mut candidates = match self.manager.list_units().await {
            Ok(units) => units
                .into_iter()
                .map(|unit| (unit.0, unit.6.to_string()))
                .collect::<HashMap<_, _>>(),
            Err(e) => {
                error!("Error listing units after reload: {:#}", e);
                return;
            }
        };
        let tracked_names = units_lock.read().await.keys().cloned().collect::<Vec<_>>();
        for name in tracked_names {
            if candidates.contains_key(&name) {
                continue;
            }
            match self.manager.load_unit(&name).await {
                Ok(object_path) => {
                    candidates.insert(name, object_path);
                }
                Err(e) => error!("Error loading unit {name} after reload: {:#}", e),
            }
        }
        for (name, object_path) in candidates {
            let was_tracked = units_lock.read().await.contains_key(&name);
            match self.create_unit(name.clone(), object_path).await {
                UnitCreation::Tracked(unit_data) => {
                    let started = match unit_data.proxy.active_state().await {
                        Ok(state) => state == "active",
                        Err(e) => {
                            error!("Error getting state of unit {name} after reload: {:#}", e);
                            continue;
                        }
                    };
                    units_lock
                        .write()
                        .await
                        .insert(name.clone(), Arc::new(unit_data));
                    if !was_tracked {
                        info!("Unit {} now has Traefik config", name);
                        send_watch_event(tx_watch_events, WatchEvent::Watch(name.clone())).await;
                    }
                    let job = JobEvent {
                        unit_name: name,
                        started,
                    };
                    send_watch_event(tx_watch_events, WatchEvent::Job(job)).await;
                }
                UnitCreation::Untracked => {
                    if was_tracked {
                        info!("Unit {} no longer has Traefik config", name);
                        units_lock.write().await.remove(&name);
                        let job = JobEvent {
                            unit_name: name.clone(),
                            started: false,
                        };
                        send_watch_event(tx_watch_events, WatchEvent::Job(job)).await;
                    }
                    ignored.insert(name);
                }
                UnitCreation::Failed => {}
            }
        }
    }

    pub async fn get_messages(
        &self,
        tx_new_job_event: tokio::sync::mpsc::Sender<JobEvent>,
        watched_map: UnitList,
        mut rx_watch_events: tokio::sync::mpsc::Receiver<WatchEvent>,
    ) -> Result<()> {
        let units = watched_map.read().await.keys().cloned().collect::<Vec<_>>();
        let mut streamed_units = units.iter().cloned().collect::<HashSet<_>>();
        let initial_watched_units_count = units.len();
        debug!("Watching {} units.", initial_watched_units_count);
        let streams_of_changes = units
            .into_iter()
            .async_map(|unit_name| async move { self.create_changes_stream(unit_name).await })
            .await
            .into_iter()
            .flatten();
        let mut changes_stream = futures::stream::select_all(streams_of_changes);
        let mut has_streams = !changes_stream.is_empty();
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint =
            signal(SignalKind::interrupt()).context("listening for SIGINT (Ctrl+C) signal")?;
        let mut sigterm =
            signal(SignalKind::terminate()).context("listening for SIGTERM signal")?;
        loop {
            tokio::select! {
                event = rx_watch_events.recv() => {
                    match event {
                        Some(WatchEvent::Watch(unit_name)) => {
                            if !streamed_units.insert(unit_name.clone()) {
                                trace!("Already following state of unit {}", unit_name);
                                continue;
                            }
                            info!("New unit being wached: {}", unit_name);
                            let new_unit_changes_stream = self.create_changes_stream(unit_name).await;
                            changes_stream.extend(new_unit_changes_stream);
                            has_streams = !changes_stream.is_empty();
                        }
                        Some(WatchEvent::Job(job)) => send_job(&tx_new_job_event, job).await?,
                        None => anyhow::bail!("the unit watcher stopped"),
                    }
                }
                property_changed_fut_opt = changes_stream.next(), if has_streams => {
                    let Some(property_changed_fut) = property_changed_fut_opt else {
                        anyhow::bail!("all unit state streams closed");
                    };
                    if let Some(job) = property_changed_fut.await {
                        send_job(&tx_new_job_event, job).await?;
                    }
                }
                _ = sigint.recv() => {
                    trace!("SIGINT (Ctrl+C) received, stopping...");
                    return Ok(());
                }
                _ = sigterm.recv() => {
                    trace!("SIGTERM received, stopping...");
                    return Ok(());
                }
            };
        }
    }
}

async fn send_job(
    tx_new_job_event: &tokio::sync::mpsc::Sender<JobEvent>,
    job: JobEvent,
) -> Result<()> {
    tx_new_job_event
        .send(job)
        .await
        .context("the job processing stopped")?;
    trace!("Message sent to channel");
    Ok(())
}

async fn send_watch_event(
    tx_watch_events: &tokio::sync::mpsc::Sender<WatchEvent>,
    event: WatchEvent,
) {
    if let Err(e) = tx_watch_events.send(event).await {
        error!("Error sending watch event: {:#}", e);
    }
}

impl<'a> DBusContext<'a> {
    pub async fn new() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connect to system bus")?;
        let proxy = crate::manager::ManagerProxy::new(&conn).await?;
        Ok(Self {
            conn: Some(Box::new(conn)),
            manager: Arc::new(RealSystemdManager { proxy }),
            fs: Arc::new(crate::infra::RealFileSystem),
        })
    }

    #[cfg(test)]
    pub fn new_test_context(
        manager: Arc<dyn SystemdManager + 'a + Send + Sync>,
        fs: Arc<dyn FileSystem>,
    ) -> Self {
        Self {
            manager,
            fs,
            conn: None,
        }
    }

    pub async fn list_units(&self) -> Result<UnitList> {
        let units = self.manager.list_units().await?;
        let mut units_map = HashMap::new();
        for unit in units {
            let name = unit.0;
            let object_path = unit.6;
            if let UnitCreation::Tracked(unit_data) =
                self.create_unit(name, object_path.to_string()).await
            {
                units_map.insert(unit_data.name.clone(), Arc::new(unit_data));
            }
        }
        let unit_list = Arc::new(RwLock::new(units_map));
        if log_enabled!(log::Level::Debug) {
            let units = unit_list.read().await;
            let names = units.keys().cloned().collect::<Vec<_>>();
            trace!("Loaded {} units. Units: {names:?}", names.len());
        }
        Ok(unit_list)
    }

    async fn create_unit(&self, name: String, object_path: String) -> UnitCreation {
        if !name.ends_with(".service") {
            return UnitCreation::Untracked;
        }
        trace!("Creating unit {}", name);
        let proxy = match self.manager.get_unit(object_path).await {
            Ok(proxy) => proxy,
            Err(e) => {
                error!("Error getting unit: {:#}", e);
                return UnitCreation::Failed;
            }
        };
        let unit_data = UnitData {
            proxy,
            name: name.clone(),
        };
        match self
            .has_traefik_config_in_configuration_files(&unit_data)
            .await
        {
            Ok(true) => UnitCreation::Tracked(unit_data),
            Ok(false) => UnitCreation::Untracked,
            Err(e) => {
                error!("Error getting unit: {:#}", e);
                UnitCreation::Failed
            }
        }
    }

    pub async fn is_unit_running(&self, unit_name: String) -> Result<bool> {
        let obj_path = self.manager.load_unit(unit_name.as_str()).await?;
        let state = self
            .manager
            .get_unit(obj_path.to_string())
            .await?
            .active_state()
            .await?;
        Ok(state == "active")
    }

    pub async fn get_traefik_yaml_config_from_configuration_files(
        &self,
        unit_data: &UnitData,
    ) -> Result<Vec<String>> {
        let files = self.get_config_files_for_unit(unit_data).await?;
        let lines = self
            .get_traefik_config_from_configuration_files(files)
            .await?;
        Ok(lines)
    }

    async fn get_config_files_for_unit(&self, unit_data: &UnitData) -> Result<Vec<String>> {
        let mut all_paths: Vec<_> = unit_data
            .proxy
            .drop_in_paths()
            .await?
            .into_iter()
            .filter(|p| self.fs.exists(std::path::Path::new(&p)))
            .collect();
        let fragment_path = unit_data.proxy.fragment_path().await?;
        if self.fs.exists(std::path::Path::new(&fragment_path)) {
            all_paths.push(fragment_path);
        }
        if all_paths.is_empty() {
            trace!("No config file for service: {}", unit_data.name);
        } else if all_paths.len() == 1 {
            trace!(
                "Config file for service {}: {}",
                unit_data.name, all_paths[0]
            );
        } else {
            trace!("Config files for service {}: {all_paths:?}", unit_data.name,);
        }
        Ok(all_paths)
    }

    async fn has_traefik_config_in_configuration_files(
        &self,
        unit_data: &UnitData,
    ) -> Result<bool> {
        let files = self.get_config_files_for_unit(unit_data).await?;
        for file in &files {
            trace!("Checking config file {}", file);
            let text = self.fs.read_to_string(Path::new(file))?;
            let parser = systemd_lsp::SystemdParser::new();
            let unit_config = parser.parse(&text);
            if unit_config.sections.contains_key("X-Traefik") {
                debug!("Found X-Traefik in {} for service {}", file, unit_data.name);
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn get_traefik_config_from_configuration_files(
        &self,
        files: Vec<String>,
    ) -> Result<Vec<String>> {
        let mut lines = vec![];
        for file in &files {
            let text = self.fs.read_to_string(Path::new(file))?;
            let parser = systemd_lsp::SystemdParser::new();
            let unit_config = parser.parse(&text);
            if let Some(section) = unit_config.sections.get("X-Traefik") {
                trace!("Found X-Traefik in {}", file);
                for directive in section.directives.iter().filter(|d| d.key == "Label") {
                    lines.push(directive.value.to_owned());
                }
            } else {
                trace!("Missing X-Traefik section in {}", file);
                continue;
            }
        }
        Ok(lines)
    }

    async fn create_changes_stream(
        &self,
        unit_name: String,
    ) -> Option<Pin<Box<dyn Stream<Item = impl Future<Output = Option<JobEvent>>> + Send>>> {
        let obj_path = match self.manager.load_unit(unit_name.as_str()).await {
            Ok(obj_path) => obj_path,
            Err(e) => {
                error!("Error loading unit: {:#}", e);
                return None;
            }
        };
        let unit_opt = match self.manager.get_unit(obj_path.to_string()).await {
            Ok(unit) => Some(unit),
            Err(e) => {
                error!("Error getting unit: {:#}", e);
                return None;
            }
        };
        let unit = match unit_opt {
            Some(unit) => unit,
            None => {
                error!("Error getting unit");
                return None;
            }
        };
        let stream = match unit.receive_active_state_changed().await {
            Ok(s) => s,
            Err(e) => {
                error!("Error getting active state changed stream: {:#}", e);
                return None;
            }
        }
        .map(move |property_changed| {
            let unit_name_clone = unit_name.clone();
            async move {
                let state = match property_changed {
                    Ok(x) => x,
                    Err(e) => {
                        error!("Error getting property changed: {:#}", e);
                        return None;
                    }
                };
                let job = JobEvent {
                    unit_name: unit_name_clone,
                    started: state == "active",
                };
                trace!("New job: {:?}", job);
                Some(job)
            }
        })
        .boxed();
        Some(stream)
    }
}

pub struct RealSystemdManager<'a> {
    proxy: crate::manager::ManagerProxy<'a>,
}

#[async_trait]
impl SystemdManager for RealSystemdManager<'static> {
    async fn list_units(
        &self,
    ) -> Result<
        Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            zbus::zvariant::OwnedObjectPath,
            u32,
            String,
            zbus::zvariant::OwnedObjectPath,
        )>,
    > {
        Ok(self.proxy.list_units().await?)
    }

    async fn subscribe(&self) -> Result<()> {
        Ok(self.proxy.subscribe().await?)
    }

    async fn receive_reloading(&self) -> Result<Pin<Box<dyn Stream<Item = Result<bool>> + Send>>> {
        let stream = self.proxy.receive_reloading().await?;
        Ok(Box::pin(stream.map(|msg| {
            let args = msg.args().map_err(|e| anyhow::anyhow!(e))?;
            Ok(*args.active())
        }))
            as Pin<Box<dyn Stream<Item = Result<bool>> + Send>>)
    }

    async fn receive_unit_new(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<NewUnitArgs>> + Send>>> {
        let stream = self.proxy.receive_unit_new().await?;
        Ok(Box::pin(stream.map(|msg| {
            let args = msg.args().map_err(|e| anyhow::anyhow!(e))?;
            Ok(NewUnitArgs {
                id: args.id().to_string(),
                unit: args.unit().to_string(),
            })
        }))
            as Pin<Box<dyn Stream<Item = Result<NewUnitArgs>> + Send>>)
    }

    async fn load_unit(&self, name: &str) -> Result<String> {
        let path = self.proxy.load_unit(name).await?;
        Ok(path.to_string())
    }

    async fn get_unit(&self, path: String) -> Result<Box<dyn SystemdUnit>> {
        let proxy = crate::unit::UnitProxy::builder(self.proxy.as_ref().connection())
            .path(path)?
            .build()
            .await?;
        Ok(Box::new(RealSystemdUnit { proxy }) as Box<dyn SystemdUnit>)
    }
}

pub struct RealSystemdUnit<'a> {
    proxy: crate::unit::UnitProxy<'a>,
}

#[async_trait]
impl SystemdUnit for RealSystemdUnit<'static> {
    async fn drop_in_paths(&self) -> Result<Vec<String>> {
        Ok(self.proxy.drop_in_paths().await?)
    }

    async fn fragment_path(&self) -> Result<String> {
        Ok(self.proxy.fragment_path().await?)
    }

    async fn active_state(&self) -> Result<String> {
        Ok(self.proxy.active_state().await?)
    }

    async fn receive_active_state_changed(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let stream = self.proxy.receive_active_state_changed().await;
        Ok(Box::pin(stream.then(|msg| async move {
            let v = msg.get().await.map_err(|e| anyhow::anyhow!(e))?;
            Ok(v)
        }))
            as Pin<Box<dyn Stream<Item = Result<String>> + Send>>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::tests::MockFileSystem;
    use std::sync::Arc;

    fn expect_subscriptions(mock_manager: &mut MockSystemdManager) {
        mock_manager
            .expect_subscribe()
            .times(1)
            .return_once(|| Ok(()));
        mock_manager
            .expect_receive_reloading()
            .return_once(|| Ok(Box::pin(futures::stream::empty())));
    }

    #[tokio::test]
    async fn test_is_unit_running() {
        let mut mock_manager = MockSystemdManager::new();
        let mut mock_unit = MockSystemdUnit::new();

        mock_manager
            .expect_load_unit()
            .with(mockall::predicate::eq("test.service"))
            .returning(|_| Ok("/org/freedesktop/systemd1/unit/test_service".to_string()));

        mock_unit
            .expect_active_state()
            .returning(|| Ok("active".to_string()));

        mock_manager
            .expect_get_unit()
            .with(mockall::predicate::eq(
                "/org/freedesktop/systemd1/unit/test_service".to_string(),
            ))
            .return_once(|_| Ok(Box::new(mock_unit)));

        let context =
            DBusContext::new_test_context(Arc::new(mock_manager), Arc::new(MockFileSystem::new()));

        let is_running = context
            .is_unit_running("test.service".to_string())
            .await
            .unwrap();
        assert!(is_running);
    }

    #[tokio::test]
    async fn test_is_unit_running_is_false_when_not_active() {
        let mut mock_manager = MockSystemdManager::new();
        mock_manager
            .expect_load_unit()
            .returning(|_| Ok("/obj/test".to_string()));
        mock_manager.expect_get_unit().returning(|_| {
            let mut u = MockSystemdUnit::new();
            u.expect_active_state()
                .returning(|| Ok("deactivating".to_string()));
            Ok(Box::new(u))
        });
        let context =
            DBusContext::new_test_context(Arc::new(mock_manager), Arc::new(MockFileSystem::new()));

        let is_running = context
            .is_unit_running("test.service".to_string())
            .await
            .unwrap();

        assert!(!is_running);
    }

    #[tokio::test]
    async fn test_create_changes_stream_is_none_when_the_unit_cannot_be_followed() {
        let mut failing_load = MockSystemdManager::new();
        failing_load
            .expect_load_unit()
            .returning(|_| Err(anyhow::anyhow!("no such unit")));

        let mut failing_get = MockSystemdManager::new();
        failing_get
            .expect_load_unit()
            .returning(|_| Ok("/obj/test".to_string()));
        failing_get
            .expect_get_unit()
            .returning(|_| Err(anyhow::anyhow!("no such object")));

        let mut failing_stream = MockSystemdManager::new();
        failing_stream
            .expect_load_unit()
            .returning(|_| Ok("/obj/test".to_string()));
        failing_stream.expect_get_unit().returning(|_| {
            let mut u = MockSystemdUnit::new();
            u.expect_receive_active_state_changed()
                .returning(|| Err(anyhow::anyhow!("no match rule")));
            Ok(Box::new(u))
        });

        for mock_manager in [failing_load, failing_get, failing_stream] {
            let context = DBusContext::new_test_context(
                Arc::new(mock_manager),
                Arc::new(MockFileSystem::new()),
            );
            assert!(
                context
                    .create_changes_stream("test.service".to_string())
                    .await
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn test_load_and_watch_units_fails_when_subscribe_fails() {
        let mut mock_manager = MockSystemdManager::new();
        mock_manager
            .expect_subscribe()
            .returning(|| Err(anyhow::anyhow!("access denied")));
        let context =
            DBusContext::new_test_context(Arc::new(mock_manager), Arc::new(MockFileSystem::new()));

        let error = context.load_and_watch_units().await.err().unwrap();

        assert_eq!(error.to_string(), "subscribing to systemd signals");
    }

    #[tokio::test]
    async fn test_load_and_watch_units_fails_when_listing_fails() {
        let mut mock_manager = MockSystemdManager::new();
        expect_subscriptions(&mut mock_manager);
        mock_manager
            .expect_receive_unit_new()
            .return_once(|| Ok(Box::pin(futures::stream::pending()) as UnitNewStream));
        mock_manager
            .expect_list_units()
            .returning(|| Err(anyhow::anyhow!("bus closed")));
        let context =
            DBusContext::new_test_context(Arc::new(mock_manager), Arc::new(MockFileSystem::new()));

        let error = context.load_and_watch_units().await.err().unwrap();

        assert_eq!(error.to_string(), "bus closed");
    }

    #[tokio::test]
    async fn test_list_units() {
        let mut mock_manager = MockSystemdManager::new();
        let mock_fs = Arc::new(MockFileSystem::new());

        let unit_name = "test.service".to_string();
        let object_path = zbus::zvariant::OwnedObjectPath::try_from(
            "/org/freedesktop/systemd1/unit/test_service",
        )
        .unwrap();

        let unit_name_clone = unit_name.clone();
        let object_path_clone = object_path.clone();
        mock_manager.expect_list_units().return_once(move || {
            Ok(vec![(
                unit_name_clone,
                "loaded".into(),
                "active".into(),
                "running".into(),
                "".into(),
                "".into(),
                object_path_clone.clone(),
                0,
                "".into(),
                object_path_clone,
            )])
        });

        mock_manager.expect_get_unit().returning(move |_| {
            let mut u = MockSystemdUnit::new();
            u.expect_drop_in_paths().returning(|| Ok(vec![]));
            u.expect_fragment_path()
                .returning(|| Ok("/lib/systemd/system/test.service".to_string()));
            Ok(Box::new(u))
        });

        mock_fs.add_file(
            "/lib/systemd/system/test.service",
            "[X-Traefik]\nLabel=test",
        );

        let context = DBusContext::new_test_context(Arc::new(mock_manager), mock_fs);
        let units = context.list_units().await.unwrap();
        let units = units.read().await;
        assert!(units.contains_key("test.service"));
    }

    #[tokio::test]
    async fn test_get_traefik_yaml_config_from_configuration_files() {
        let mut mock_unit = MockSystemdUnit::new();
        mock_unit.expect_drop_in_paths().returning(|| {
            Ok(vec![
                "/etc/systemd/system/test.service.d/traefik.conf".to_string(),
            ])
        });
        mock_unit
            .expect_fragment_path()
            .returning(|| Ok("/lib/systemd/system/test.service".to_string()));

        let mock_fs = Arc::new(MockFileSystem::new());
        mock_fs.add_file(
            "/etc/systemd/system/test.service.d/traefik.conf",
            "[X-Traefik]\nLabel=label1",
        );
        mock_fs.add_file(
            "/lib/systemd/system/test.service",
            "[X-Traefik]\nLabel=label2",
        );

        let context = DBusContext::new_test_context(Arc::new(MockSystemdManager::new()), mock_fs);

        let unit_data = UnitData {
            proxy: Box::new(mock_unit),
            name: "test.service".to_string(),
        };

        let config = context
            .get_traefik_yaml_config_from_configuration_files(&unit_data)
            .await
            .unwrap();
        assert_eq!(config, vec!["label1".to_string(), "label2".to_string()]);
    }

    #[tokio::test]
    async fn test_watch_units() {
        let mut mock_manager = MockSystemdManager::new();

        let args = NewUnitArgs {
            id: "new.service".to_string(),
            unit: "/obj/path".to_string(),
        };
        mock_manager.expect_list_units().return_once(|| Ok(vec![]));
        expect_subscriptions(&mut mock_manager);
        mock_manager.expect_receive_unit_new().return_once(move || {
            Ok(Box::pin(futures::stream::iter(vec![Ok(args)]))
                as Pin<Box<dyn Stream<Item = Result<NewUnitArgs>> + Send>>)
        });

        mock_manager.expect_get_unit().returning(|_| {
            let mut u = MockSystemdUnit::new();
            u.expect_drop_in_paths().returning(|| Ok(vec![]));
            u.expect_fragment_path()
                .returning(|| Ok("/lib/systemd/system/new.service".to_string()));
            Ok(Box::new(u))
        });

        let mock_fs = Arc::new(MockFileSystem::new());
        mock_fs.add_file("/lib/systemd/system/new.service", "[X-Traefik]\nLabel=new");

        let context = DBusContext::new_test_context(Arc::new(mock_manager), mock_fs);
        let (units_lock, handles, mut rx_new_unit) = context.load_and_watch_units().await.unwrap();

        let event =
            tokio::time::timeout(tokio::time::Duration::from_millis(500), rx_new_unit.recv())
                .await
                .expect("Timeout waiting for new unit event")
                .expect("Channel closed before receiving event");
        assert_eq!(event, WatchEvent::Watch("new.service".to_string()));

        let units = units_lock.read().await;
        assert!(units.contains_key("new.service"));

        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_watch_units_probes_untracked_unit_only_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut mock_manager = MockSystemdManager::new();

        // systemd announces the same not-found unit twice (in reality: endlessly, as the
        // daemon's own probing re-materializes it). The daemon must inspect it only once.
        let args1 = NewUnitArgs {
            id: "ghost.service".to_string(),
            unit: "/obj/path".to_string(),
        };
        let args2 = NewUnitArgs {
            id: "ghost.service".to_string(),
            unit: "/obj/path".to_string(),
        };
        mock_manager.expect_list_units().return_once(|| Ok(vec![]));
        expect_subscriptions(&mut mock_manager);
        mock_manager.expect_receive_unit_new().return_once(move || {
            Ok(Box::pin(futures::stream::iter(vec![Ok(args1), Ok(args2)]))
                as Pin<Box<dyn Stream<Item = Result<NewUnitArgs>> + Send>>)
        });

        let probe_count = Arc::new(AtomicUsize::new(0));
        let probe_count_clone = probe_count.clone();
        mock_manager.expect_get_unit().returning(move |_| {
            probe_count_clone.fetch_add(1, Ordering::SeqCst);
            let mut u = MockSystemdUnit::new();
            u.expect_drop_in_paths().returning(|| Ok(vec![]));
            u.expect_fragment_path()
                .returning(|| Ok("/lib/systemd/system/ghost.service".to_string()));
            Ok(Box::new(u))
        });

        // No config file on disk -> the unit is untracked.
        let mock_fs = Arc::new(MockFileSystem::new());

        let context = DBusContext::new_test_context(Arc::new(mock_manager), mock_fs);
        let (units_lock, handles, _rx_new_unit) = context.load_and_watch_units().await.unwrap();

        // The stream is finite, so the watch task ends once both events are drained.
        for h in handles {
            tokio::time::timeout(tokio::time::Duration::from_millis(500), h)
                .await
                .expect("watch task did not finish")
                .expect("watch task panicked");
        }

        assert_eq!(
            probe_count.load(Ordering::SeqCst),
            1,
            "an untracked unit must be inspected only once, then cached"
        );
        let units = units_lock.read().await;
        assert!(!units.contains_key("ghost.service"));
    }

    #[tokio::test]
    async fn test_load_and_watch_units_announces_unit_loaded_while_listing() {
        type Subscriber = futures::channel::mpsc::UnboundedSender<Result<NewUnitArgs>>;
        let subscriber: Arc<std::sync::Mutex<Option<Subscriber>>> = Default::default();
        let mut mock_manager = MockSystemdManager::new();

        let subscriber_clone = subscriber.clone();
        expect_subscriptions(&mut mock_manager);
        mock_manager.expect_receive_unit_new().return_once(move || {
            let (tx, rx) = futures::channel::mpsc::unbounded();
            *subscriber_clone.lock().unwrap() = Some(tx);
            Ok(Box::pin(rx) as UnitNewStream)
        });

        // systemd only announces UnitNew to whoever is subscribed at that moment.
        let subscriber_clone = subscriber.clone();
        mock_manager.expect_list_units().return_once(move || {
            if let Some(tx) = subscriber_clone.lock().unwrap().as_ref() {
                tx.unbounded_send(Ok(NewUnitArgs {
                    id: "late.service".to_string(),
                    unit: "/obj/path".to_string(),
                }))
                .unwrap();
            }
            Ok(vec![])
        });

        mock_manager.expect_get_unit().returning(|_| {
            let mut u = MockSystemdUnit::new();
            u.expect_drop_in_paths().returning(|| Ok(vec![]));
            u.expect_fragment_path()
                .returning(|| Ok("/lib/systemd/system/late.service".to_string()));
            Ok(Box::new(u))
        });

        let mock_fs = Arc::new(MockFileSystem::new());
        mock_fs.add_file(
            "/lib/systemd/system/late.service",
            "[X-Traefik]\nLabel=late",
        );

        let context = DBusContext::new_test_context(Arc::new(mock_manager), mock_fs);
        let (units_lock, handles, mut rx_new_unit) = context.load_and_watch_units().await.unwrap();

        let event =
            tokio::time::timeout(tokio::time::Duration::from_millis(500), rx_new_unit.recv())
                .await
                .expect("unit loaded while listing units was never announced")
                .expect("Channel closed before receiving event");
        assert_eq!(event, WatchEvent::Watch("late.service".to_string()));
        assert!(units_lock.read().await.contains_key("late.service"));

        for h in handles {
            h.abort();
        }
    }

    type UnitNewStream = Pin<Box<dyn Stream<Item = Result<NewUnitArgs>> + Send>>;

    /// Mimics zbus: signals go through a small bounded queue and the socket reader only gets to
    /// a method reply after it managed to enqueue every signal that arrived before it.
    struct QueueLimitedSystemdManager {
        unit_new_stream: std::sync::Mutex<Option<UnitNewStream>>,
        rx_replies_ready: tokio::sync::watch::Receiver<bool>,
    }

    #[async_trait]
    impl SystemdManager for QueueLimitedSystemdManager {
        #[allow(clippy::type_complexity)]
        async fn list_units(
            &self,
        ) -> Result<
            Vec<(
                String,
                String,
                String,
                String,
                String,
                String,
                zbus::zvariant::OwnedObjectPath,
                u32,
                String,
                zbus::zvariant::OwnedObjectPath,
            )>,
        > {
            Ok(vec![])
        }

        async fn subscribe(&self) -> Result<()> {
            Ok(())
        }

        async fn receive_reloading(
            &self,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<bool>> + Send>>> {
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn receive_unit_new(&self) -> Result<UnitNewStream> {
            Ok(self.unit_new_stream.lock().unwrap().take().unwrap())
        }

        async fn load_unit(&self, _name: &str) -> Result<String> {
            unimplemented!()
        }

        async fn get_unit(&self, path: String) -> Result<Box<dyn SystemdUnit>> {
            self.rx_replies_ready
                .clone()
                .wait_for(|ready| *ready)
                .await?;
            let mut u = MockSystemdUnit::new();
            u.expect_drop_in_paths().returning(|| Ok(vec![]));
            u.expect_fragment_path().returning(move || Ok(path.clone()));
            Ok(Box::new(u))
        }
    }

    #[tokio::test]
    async fn test_watch_units_keeps_draining_unit_new_while_inspecting_unit() {
        const QUEUE_CAPACITY: usize = 2;
        let (mut tx_signal, rx_signal) =
            futures::channel::mpsc::channel::<Result<NewUnitArgs>>(QUEUE_CAPACITY);
        let (tx_replies_ready, rx_replies_ready) = tokio::sync::watch::channel(false);
        let manager = QueueLimitedSystemdManager {
            unit_new_stream: std::sync::Mutex::new(Some(Box::pin(rx_signal))),
            rx_replies_ready,
        };
        let mock_fs = Arc::new(MockFileSystem::new());
        mock_fs.add_file("/web.service", "[X-Traefik]\nLabel=web");
        let context = DBusContext::new_test_context(Arc::new(manager), mock_fs);
        let (units_lock, handles, mut rx_new_unit) = context.load_and_watch_units().await.unwrap();

        // A burst of UnitNew signals, larger than the queue, like systemd emits on daemon-reload.
        let socket_reader = tokio::spawn(async move {
            use futures::SinkExt;
            let names = std::iter::once("web.service".to_string())
                .chain((0..QUEUE_CAPACITY * 4).map(|i| format!("other{i}.service")));
            for name in names {
                let args = NewUnitArgs {
                    unit: format!("/{name}"),
                    id: name,
                };
                tx_signal.send(Ok(args)).await.unwrap();
            }
            tx_replies_ready.send(true).unwrap();
            tx_signal
        });

        let event = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            rx_new_unit.recv(),
        )
        .await
        .expect("deadlock: unit inspection waited on a reply stuck behind the full UnitNew queue")
        .expect("Channel closed before receiving event");
        assert_eq!(event, WatchEvent::Watch("web.service".to_string()));
        assert!(units_lock.read().await.contains_key("web.service"));

        socket_reader.abort();
        for h in handles {
            h.abort();
        }
    }

    /// Serves `org.freedesktop.systemd1.Unit` the way systemd does: the file paths are declared
    /// `const`, so no PropertiesChanged is ever sent when they change on daemon-reload.
    struct FakeSystemdUnitObject {
        drop_in_paths: Arc<std::sync::Mutex<Vec<String>>>,
        fragment_path: Arc<std::sync::Mutex<String>>,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl FakeSystemdUnitObject {
        #[zbus(property(emits_changed_signal = "const"))]
        fn drop_in_paths(&self) -> Vec<String> {
            self.drop_in_paths.lock().unwrap().clone()
        }

        #[zbus(property(emits_changed_signal = "const"))]
        fn fragment_path(&self) -> String {
            self.fragment_path.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn test_real_unit_reads_file_paths_changed_by_daemon_reload() {
        const UNIT_PATH: &str = "/org/freedesktop/systemd1/unit/web_2eservice";
        let drop_in_paths = Arc::new(std::sync::Mutex::new(vec![
            "/nix/store/old/web.conf".into(),
        ]));
        let fragment_path = Arc::new(std::sync::Mutex::new("/nix/store/old/web.service".into()));
        let (server_stream, client_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = zbus::connection::Builder::async_io_unix_stream(server_stream)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .serve_at(
                UNIT_PATH,
                FakeSystemdUnitObject {
                    drop_in_paths: drop_in_paths.clone(),
                    fragment_path: fragment_path.clone(),
                },
            )
            .unwrap()
            .build();
        let client = zbus::connection::Builder::async_io_unix_stream(client_stream)
            .p2p()
            .build();
        let (_server, client) = futures::try_join!(server, client).unwrap();
        let unit = RealSystemdUnit {
            proxy: crate::unit::UnitProxy::builder(&client)
                .path(UNIT_PATH)
                .unwrap()
                .build()
                .await
                .unwrap(),
        };
        assert_eq!(
            unit.drop_in_paths().await.unwrap(),
            vec!["/nix/store/old/web.conf"]
        );
        assert_eq!(
            unit.fragment_path().await.unwrap(),
            "/nix/store/old/web.service"
        );

        *drop_in_paths.lock().unwrap() = vec!["/nix/store/new/web.conf".into()];
        *fragment_path.lock().unwrap() = "/nix/store/new/web.service".into();

        assert_eq!(
            unit.drop_in_paths().await.unwrap(),
            vec!["/nix/store/new/web.conf"],
            "DropInPaths was served from a stale cache"
        );
        assert_eq!(
            unit.fragment_path().await.unwrap(),
            "/nix/store/new/web.service",
            "FragmentPath was served from a stale cache"
        );
    }

    #[tokio::test]
    async fn test_get_messages_does_not_follow_again_a_unit_already_in_the_map() {
        // the watcher can insert a unit and send its Watch before get_messages takes its
        // snapshot of the map, so the unit shows up in both
        let (tx_job, _rx_job) = tokio::sync::mpsc::channel(10);
        let (tx_watch_events, rx_watch_events) = tokio::sync::mpsc::channel(10);
        let mut mock_manager = MockSystemdManager::new();
        mock_manager
            .expect_load_unit()
            .returning(|_| Ok("/obj/web".to_string()));
        mock_manager.expect_get_unit().times(1).returning(|_| {
            let mut u = MockSystemdUnit::new();
            u.expect_receive_active_state_changed().return_once(|| {
                Ok(Box::pin(futures::stream::pending())
                    as Pin<Box<dyn Stream<Item = Result<String>> + Send>>)
            });
            Ok(Box::new(u))
        });
        let context =
            DBusContext::new_test_context(Arc::new(mock_manager), Arc::new(MockFileSystem::new()));
        let units_lock = Arc::new(RwLock::new(HashMap::from([(
            "web.service".to_string(),
            Arc::new(UnitData::new_test(
                "web.service",
                Box::new(MockSystemdUnit::new()),
            )),
        )])));
        tx_watch_events
            .send(WatchEvent::Watch("web.service".to_string()))
            .await
            .unwrap();
        drop(tx_watch_events);

        let error = context
            .get_messages(tx_job, units_lock, rx_watch_events)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "the unit watcher stopped");
    }

    #[tokio::test]
    async fn test_get_messages_fails_when_job_processing_stopped() {
        let (tx_job, rx_job) = tokio::sync::mpsc::channel(10);
        drop(rx_job);
        let (tx_watch_events, rx_watch_events) = tokio::sync::mpsc::channel(10);
        let job = JobEvent {
            unit_name: "web.service".to_string(),
            started: true,
        };
        tx_watch_events.send(WatchEvent::Job(job)).await.unwrap();
        let context = DBusContext::new_test_context(
            Arc::new(MockSystemdManager::new()),
            Arc::new(MockFileSystem::new()),
        );

        let error = context
            .get_messages(
                tx_job,
                Arc::new(RwLock::new(HashMap::new())),
                rx_watch_events,
            )
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "the job processing stopped");
    }

    #[allow(clippy::type_complexity)]
    fn list_units_row(
        name: &str,
        object_path: &str,
    ) -> (
        String,
        String,
        String,
        String,
        String,
        String,
        zbus::zvariant::OwnedObjectPath,
        u32,
        String,
        zbus::zvariant::OwnedObjectPath,
    ) {
        let object_path = zbus::zvariant::OwnedObjectPath::try_from(object_path).unwrap();
        (
            name.to_string(),
            "".into(),
            "loaded".into(),
            "active".into(),
            "running".into(),
            "".into(),
            object_path.clone(),
            0,
            "".into(),
            object_path,
        )
    }

    #[tokio::test]
    async fn test_reload_checks_units_again() {
        let mock_fs = Arc::new(MockFileSystem::new());
        mock_fs.add_file("/units/losing", "[X-Traefik]\nLabel=a");
        mock_fs.add_file("/units/gaining", "[Service]");
        mock_fs.add_file("/units/keeping", "[X-Traefik]\nLabel=c");

        let mut mock_manager = MockSystemdManager::new();
        mock_manager.expect_subscribe().return_once(|| Ok(()));
        let (tx_reloading, rx_reloading) = futures::channel::mpsc::unbounded::<Result<bool>>();
        mock_manager
            .expect_receive_reloading()
            .return_once(move || Ok(Box::pin(rx_reloading)));
        mock_manager
            .expect_receive_unit_new()
            .return_once(|| Ok(Box::pin(futures::stream::pending()) as UnitNewStream));
        mock_manager.expect_list_units().returning(|| {
            Ok(["losing", "gaining", "keeping"]
                .map(|name| list_units_row(&format!("{name}.service"), &format!("/obj/{name}")))
                .to_vec())
        });
        mock_manager.expect_get_unit().returning(|object_path| {
            let fragment_path = object_path.replace("/obj/", "/units/");
            let mut u = MockSystemdUnit::new();
            u.expect_drop_in_paths().returning(|| Ok(vec![]));
            u.expect_fragment_path()
                .returning(move || Ok(fragment_path.clone()));
            u.expect_active_state()
                .returning(|| Ok("active".to_string()));
            Ok(Box::new(u))
        });

        let context = DBusContext::new_test_context(Arc::new(mock_manager), mock_fs.clone());
        let (units_lock, handles, mut rx_watch_events) =
            context.load_and_watch_units().await.unwrap();
        let mut tracked = units_lock.read().await.keys().cloned().collect::<Vec<_>>();
        tracked.sort();
        assert_eq!(tracked, vec!["keeping.service", "losing.service"]);

        mock_fs.add_file("/units/losing", "[Service]");
        mock_fs.add_file("/units/gaining", "[X-Traefik]\nLabel=b");
        tx_reloading.unbounded_send(Ok(true)).unwrap();
        tx_reloading.unbounded_send(Ok(false)).unwrap();

        let mut events = vec![];
        for _ in 0..4 {
            let event = tokio::time::timeout(
                tokio::time::Duration::from_millis(500),
                rx_watch_events.recv(),
            )
            .await
            .expect("Timeout waiting for watch event")
            .expect("Channel closed before receiving event");
            events.push(event);
        }
        let job = |unit_name: &str, started| {
            WatchEvent::Job(JobEvent {
                unit_name: unit_name.to_string(),
                started,
            })
        };
        for expected in [
            WatchEvent::Watch("gaining.service".to_string()),
            job("gaining.service", true),
            job("keeping.service", true),
            job("losing.service", false),
        ] {
            assert!(
                events.contains(&expected),
                "missing {expected:?} in {events:?}"
            );
        }
        let mut tracked = units_lock.read().await.keys().cloned().collect::<Vec<_>>();
        tracked.sort();
        assert_eq!(tracked, vec!["gaining.service", "keeping.service"]);

        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_watch_units_retries_unit_whose_inspection_failed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut mock_manager = MockSystemdManager::new();
        expect_subscriptions(&mut mock_manager);
        mock_manager.expect_list_units().returning(|| Ok(vec![]));
        mock_manager.expect_receive_unit_new().return_once(|| {
            let announcements = (0..2).map(|_| {
                Ok(NewUnitArgs {
                    id: "web.service".to_string(),
                    unit: "/obj/web".to_string(),
                })
            });
            Ok(
                Box::pin(futures::stream::iter(announcements).chain(futures::stream::pending()))
                    as UnitNewStream,
            )
        });
        let get_unit_calls = AtomicUsize::new(0);
        mock_manager.expect_get_unit().returning(move |_| {
            if get_unit_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(anyhow::anyhow!("transient failure"));
            }
            let mut u = MockSystemdUnit::new();
            u.expect_drop_in_paths().returning(|| Ok(vec![]));
            u.expect_fragment_path()
                .returning(|| Ok("/units/web".to_string()));
            Ok(Box::new(u))
        });
        let mock_fs = Arc::new(MockFileSystem::new());
        mock_fs.add_file("/units/web", "[X-Traefik]\nLabel=web");
        let context = DBusContext::new_test_context(Arc::new(mock_manager), mock_fs);

        let (units_lock, handles, mut rx_watch_events) =
            context.load_and_watch_units().await.unwrap();

        let event = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            rx_watch_events.recv(),
        )
        .await
        .expect("a failed inspection must not stop the unit from being tracked later")
        .unwrap();
        assert_eq!(event, WatchEvent::Watch("web.service".to_string()));
        assert!(units_lock.read().await.contains_key("web.service"));
        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_reload_changes_nothing_when_listing_fails() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut mock_manager = MockSystemdManager::new();
        mock_manager.expect_subscribe().return_once(|| Ok(()));
        let (tx_reloading, rx_reloading) = futures::channel::mpsc::unbounded::<Result<bool>>();
        mock_manager
            .expect_receive_reloading()
            .return_once(move || Ok(Box::pin(rx_reloading)));
        mock_manager
            .expect_receive_unit_new()
            .return_once(|| Ok(Box::pin(futures::stream::pending()) as UnitNewStream));
        let list_units_calls = Arc::new(AtomicUsize::new(0));
        let list_units_calls_clone = list_units_calls.clone();
        mock_manager.expect_list_units().returning(move || {
            if list_units_calls_clone.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(vec![list_units_row("web.service", "/obj/web")])
            } else {
                Err(anyhow::anyhow!("bus closed"))
            }
        });
        mock_manager.expect_get_unit().returning(|_| {
            let mut u = MockSystemdUnit::new();
            u.expect_drop_in_paths().returning(|| Ok(vec![]));
            u.expect_fragment_path()
                .returning(|| Ok("/units/web".to_string()));
            Ok(Box::new(u))
        });
        let mock_fs = Arc::new(MockFileSystem::new());
        mock_fs.add_file("/units/web", "[X-Traefik]\nLabel=web");
        let context = DBusContext::new_test_context(Arc::new(mock_manager), mock_fs);
        let (units_lock, handles, mut rx_watch_events) =
            context.load_and_watch_units().await.unwrap();

        tx_reloading.unbounded_send(Ok(false)).unwrap();
        let wait_for_reload = async {
            while list_units_calls.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(tokio::time::Duration::from_millis(500), wait_for_reload)
            .await
            .expect("the reload was not handled");
        tokio::task::yield_now().await;

        assert!(rx_watch_events.try_recv().is_err());
        assert!(units_lock.read().await.contains_key("web.service"));
        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_get_messages_forwards_jobs_and_follows_each_unit_once() {
        let (tx_job, mut rx_job) = tokio::sync::mpsc::channel(10);
        let (tx_watch_events, rx_watch_events) = tokio::sync::mpsc::channel(10);

        let mut mock_manager = MockSystemdManager::new();
        mock_manager
            .expect_load_unit()
            .returning(|_| Ok("/obj/path/new".to_string()));
        mock_manager.expect_get_unit().times(1).returning(|_| {
            let mut u = MockSystemdUnit::new();
            u.expect_receive_active_state_changed().return_once(|| {
                Ok(Box::pin(futures::stream::pending())
                    as Pin<Box<dyn Stream<Item = Result<String>> + Send>>)
            });
            Ok(Box::new(u))
        });
        let context =
            DBusContext::new_test_context(Arc::new(mock_manager), Arc::new(MockFileSystem::new()));

        let job = JobEvent {
            unit_name: "gone.service".to_string(),
            started: false,
        };
        for event in [
            WatchEvent::Watch("new.service".to_string()),
            WatchEvent::Watch("new.service".to_string()),
            WatchEvent::Job(job),
        ] {
            tx_watch_events.send(event).await.unwrap();
        }
        drop(tx_watch_events);

        let units_lock = Arc::new(RwLock::new(HashMap::new()));
        context
            .get_messages(tx_job, units_lock, rx_watch_events)
            .await
            .expect_err("a closed watcher channel must be an error");

        assert_eq!(
            rx_job.recv().await,
            Some(JobEvent {
                unit_name: "gone.service".to_string(),
                started: false,
            })
        );
    }

    #[tokio::test]
    async fn test_get_messages() {
        let (tx_job, mut rx_job) = tokio::sync::mpsc::channel(10);
        let (tx_new_unit, rx_new_unit) = tokio::sync::mpsc::channel(10);

        let mut mock_manager = MockSystemdManager::new();
        mock_manager
            .expect_load_unit()
            .returning(|_| Ok("/obj/path/new".to_string()));

        mock_manager.expect_get_unit().returning(|_| {
            let mut u = MockSystemdUnit::new();
            u.expect_receive_active_state_changed().return_once(|| {
                Ok(Box::pin(
                    futures::stream::iter(vec![Ok("active".to_string())])
                        .chain(futures::stream::pending()),
                )
                    as Pin<Box<dyn Stream<Item = Result<String>> + Send>>)
            });
            Ok(Box::new(u))
        });

        let context =
            DBusContext::new_test_context(Arc::new(mock_manager), Arc::new(MockFileSystem::new()));
        let units_lock = Arc::new(RwLock::new(HashMap::new()));

        tx_new_unit
            .send(WatchEvent::Watch("new.service".to_string()))
            .await
            .unwrap();

        let context_clone = context.clone();
        let handle = tokio::spawn(async move {
            context_clone
                .get_messages(tx_job, units_lock, rx_new_unit)
                .await
        });

        let job = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx_job.recv())
            .await
            .expect("Timeout waiting for job event")
            .expect("Channel closed before receiving job");
        assert_eq!(job.unit_name, "new.service");
        assert!(job.started);

        drop(tx_new_unit);
        let error = handle.await.unwrap().unwrap_err();
        assert_eq!(error.to_string(), "the unit watcher stopped");
    }

    fn setup(
        files_contents: impl IntoIterator<Item = impl Into<String>>,
    ) -> (Vec<String>, DBusContext<'static>) {
        let mock_fs = Arc::new(MockFileSystem::new());
        let mut files = vec![];
        for (i, content) in files_contents.into_iter().enumerate() {
            let random_path = format!("/tmp/test_{i}.service");
            mock_fs.add_file(random_path.clone(), content);
            files.push(random_path);
        }
        let mock_manager = Arc::new(MockSystemdManager::new());
        let context = DBusContext::new_test_context(mock_manager, mock_fs.clone());
        (files, context)
    }

    #[tokio::test]
    async fn test_get_traefik_config_from_configuration_files_with_traefik_section() {
        let (files, context) = setup([r#"[Unit]
Description=Test Service

[Service]
Type=simple
ExecStart=/usr/bin/test

[X-Traefik]
Label=test.service.label1
Label=test.service.label2
"#]);

        let result = context
            .get_traefik_config_from_configuration_files(files)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "test.service.label1");
        assert_eq!(result[1], "test.service.label2");
    }

    #[tokio::test]
    async fn test_get_traefik_config_from_configuration_files_without_traefik_section() {
        let (files, context) = setup([r#"[Unit]
Description=Test Service

[Service]
Type=simple
ExecStart=/usr/bin/test
"#]);

        let result = context
            .get_traefik_config_from_configuration_files(files)
            .await
            .unwrap();

        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_get_traefik_config_from_configuration_files_multiple_files() {
        let (files, context) = setup([
            r#"[X-Traefik]
Label=file1.label1
Label=file1.label2
"#,
            r#"[X-Traefik]
Label=file2.label1
"#,
        ]);

        let result = context
            .get_traefik_config_from_configuration_files(files)
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "file1.label1");
        assert_eq!(result[1], "file1.label2");
        assert_eq!(result[2], "file2.label1");
    }

    #[tokio::test]
    async fn test_traefik_config_parsing_edge_cases() {
        let (files, context) = setup([r#"[X-Traefik]
"#]);

        let result = context
            .get_traefik_config_from_configuration_files(files)
            .await
            .unwrap();

        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_traefik_config_with_mixed_directives() {
        let (files, context) = setup([r#"[X-Traefik]
Label=traefik.label1
OtherDirective=should_be_ignored
Label=traefik.label2
AnotherDirective=also_ignored
Label=traefik.label3
"#]);

        let result = context
            .get_traefik_config_from_configuration_files(files)
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "traefik.label1");
        assert_eq!(result[1], "traefik.label2");
        assert_eq!(result[2], "traefik.label3");
    }

    #[tokio::test]
    async fn test_traefik_label_extraction_with_multiple_sections() {
        let (files, context) = setup([r#"[Unit]
Description=Multi Section Service

[Service]
Type=simple
ExecStart=/usr/bin/app

[X-Traefik]
Label=traefik.http.routers.app.rule=Host(`app.example.com`)
Label=traefik.http.routers.app.entrypoints=websecure
Label=traefik.http.services.app.loadbalancer.server.port=8080

[Install]
WantedBy=multi-user.target
"#]);

        let result = context
            .get_traefik_config_from_configuration_files(files)
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        assert!(result[0].contains("routers.app.rule"));
        assert!(result[1].contains("entrypoints"));
        assert!(result[2].contains("loadbalancer.server.port"));
    }

    #[tokio::test]
    async fn test_config_parsing_with_special_characters_in_labels() {
        let (files, context) = setup([r#"[X-Traefik]
Label=traefik.http.routers.app.rule=Host(`app.example.com`) && PathPrefix(`/api`)
Label=traefik.http.middlewares.app-headers.headers.customrequestheaders.X-Custom-Header=value-with-dash
"#]);
        let result = context
            .get_traefik_config_from_configuration_files(files)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert!(result[0].contains("&&"));
        assert!(result[1].contains("X-Custom-Header"));
    }

    #[tokio::test]
    async fn test_multiple_files_with_and_without_traefik() {
        let (files, context) = setup([
            r#"[X-Traefik]
Label=app.traefik
"#,
            r#"[Unit]
Description=No Traefik

[Service]
ExecStart=/bin/true
"#,
        ]);

        let result = context
            .get_traefik_config_from_configuration_files(files)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "app.traefik");
    }
}
