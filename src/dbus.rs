use std::{collections::HashMap, path::Path, pin::Pin, sync::Arc};

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

pub type UnitList = Arc<RwLock<HashMap<String, UnitData>>>;
pub struct UnitData {
    proxy: Box<dyn SystemdUnit>,
    pub name: String,
}

#[derive(Debug)]
pub struct JobEvent {
    pub unit_name: String,
    pub started: bool,
}

#[derive(Debug)]
pub struct NewUnit {
    pub unit: String,
}

pub struct NewUnitArgs {
    id: String,
    unit: String,
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
    async fn receive_unit_new(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<NewUnitArgs>> + Send>>>;
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
    pub async fn watch_units(
        &self,
        units_lock: UnitList,
    ) -> Result<(
        Vec<tokio::task::JoinHandle<()>>,
        tokio::sync::mpsc::Receiver<NewUnit>,
    )> {
        let (tx_new_unit, rx_new_unit) = tokio::sync::mpsc::channel::<NewUnit>(100);
        let units_lock_new_clone = units_lock.clone();
        let self_new_clone = self.clone();
        let h1 = tokio::spawn(async move {
            let mut unit_new_stream = match self_new_clone.manager.receive_unit_new().await {
                Ok(s) => s,
                Err(e) => {
                    error!("Error receiving unit new stream: {:#}", e);
                    return;
                }
            };
            while let Some(unit_res) = unit_new_stream.next().await {
                let args = match unit_res {
                    Ok(args) => args,
                    Err(e) => {
                        error!("Error getting unit args: {:#}", e);
                        continue;
                    }
                };
                let name = args.id.clone();
                {
                    let units = units_lock_new_clone.read().await;
                    if units.contains_key(&name) {
                        continue;
                    }
                }
                if let Some(unit_data) = self_new_clone
                    .create_unit(name.clone(), args.unit.clone())
                    .await
                {
                    let mut units = units_lock_new_clone.write().await;
                    trace!("Adding unit {} to watched list", &unit_data.name);
                    let unit_name = unit_data.name.clone();
                    units.insert(unit_name.clone(), unit_data);
                    if let Err(e) = tx_new_unit.send(NewUnit { unit: unit_name }).await {
                        error!("Error sending new unit event: {:#}", e);
                    }
                }
            }
        });
        Ok((vec![h1], rx_new_unit))
    }

    pub async fn get_messages(
        &self,
        tx_new_job_event: tokio::sync::mpsc::Sender<JobEvent>,
        watched_map: UnitList,
        mut rx_new_unit: tokio::sync::mpsc::Receiver<NewUnit>,
    ) -> Result<()> {
        let units = watched_map.read().await.keys().cloned().collect::<Vec<_>>();
        let initial_watched_units_count = units.len();
        debug!("Watching {} units.", initial_watched_units_count);
        let mut has_initial_units = initial_watched_units_count > 0;
        let streams_of_changes = units
            .into_iter()
            .async_map(|unit_name| async move { self.create_changes_stream(unit_name).await })
            .await
            .into_iter()
            .flatten();
        let mut changes_stream = futures::stream::select_all(streams_of_changes);
        let mut done = false;
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = match signal(SignalKind::interrupt()) {
            Err(err) => {
                eprintln!("Error listening for SIGINT (Ctrl+C) signal: {err}");
                std::process::exit(1);
            }
            Ok(sigint) => sigint,
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Err(err) => {
                eprintln!("Error listening for SIGTERM signal: {err}");
                std::process::exit(1);
            }
            Ok(sigterm) => sigterm,
        };
        loop {
            if done {
                break;
            }
            tokio::select! {
                event = rx_new_unit.recv() => {
                    if let Some(event) = event {
                        info!("New unit being wached: {}", &event.unit);
                        let new_unit_changes_stream = self.create_changes_stream(event.unit).await;
                        changes_stream.extend(new_unit_changes_stream);
                        has_initial_units  = true;
                    } else {
                        trace!("New unit channel closed");
                        done = true;
                    }
                }
                property_changed_fut_opt = changes_stream.next(), if has_initial_units => {
                    if let Some(property_changed_fut) = property_changed_fut_opt {
                        let job = match property_changed_fut.await {
                            Some(the_job) => the_job,
                            None => continue,
                        };
                        match tx_new_job_event.send(job).await {
                            Err(e) => error!("Error sending message: {:#}", e),
                            Ok(_) => trace!("Message sent to channel"),
                        }
                    } else {
                        trace!("Changes streams closed");
                        done = true;
                    }
                }
                _ = sigint.recv() => {
                    trace!("SIGINT (Ctrl+C) received, stopping...");
                    done = true;
                }
                _ = sigterm.recv() => {
                    trace!("SIGTERM received, stopping...");
                    done = true;
                }
            };
        }
        Ok(())
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
            if let Some(unit_data) = self.create_unit(name, object_path.to_string()).await {
                units_map.insert(unit_data.name.clone(), unit_data);
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

    async fn create_unit(&self, name: String, object_path: String) -> Option<UnitData> {
        if !name.ends_with(".service") {
            return None;
        }
        let proxy = match self.manager.get_unit(object_path).await {
            Ok(proxy) => proxy,
            Err(e) => {
                error!("Error getting unit: {:#}", e);
                return None;
            }
        };
        let unit_data = UnitData {
            proxy,
            name: name.clone(),
        };
        let is_tracked = match self
            .has_traefik_config_in_configuration_files(&unit_data)
            .await
        {
            Ok(is_tracked) => is_tracked,
            Err(e) => {
                error!("Error getting unit: {:#}", e);
                return None;
            }
        };
        if is_tracked {
            return Some(unit_data);
        }
        None
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
                trace!("New job: {:?}", &job);
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
        let units_lock = Arc::new(RwLock::new(HashMap::new()));

        let (handles, mut rx_new_unit) = context.watch_units(units_lock.clone()).await.unwrap();

        let event =
            tokio::time::timeout(tokio::time::Duration::from_millis(500), rx_new_unit.recv())
                .await
                .expect("Timeout waiting for new unit event")
                .expect("Channel closed before receiving event");
        assert_eq!(event.unit, "new.service");

        let units = units_lock.read().await;
        assert!(units.contains_key("new.service"));

        for h in handles {
            h.abort();
        }
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
                Ok(
                    Box::pin(futures::stream::iter(vec![Ok("active".to_string())]))
                        as Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
                )
            });
            Ok(Box::new(u))
        });

        let context =
            DBusContext::new_test_context(Arc::new(mock_manager), Arc::new(MockFileSystem::new()));
        let units_lock = Arc::new(RwLock::new(HashMap::new()));

        tx_new_unit
            .send(NewUnit {
                unit: "new.service".to_string(),
            })
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

        drop(tx_new_unit); // Now we can drop it to close rx_new_unit in get_messages
        handle.await.unwrap().unwrap();
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
