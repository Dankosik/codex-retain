use std::{fs::File, io::Read, path::Path};

use serde::Deserialize;

use crate::{cli::Format, error::AppError};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    format: Option<Format>,
}

pub(crate) fn resolve_format(
    flag_or_environment: Option<Format>,
    config_path: Option<&Path>,
) -> Result<Format, AppError> {
    let config = config_path.map(load).transpose()?.unwrap_or_default();
    Ok(flag_or_environment.or(config.format).unwrap_or_default())
}

fn load(path: &Path) -> Result<Config, AppError> {
    let read_error = |source| AppError::ConfigRead {
        path: path.to_owned(),
        source,
    };
    let file = File::open(path).map_err(read_error)?;
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(read_error)?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(AppError::ConfigTooLarge {
            path: path.to_owned(),
            limit: MAX_CONFIG_BYTES,
        });
    }
    toml::from_str(&text).map_err(|error: toml::de::Error| AppError::ConfigParse {
        path: path.to_owned(),
        // Avoid echoing the config source, which may later contain secrets.
        message: error.message().to_owned(),
    })
}
