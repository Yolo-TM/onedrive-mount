// Loads and saves config.toml — missing file is treated as an empty config, not an error

use onedrive_mount::{config::Config, paths::config_file};

pub enum LoadResult {
    Ok(Config),
    /// The file exists but could not be parsed — the raw error message is included.
    ParseError(String),
    /// The file doesn't exist yet — first launch.
    Missing,
}

/// Returns the full load result so callers can surface parse errors to the user.
pub fn load_result() -> LoadResult {
    let path = config_file();
    if !path.exists() {
        return LoadResult::Missing;
    }
    match Config::load(&path) {
        Ok(c) => LoadResult::Ok(c),
        Err(e) => LoadResult::ParseError(e.to_string()),
    }
}

pub fn save(config: &Config) -> Result<(), String> {
    let errors = config.validate();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let path = config_file();
    config.save(&path).map_err(|e| e.to_string())
}
