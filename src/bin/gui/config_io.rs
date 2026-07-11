use onedrive_mount::{config::Config, paths::config_file};

pub enum LoadResult {
    Ok(Config),
    ParseError(String),
    Missing,
}

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
