use std::sync::{Arc, Mutex};

use crate::logger::Logger;

// The renderer treats this as an opaque string blob (its whole serialized
// settings state) — we don't parse it, just persist and echo it back,
// matching main/src/host-files/ConfigStore.ts's wire contract. The on-disk
// path doesn't need to match the real app's, since the renderer only ever
// talks to it through GET /config and the save-config/config-changed events.
pub struct ConfigStore {
    path: Mutex<std::path::PathBuf>,
}

pub fn config_path() -> std::path::PathBuf {
    crate::xdg::data_dir().join("apt-kwin-overlay/config.json")
}

impl ConfigStore {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self {
            path: Mutex::new(path),
        }
    }

    pub fn load(&self) -> Option<String> {
        std::fs::read_to_string(&*self.path.lock().unwrap()).ok()
    }

    pub fn save(&self, contents: &str, is_temporary: bool, logger: &Arc<Logger>) {
        let mut path = self.path.lock().unwrap();
        if is_temporary && path.extension().and_then(|e| e.to_str()) != Some("tmp") {
            let mut with_tmp = path.clone().into_os_string();
            with_tmp.push(".tmp");
            *path = with_tmp.into();
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::write(&*path, contents) {
            logger.error(&format!("[config_store] failed to save config: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_save_switches_to_tmp_file_for_good() {
        let dir = std::env::temp_dir().join(format!("apt-config-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let store = ConfigStore::new(path.clone());
        let logger = Arc::new(Logger::new(Arc::new(crate::server::EventBus::new().0)));

        store.save("real", false, &logger);
        assert_eq!(store.load().as_deref(), Some("real"));

        // ConfigStore.ts: once a temporary save happened, every later save
        // (temporary or not) goes to config.json.tmp; config.json is left alone.
        store.save("tmp1", true, &logger);
        store.save("tmp2", false, &logger);
        assert_eq!(store.load().as_deref(), Some("tmp2"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "real");
        assert_eq!(std::fs::read_to_string(dir.join("config.json.tmp")).unwrap(), "tmp2");
        assert!(!dir.join("config.json.tmp.tmp").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
