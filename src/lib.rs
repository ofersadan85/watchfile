//! # Watchfile
//!
//! A simple trait that will watch a (serializable) file for changes and update a data struct.
//!
//! Uses `tokio` for async file reading and `tokio::sync::watch` for notifying about changes.
//!
//! ## File types / Features
//!
//! Currently supported:
//!
//! - JSON (`features = ["json"]` using `serde_json`)
//! - TOML (`features = ["toml"]` using `toml`)
//! - YAML (`features = ["yaml"]` using `serde_yaml`)
//!
//! All of them are optional features, all enabled by default. To minimize dependencies, you can disable them by using `default-features = false` in your `Cargo.toml`, and then enabling only the ones you need (for example, only JSON):
//!
//! ```toml
//! [dependencies]
//! watchfile = { version = "0.1", default-features = false, features = ["json"] }
//! ```
//!
//! Usage:
//!
//! ```rust
//! use std::time::Duration;
//! use watchfile::WatchFile;
//! use std::ops::Deref;
//!
//! #[derive(serde::Deserialize, Default, PartialEq, Debug)]
//! struct Config {
//!   data: String,
//! }
//!
//! impl WatchFile for Config {}
//!
//! #[tokio::main]
//! async fn main() {
//!   let rx = Config::watch_file("config.json", Duration::from_secs(1));
//!   assert_eq!(rx.borrow().deref(), &Config::default());
//! }
//! ```

use std::{path::Path, time::SystemTime};
use tokio::{
    fs::File,
    io::AsyncReadExt,
    sync::watch,
    time::{sleep, Duration},
};
use tracing::{error, info, instrument, warn};

/// Enum representing the supported file types, based on the enabled features.
#[derive(Debug)]
pub enum FileType<P>
where
    P: AsRef<Path> + std::fmt::Debug,
{
    #[cfg(feature = "json")]
    /// JSON file type, only available with the `json` feature.
    Json(P),
    #[cfg(feature = "toml")]
    /// TOML file type, only available with the `toml` feature.
    Toml(P),
    /// YAML file type, only available with the `yaml` feature.
    #[cfg(feature = "yaml")]
    Yaml(P),
}

impl<P> AsRef<Path> for FileType<P>
where
    P: AsRef<Path> + std::fmt::Debug,
{
    fn as_ref(&self) -> &Path {
        match self {
            #[cfg(feature = "json")]
            Self::Json(ref path) => path.as_ref(),
            #[cfg(feature = "toml")]
            Self::Toml(ref path) => path.as_ref(),
            #[cfg(feature = "yaml")]
            Self::Yaml(ref path) => path.as_ref(),
        }
    }
}

