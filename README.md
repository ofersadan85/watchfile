# Watchfile

A simple trait that will watch a (serializable) file for changes and update a data struct.

Uses `tokio` for async file reading and `tokio::sync::watch` for notifying about changes.

## File types / Features

Currently supported:

- JSON (`features = ["json"]` using `serde_json`)
- TOML (`features = ["toml"]` using `toml`)
- YAML (`features = ["yaml"]` using `serde_yaml`)

All of the are optional features, all enabled by default. To minimize dependencies, you can disable them by using `default-features = false` in your `Cargo.toml`, and then enabling only the ones you need (for example, only JSON):

```toml
[dependencies]
watchfile = { version = "0.1", default-features = false, features = ["json"] }
```

Usage:

```rust
use std::time::Duration;
use watchfile::WatchFile;
use std::ops::Deref;

#[derive(serde::Deserialize, Default, PartialEq, Debug)]
struct Config {
  data: String,
}

impl WatchFile for Config {}

#[tokio::main]
async fn main() {
  let rx = Config::watch_file("config.json", Duration::from_secs(1));
  assert_eq!(rx.borrow().deref(), &Config::default());
}
```
