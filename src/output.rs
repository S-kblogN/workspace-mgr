use serde::Serialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Human,
    Json,
}

pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| Error::message(format!("failed to serialize output: {error}")))?;
    println!("{text}");
    Ok(())
}

pub fn print_human<T: Serialize>(value: &T) -> Result<()> {
    let text = serde_yaml::to_string(value)
        .map_err(|error| Error::message(format!("failed to encode output: {error}")))?;
    print!("{text}");
    Ok(())
}