/// Trait for watching a file and updating a struct.
pub trait WatchFile
where
    Self: Sized + Default + PartialEq + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    /// Hook to run after deserialization, useful for post-processing.
    ///
    /// Note: This method is called after deserialization, so it is not called on initialization.
    ///
    /// This method is called whenever the file is reloaded and re-serialized, so it's best to avoid
    /// any expensive operations or side effects here. However, for files that don't change often,
    /// the extra processing is negligible, since this will only run when a change is detected.
    ///
    /// If using this method along with [`Self::save`], note that these changes will be written to the file.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use watchfile::WatchFile;
    /// use std::collections::HashMap;
    /// use std::time::Duration;
    ///
    /// #[derive(serde::Deserialize, Default, PartialEq, Debug)]
    /// struct Config {
    ///   data: HashMap<String, String>,
    /// }
    ///
    /// impl WatchFile for Config {
    ///   fn post_serialize(&mut self) {
    ///     self.data.insert("key".to_string(), "value".to_string());
    ///   }
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///   let config = Config::watch_file("config.json", Duration::from_millis(100));
    ///   assert_eq!(config.borrow().data.len(), 0, "Post processing doesn't run on initialization");
    ///   tokio::fs::write("config.json", "{\"data\": {}}").await.unwrap();
    ///   tokio::time::sleep(Duration::from_millis(200)).await;
    ///   assert_eq!(config.borrow().data.len(), 1, "Post processing runs on reload");
    ///   tokio::fs::remove_file("config.json").await.unwrap();
    ///   tokio::time::sleep(Duration::from_millis(200)).await;
    ///   assert_eq!(config.borrow().data.len(), 1, "No update on error (file not found)");
    /// }
    /// ```
    fn post_serialize(&mut self) {}

    /// Deserializes the buffer based on the file type and enabled features.
    fn serialize<P>(file_type: &FileType<P>, buf: &str) -> Result<Self, Box<dyn std::error::Error>>
    where
        P: AsRef<Path> + std::fmt::Debug + Send + Sync + 'static,
    {
        let serialized = match file_type {
            #[cfg(feature = "json")]
            FileType::Json(_) => serde_json::from_str::<Self>(buf).map_err(Into::into),
            #[cfg(feature = "toml")]
            FileType::Toml(_) => toml::from_str::<Self>(buf).map_err(Into::into),
            #[cfg(feature = "yaml")]
            FileType::Yaml(_) => serde_yaml::from_str::<Self>(buf).map_err(Into::into),
        };
        match serialized {
            Ok(mut serialized) => {
                eprintln!("Running post_serialize");
                serialized.post_serialize();
                Ok(serialized)
            }
            Err(err) => Err(err),
        }
    }

    /// Updates the buffer if the file modified time is newer than the last check.
    fn load_if_changed<P>(
        file_type: &FileType<P>,
        last_modified: &mut SystemTime,
        buf: &mut String,
    ) -> impl std::future::Future<Output = Result<bool, tokio::io::Error>> + Send + Sync
    where
        P: AsRef<Path> + std::fmt::Debug + Send + Sync + 'static,
    {
        async move {
            let mut file = File::open(file_type).await?;
            let modified = file.metadata().await?.modified()?;
            if modified > *last_modified {
                *last_modified = modified;
                buf.clear();
                file.read_to_string(buf).await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    /// Continuously checks the file for changes and sends updates on a [tokio::sync::watch] channel
    ///
    /// Only updates when all of the conditions are true:
    /// 1. The file has changed since the last check (intervals are determined by the duration).
    /// 2. The file is successfully read and serialized.
    /// 3. The serialized data is different (`!=`) from the previous data.
    #[instrument(skip(duration, sender))]
    fn reload<P>(
        file_type: FileType<P>,
        duration: Duration,
        sender: watch::Sender<Self>,
    ) -> impl std::future::Future<Output = ()> + Send + Sync + 'static
    where
        P: AsRef<Path> + std::fmt::Debug + Send + Sync + 'static,
    {
        async {
            let mut buf = String::new();
            let mut previous_error = String::new();
            let mut error = String::new();
            let mut last_modified = SystemTime::UNIX_EPOCH;
            loop {
                let changed = Self::load_if_changed(&file_type, &mut last_modified, &mut buf)
                    .await
                    .unwrap_or_else(|e| {
                        error = format!("Failed to read config file: {e}");
                        false
                    });
                if changed {
                    let serialized = Self::serialize(&file_type, &buf);
                    if let Ok(serialized) = serialized {
                        if serialized != *sender.borrow() {
                            info!("Change detected, reloading {file_type:?}");
                            let _ = sender.send_replace(serialized);
                        }
                    } else if let Err(err) = serialized {
                        error = format!("Failed to parse config file: {err}");
                    }
                }
                if !error.is_empty() && error != previous_error {
                    warn!("{error}");
                    previous_error = std::mem::take(&mut error);
                }
                sleep(duration).await;
            }
        }
    }

    /// Spawn a task that watches a file for changes and sends updates on a channel.
    ///
    /// Same as [`Self::watch_file_channel`] but drops the "extra" sender and returns only the receiver.
    ///
    /// The sender still exists in the background task, but it is not returned to the caller, so direct updates are not possible.
    ///
    /// ## Panics
    ///
    /// Panics if the file type is not supported by the crate with the enabled features.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use watchfile::WatchFile;
    /// use std::ops::Deref;
    ///
    /// #[derive(serde::Deserialize, Default, PartialEq, Debug)]
    /// struct Config {
    ///   data: String,
    /// }
    ///
    /// impl WatchFile for Config {}
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///   let rx = Config::watch_file("config.json", Duration::from_secs(1));
    ///   assert_eq!(rx.borrow().deref(), &Config::default());
    /// }
    /// ```
    fn watch_file<P>(path: P, duration: Duration) -> watch::Receiver<Self>
    where
        P: AsRef<Path> + std::fmt::Debug + Send + Sync + 'static,
    {
        Self::watch_file_channel(path, duration).1
    }

    /// Spawn a task that watches a file for changes and sends updates on a channel.
    ///
    /// ## Panics
    ///
    /// Panics if the file type is not supported by the crate with the enabled features.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use watchfile::WatchFile;
    /// use std::ops::Deref;
    ///
    /// #[derive(serde::Deserialize, Default, PartialEq, Debug)]
    /// struct Config {
    ///   data: String,
    /// }
    ///
    /// impl WatchFile for Config {}
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///   let (tx, mut rx) = Config::watch_file_channel("config.json", Duration::from_secs(1));
    ///   assert_eq!(tx.sender_count(), 2);
    ///   assert_eq!(tx.receiver_count(), 1);
    ///   assert_eq!(rx.borrow().deref(), &Config::default());
    /// }
    /// ```
    fn watch_file_channel<P>(
        path: P,
        duration: Duration,
    ) -> (watch::Sender<Self>, watch::Receiver<Self>)
    where
        P: AsRef<Path> + std::fmt::Debug + Send + Sync + 'static,
    {
        let (tx, rx) = watch::channel(Self::default());
        let ext = path
            .as_ref()
            .extension()
            .unwrap_or_default()
            .to_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let file_type = match ext.as_str() {
            #[cfg(feature = "json")]
            "json" => FileType::Json(path),
            #[cfg(feature = "toml")]
            "toml" => FileType::Toml(path),
            #[cfg(feature = "yaml")]
            "yaml" | "yml" => FileType::Yaml(path),
            _ => panic!("Unsupported file type {ext}, see optional features for supported types",),
        };
        tokio::spawn(Self::reload(file_type, duration, tx.clone()));
        (tx, rx)
    }

    /// Save the current state of the struct to a file, with the given path.
    ///
    /// Serializes the struct based on the file extension, make sure to enable the corresponding feature.
    ///
    /// Replaces the previous contents of the file if it exists, otherwise creates a new file.
    ///
    /// Can be used safely along with [`Self::watch_file`] and [`Self::watch_file_channel`],
    /// as those tasks do not update malformed files (e.g. in the middle of a write operation).
    ///
    /// ## Errors
    ///
    /// Returns an error in one of the following:
    /// 1. The file type is not supported by the crate with the enabled features.
    /// 2. The serialization process fails (e.g. due to a missing field, mismatched types, etc.).
    /// 3. The write operation fails (e.g. due to a permission error, disk full, etc.).
    ///
    /// ## Example
    ///
    /// ```rust
    /// use watchfile::WatchFile;
    /// use std::collections::HashMap;
    /// use std::time::Duration;
    ///
    /// #[derive(serde::Deserialize, serde::Serialize, Default, PartialEq, Debug, Clone)]
    /// struct Config {
    ///    data: HashMap<String, String>,
    /// }
    ///
    /// impl WatchFile for Config {}
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///   let config = Config::watch_file("config.json", Duration::from_millis(100));
    ///   let mut updated_config = config.borrow().clone();
    ///   updated_config.data.insert("key".to_string(), "value".to_string());
    ///   updated_config.save("config.json").await.unwrap();
    ///   tokio::time::sleep(Duration::from_millis(200)).await;
    ///   
    ///   assert_eq!(config.borrow().data.len(), 1);
    ///   assert_eq!(config.borrow().data.get("key"), Some(&"value".to_string()));
    ///
    ///   tokio::fs::remove_file("config.json").await.unwrap();
    ///   tokio::time::sleep(Duration::from_millis(200)).await;
    ///   assert!(tokio::fs::metadata("config.json").await.is_err(), "File deleted");
    ///   assert_eq!(config.borrow().data.len(), 1, "No update on error (file not found)");
    /// }
    /// ```
    fn save<P>(
        &self,
        path: P,
    ) -> impl std::future::Future<Output = Result<(), ()>> + Send + Sync + '_
    where
        P: AsRef<Path> + std::fmt::Debug + Send + Sync + 'static,
        Self: serde::Serialize,
    {
        async {
            let ext = path
                .as_ref()
                .extension()
                .unwrap_or_default()
                .to_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let serialized = match ext.as_str() {
                #[cfg(feature = "json")]
                "json" => serde_json::to_string_pretty(self).map_err(Into::into),
                #[cfg(feature = "toml")]
                "toml" => toml::to_string_pretty(self).map_err(Into::into),
                #[cfg(feature = "yaml")]
                "yaml" | "yml" => serde_yaml::to_string(self).map_err(Into::into),
                _ => {
                    error!(
                        "Unsupported file type {ext}, see optional features for supported types"
                    );
                    return Err(());
                }
            }
            .map_err(|e: Box<dyn std::error::Error>| {
                warn!("Failed to serialize: {e}");
            });
            if let Ok(serialized) = serialized {
                tokio::fs::write(path, serialized).await.map_err(|e| {
                    warn!("Failed to write file: {e}");
                })?;
            }
            Ok(())
        }
    }
}
