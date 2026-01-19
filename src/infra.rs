use anyhow::{Result, anyhow};
use std::{fs, path::Path};

pub trait FileSystem: Send + Sync {
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn write(&self, path: &Path, contents: &str) -> Result<()>;
    fn exists(&self, path: &Path) -> bool;
    fn remove_file(&self, path: &Path) -> Result<()>;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
}

pub struct RealFileSystem;

impl FileSystem for RealFileSystem {
    fn read_to_string(&self, path: &Path) -> Result<String> {
        Ok(std::fs::read_to_string(path)?)
    }

    fn write(&self, path: &Path, contents: &str) -> Result<()> {
        Ok(std::fs::write(path, contents)?)
    }

    fn exists(&self, path: &Path) -> bool {
        if path.as_os_str().is_empty() {
            return false;
        }
        if matches!(fs::exists(path), Ok(true)) {
            if let Ok(metadata) = fs::metadata(path) {
                return metadata.is_file();
            }
            return false;
        }
        false
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        if path.exists() {
            match std::fs::remove_file(path) {
                Ok(_) => Ok(()),
                Err(e) => {
                    error!("Failed to remove file: {:#}", e);
                    Err(anyhow!("Failed to remove file: {:#}", e))
                }
            }
        } else {
            Ok(())
        }
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        Ok(std::fs::create_dir_all(path)?)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use anyhow::bail;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    pub struct MockFileSystem {
        files: Arc<Mutex<HashMap<String, String>>>,
    }

    impl MockFileSystem {
        pub fn new() -> Self {
            Self {
                files: Arc::new(Mutex::new(HashMap::new())),
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

        pub fn file_exists_in_memory(&self, path: impl AsRef<str>) -> bool {
            self.files.lock().unwrap().contains_key(path.as_ref())
        }
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
    }
}
