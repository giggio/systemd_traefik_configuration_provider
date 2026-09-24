use anyhow::{Result, anyhow};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub trait FileSystem: Send + Sync {
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn write(&self, path: &Path, contents: &str) -> Result<()>;
    fn exists(&self, path: &Path) -> bool;
    fn remove_file(&self, path: &Path) -> Result<()>;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    fn list_files(&self, dir: &Path) -> Result<Vec<PathBuf>>;
}

pub struct RealFileSystem;

impl FileSystem for RealFileSystem {
    fn read_to_string(&self, path: &Path) -> Result<String> {
        Ok(std::fs::read_to_string(path)?)
    }

    /// Traefik watches the directory, so the file is replaced with a rename instead of being
    /// truncated and rewritten, which Traefik could read half written. The temporary file name
    /// does not end in `.yml`, so Traefik ignores it.
    fn write(&self, path: &Path, contents: &str) -> Result<()> {
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow!("no file name in {}", path.display()))?;
        let tmp_path = path.with_file_name(format!(".{}.tmp", file_name.to_string_lossy()));
        std::fs::write(&tmp_path, contents)?;
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(anyhow!("removing {}: {}", path.display(), e))
            }
            _ => Ok(()),
        }
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        Ok(std::fs::create_dir_all(path)?)
    }

    fn list_files(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut files = vec![];
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.push(entry.path());
            }
        }
        Ok(files)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use anyhow::bail;
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    pub struct MockFileSystem {
        files: Arc<Mutex<HashMap<String, String>>>,
        write_count: AtomicUsize,
    }

    impl MockFileSystem {
        pub fn new() -> Self {
            Self {
                files: Arc::new(Mutex::new(HashMap::new())),
                write_count: AtomicUsize::new(0),
            }
        }

        pub fn add_file(&self, path: impl Into<String>, content: impl Into<String>) {
            self.files
                .lock()
                .unwrap()
                .insert(path.into(), content.into());
        }

        pub fn get_file_content(&self, path: impl AsRef<str>) -> Option<String> {
            self.files.lock().unwrap().get(path.as_ref()).cloned()
        }

        pub fn write_count(&self) -> usize {
            self.write_count.load(Ordering::SeqCst)
        }

        pub fn file_exists_in_memory(&self, path: impl AsRef<str>) -> bool {
            self.files.lock().unwrap().contains_key(path.as_ref())
        }
    }

    #[test]
    fn test_real_write_replaces_file_atomically() {
        use std::io::Read;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("web.service.yml");
        std::fs::write(&path, "old").unwrap();
        let mut reader_of_old_file = std::fs::File::open(&path).unwrap();

        RealFileSystem.write(&path, "new").unwrap();

        let mut seen_by_old_reader = String::new();
        reader_of_old_file
            .read_to_string(&mut seen_by_old_reader)
            .unwrap();
        assert_eq!(
            seen_by_old_reader, "old",
            "the file was truncated in place instead of replaced"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let files = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(files, vec!["web.service.yml"]);
    }

    #[test]
    fn test_real_exists_is_true_only_for_files() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file_path = temp_dir.path().join("web.service.yml");
        std::fs::write(&file_path, "").unwrap();

        assert!(RealFileSystem.exists(&file_path));
        assert!(!RealFileSystem.exists(temp_dir.path()));
        assert!(!RealFileSystem.exists(&temp_dir.path().join("missing.yml")));
        assert!(!RealFileSystem.exists(Path::new("")));
    }

    #[test]
    fn test_real_list_files_skips_directories() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("web.service.yml"), "").unwrap();
        std::fs::create_dir(temp_dir.path().join("subdir")).unwrap();

        let files = RealFileSystem.list_files(temp_dir.path()).unwrap();

        assert_eq!(files, vec![temp_dir.path().join("web.service.yml")]);
    }

    #[test]
    fn test_real_remove_file_of_missing_file_is_ok() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        assert!(
            RealFileSystem
                .remove_file(&temp_dir.path().join("missing.yml"))
                .is_ok()
        );
    }

    impl FileSystem for MockFileSystem {
        fn read_to_string(&self, path: &Path) -> Result<String> {
            let files = self.files.lock().unwrap();
            let path_str = path.to_str().ok_or_else(|| anyhow!("Invalid path"))?;
            match files.get(path_str) {
                Some(content) => Ok(content.clone()),
                None => bail!("File not found: {:?}", path),
            }
        }

        fn write(&self, path: &Path, contents: &str) -> Result<()> {
            self.write_count.fetch_add(1, Ordering::SeqCst);
            let mut files = self.files.lock().unwrap();
            let path_str = path.to_str().ok_or_else(|| anyhow!("Invalid path"))?;
            files.insert(path_str.to_string(), contents.to_string());
            Ok(())
        }

        fn exists(&self, path: &Path) -> bool {
            let files = self.files.lock().unwrap();
            let path_str = match path.to_str() {
                Some(s) => s,
                None => return false,
            };
            files.contains_key(path_str)
        }

        fn remove_file(&self, path: &Path) -> Result<()> {
            let mut files = self.files.lock().unwrap();
            let path_str = path.to_str().ok_or_else(|| anyhow!("Invalid path"))?;
            files.remove(path_str);
            Ok(())
        }

        fn create_dir_all(&self, _path: &Path) -> Result<()> {
            Ok(())
        }

        fn list_files(&self, dir: &Path) -> Result<Vec<PathBuf>> {
            let files = self.files.lock().unwrap();
            Ok(files
                .keys()
                .map(PathBuf::from)
                .filter(|path| path.parent() == Some(dir))
                .collect())
        }
    }
}
