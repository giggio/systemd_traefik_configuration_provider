use anyhow::Result;
use std::{path::Path, sync::Arc};

use crate::{
    dbus::{DBusContext, JobEvent, UnitData, UnitList},
    helpers::sanitize_filename,
    infra::FileSystem,
    yaml::build_traefik_file_yaml,
};

pub async fn reconcile(
    dbus: &DBusContext<'_>,
    watched_units: &UnitList,
    fs: &dyn FileSystem,
    traefik_dir: &Path,
) -> Result<()> {
    let read = watched_units.read().await;
    for (unit_name, unit_data) in read.iter() {
        let started = match dbus.is_unit_running(unit_name.clone()).await {
            Ok(running) => running,
            Err(e) => {
                error!("Error checking if unit {unit_name} is running: {e}");
                false
            }
        };
        debug!(
            "Reconciling unit {} as {}started",
            unit_name,
            if started { "" } else { "not " }
        );
        if let Err(e) =
            handle_service_state_changed(dbus, started, unit_data, fs, traefik_dir).await
        {
            error!(
                "Error handling reconciliation of unit {}: {:#}",
                unit_name, e
            );
        }
    }
    Ok(())
}

pub async fn process_service_change_messages(
    watched: UnitList,
    dbus: DBusContext<'static>,
    fs: Arc<dyn FileSystem>,
    traefik_dir: &Path,
) -> Result<(
    tokio::sync::mpsc::Sender<JobEvent>,
    tokio::task::JoinHandle<()>,
)> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<JobEvent>(100);
    let dbus = dbus.clone();
    let traefik_dir = traefik_dir.to_owned();
    let handle = tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let result = if job.started {
                let units = watched.read().await;
                let Some(unit_data) = units.get(&job.unit_name) else {
                    debug!(
                        "Not generating yaml for unit {}, it is not tracked.",
                        job.unit_name
                    );
                    continue;
                };
                handle_service_state_changed(&dbus, true, unit_data, fs.as_ref(), &traefik_dir)
                    .await
            } else {
                // a unit dropped from tracking (e.g. lost its Traefik config) still needs its
                // yaml removed, so this does not depend on the unit data
                remove_unit_yaml(&job.unit_name, fs.as_ref(), &traefik_dir)
            };
            if let Err(e) = result {
                error!("Error handling service state change message: {:#}", e);
            } else {
                trace!("Message handled");
            }
        }
    });
    Ok((tx, handle))
}

pub async fn handle_service_state_changed(
    dbus: &DBusContext<'_>,
    started: bool,
    unit_data: &UnitData,
    fs: &dyn FileSystem,
    traefik_dir: &Path,
) -> Result<()> {
    trace!(
        "Handling start/stop for unit {}, started={started}",
        unit_data.name
    );
    if started {
        let lines = dbus
            .get_traefik_yaml_config_from_configuration_files(unit_data)
            .await?;
        let yaml_config = build_traefik_file_yaml(lines)?;
        write_unit_yaml(&unit_data.name, yaml_config, fs, traefik_dir)?;
    } else {
        remove_unit_yaml(&unit_data.name, fs, traefik_dir)?;
    }
    Ok(())
}

fn write_unit_yaml(
    unit: &str,
    yaml: String,
    fs: &dyn FileSystem,
    traefik_dir: &Path,
) -> Result<()> {
    let sanitized_filename = sanitize_filename(unit);
    let dest = traefik_dir.join(format!("{}.yml", sanitized_filename));

    if fs.exists(&dest)
        && fs
            .read_to_string(&dest)
            .is_ok_and(|current| current == yaml)
    {
        trace!("Unit yaml for {} at {} is up to date", unit, dest.display());
        return Ok(());
    }

    trace!("Unit yaml for {} at {} is {yaml}", unit, dest.display());
    fs.write(&dest, &yaml)?;
    info!("Wrote {}", dest.display());
    Ok(())
}

fn remove_unit_yaml(unit: &str, fs: &dyn FileSystem, traefik_dir: &Path) -> Result<()> {
    let safe = sanitize_filename(unit);
    let dest = traefik_dir.join(format!("{}.yml", safe));
    if !fs.exists(&dest) {
        return Ok(());
    }
    debug!("Removing unit yaml for {unit} from {}", dest.display());
    if fs.exists(&dest) {
        fs.remove_file(&dest)?;
    }
    info!("Removed {}", dest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::infra::tests::MockFileSystem;
    use pretty_assertions::assert_eq;
    use serial_test::serial;
    use tempfile::TempDir;

    #[test]
    #[serial]
    fn test_write_unit_yaml_creates_file() {
        let temp_dir = TempDir::new().unwrap();
        let canonical_temp_path = temp_dir.path().canonicalize().unwrap();
        let fs = MockFileSystem::new();
        let result = write_unit_yaml("test.service", "foo".to_string(), &fs, &canonical_temp_path);
        assert!(result.is_ok());
        let yaml_path = canonical_temp_path.join("test.service.yml");
        assert!(
            fs.file_exists_in_memory(yaml_path.to_str().unwrap()),
            "YAML path: {}",
            yaml_path.display()
        );
        let content = fs.get_file_content(yaml_path.to_str().unwrap()).unwrap();
        assert_eq!(content, "foo");
    }

    #[test]
    #[serial]
    fn test_write_unit_yaml_sanitizes_filename() {
        let temp_dir = TempDir::new().unwrap();
        let canonical_temp_path = temp_dir.path().canonicalize().unwrap();
        let fs = MockFileSystem::new();

        write_unit_yaml(
            "my@app!service.service",
            "foo".to_string(),
            &fs,
            &canonical_temp_path,
        )
        .unwrap();

        let yaml_path = canonical_temp_path.join("my_app_service.service.yml");
        assert!(fs.file_exists_in_memory(yaml_path.to_str().unwrap()));

        let content = fs.get_file_content(yaml_path.to_str().unwrap()).unwrap();
        assert_eq!(content, "foo");
    }

    #[test]
    #[serial]
    fn test_write_unit_yaml_replaces_changed_content() {
        let fs = MockFileSystem::new();
        let traefik_dir = PathBuf::from("/traefik");
        fs.add_file("/traefik/test.service.yml", "old");
        write_unit_yaml("test.service", "new".to_string(), &fs, &traefik_dir).unwrap();
        assert_eq!(
            fs.get_file_content("/traefik/test.service.yml").unwrap(),
            "new"
        );
    }

    #[test]
    #[serial]
    fn test_write_unit_yaml_does_not_rewrite_unchanged_content() {
        let fs = MockFileSystem::new();
        let traefik_dir = PathBuf::from("/traefik");
        fs.add_file("/traefik/test.service.yml", "same");
        write_unit_yaml("test.service", "same".to_string(), &fs, &traefik_dir).unwrap();
        assert_eq!(fs.write_count(), 0);
    }

    #[tokio::test]
    async fn test_repeated_start_jobs_rewrite_changed_labels() {
        // zbus only delivers the last ActiveState, so a quick restart is seen as active twice
        let fs = Arc::new(MockFileSystem::new());
        fs.add_file("/units/web.service", "[X-Traefik]\nLabel=traefik.a=1");
        let mut unit = crate::dbus::MockSystemdUnit::new();
        unit.expect_drop_in_paths().returning(|| Ok(vec![]));
        unit.expect_fragment_path()
            .returning(|| Ok("/units/web.service".to_string()));
        let watched = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::from([
            (
                "web.service".to_string(),
                UnitData::new_test("web.service", Box::new(unit)),
            ),
        ])));
        let dbus = DBusContext::new_test_context(
            Arc::new(crate::dbus::MockSystemdManager::new()),
            fs.clone(),
        );
        let (tx, handle) =
            process_service_change_messages(watched, dbus, fs.clone(), Path::new("/traefik"))
                .await
                .unwrap();
        let started = || JobEvent {
            unit_name: "web.service".to_string(),
            started: true,
        };

        tx.send(started()).await.unwrap();
        wait_for_file_content(&fs, "/traefik/web.service.yml", "a: 1\n").await;
        fs.add_file("/units/web.service", "[X-Traefik]\nLabel=traefik.b=2");
        tx.send(started()).await.unwrap();
        wait_for_file_content(&fs, "/traefik/web.service.yml", "b: 2\n").await;

        drop(tx);
        handle.await.unwrap();
    }

    async fn wait_for_file_content(fs: &MockFileSystem, path: &str, expected: &str) {
        let wait = async {
            while fs.get_file_content(path).as_deref() != Some(expected) {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(tokio::time::Duration::from_millis(500), wait)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{path} should contain {expected:?}, has {:?}",
                    fs.get_file_content(path)
                )
            });
    }

    #[tokio::test]
    async fn test_stop_job_removes_yaml_of_untracked_unit() {
        let fs = Arc::new(MockFileSystem::new());
        fs.add_file("/traefik/gone.service.yml", "old");
        let dbus = DBusContext::new_test_context(
            Arc::new(crate::dbus::MockSystemdManager::new()),
            fs.clone(),
        );
        let (tx, handle) = process_service_change_messages(
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            dbus,
            fs.clone(),
            Path::new("/traefik"),
        )
        .await
        .unwrap();

        tx.send(JobEvent {
            unit_name: "gone.service".to_string(),
            started: false,
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.unwrap();

        assert!(!fs.file_exists_in_memory("/traefik/gone.service.yml"));
    }

    #[test]
    #[serial]
    fn test_remove_unit_yaml_deletes_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap().to_string();
        let fs = MockFileSystem::new();
        let yaml_path = temp_dir.path().join("test.service.yml");
        fs.add_file(yaml_path.to_str().unwrap(), "dummy content".to_string());
        assert!(
            fs.file_exists_in_memory(yaml_path.to_str().unwrap()),
            "File should exist before delete: {}",
            yaml_path.display()
        );

        let result = remove_unit_yaml("test.service", &fs, &PathBuf::from(temp_path));
        assert!(
            result.is_ok(),
            "remove_unit_yaml should succeed, error: {:?}",
            result
        );
        assert!(
            !fs.file_exists_in_memory(yaml_path.to_str().unwrap()),
            "YAML file should be deleted at {}",
            yaml_path.display()
        );
    }

    #[test]
    #[serial]
    fn test_remove_unit_yaml_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let canonical_temp_path = temp_dir.path().canonicalize().unwrap();
        let fs = MockFileSystem::new();
        let result = remove_unit_yaml("nonexistent.service", &fs, &canonical_temp_path);
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_remove_unit_yaml_sanitizes_filename() {
        let temp_dir = TempDir::new().unwrap();
        let canonical_temp_path = temp_dir.path().canonicalize().unwrap();
        let fs = MockFileSystem::new();
        let yaml_path = canonical_temp_path.join("my_app_service.service.yml");
        fs.add_file(yaml_path.to_str().unwrap(), "dummy content".to_string());
        assert!(fs.file_exists_in_memory(yaml_path.to_str().unwrap()));
        let result = remove_unit_yaml("my@app!service.service", &fs, &canonical_temp_path);
        assert!(result.is_ok());
        assert!(!fs.file_exists_in_memory(yaml_path.to_str().unwrap()));
    }
}
