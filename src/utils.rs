//! Nostr utils
// Modified from https://github.com/0xtrr/nostr-tool
// Copyright (c) 2022 0xtr
// Distributed under the MIT software license
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Error, Result};
use log::info;
use nostr_sdk::key::FromSkStr;
use nostr_sdk::{Client, Keys};

pub fn handle_keys(private_key: Option<String>) -> Result<Keys> {
    // Parse and validate private key
    let keys = match private_key {
        Some(pk) => {
            // create a new identity using the provided private key
            Keys::from_sk_str(pk.as_str())?
        }
        None => {
            // create a new identity with a new keypair
            info!("No private key provided, creating new identity");
            Keys::generate()
        }
    };

    Ok(keys)
}

// Creates the websocket client that is used for communicating with relays
pub async fn create_client(keys: &Keys, relays: Vec<String>) -> Result<Client> {
    let client = Client::new(keys);
    let relays = relays.iter().map(|url| (url.clone(), None)).collect();
    client.add_relays(relays).await?;
    client.connect().await;
    Ok(client)
}

pub fn write_to_config(key: &str, value: &str, config_path: &PathBuf) -> Result<()> {
    // Create the directory if it doesn't exist
    let dir_path = config_path
        .parent()
        .ok_or(Error::msg("No parent".to_string()))?;

    if !dir_path.exists() {
        fs::create_dir_all(dir_path)?;
    }

    // Create the file if it doesn't exist
    if !config_path.exists() {
        File::create(config_path)?;
    }

    let file = File::open(config_path)?;
    let reader = BufReader::new(file);

    let lines = reader.lines();

    // Create a new vector to hold the updated lines
    let mut updated_lines = Vec::new();

    let mut key_found = false;

    // Loop through each line in the config file
    for line in lines {
        let line = line?;

        // Check if the line contains the key we're looking for
        if line.starts_with(&format!("{}=", key)) {
            // Update the value for the key
            updated_lines.push(format!("{}={}", key, value));
            key_found = true;
        } else {
            // Add the original line to the updated_lines vector
            updated_lines.push(line);
        }
    }

    // If the key was not found in the config file, add it to the end
    if !key_found {
        updated_lines.push(format!("{}={}", key, value));
    }

    // Write the updated lines to the config file
    let mut file = File::create(config_path)?;
    for line in updated_lines {
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }

    Ok(())
}

pub fn read_from_config(key: &str, config_path: &PathBuf) -> Result<Option<String>> {
    let file = File::open(config_path)?;
    let reader = BufReader::new(file);

    let lines = reader.lines();

    // Loop through each line in the config file
    for line in lines {
        let line = line?;

        // Check if the line contains the key we're looking for
        if line.starts_with(&format!("{}=", key)) {
            let parts: Vec<&str> = line.split('=').collect();
            if let Some(value) = parts.get(1) {
                return Ok(Some(value.to_string()));
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST: &str = "./test.txt";

    #[test]
    fn test_write_to_config() {
        let path = PathBuf::from(TEST);
        write_to_config("ONE", "Hello", &path).unwrap();
        write_to_config("TWO", "World", &path).unwrap();

        let one = read_from_config("ONE", &path).unwrap();
        assert_eq!(one.unwrap(), "Hello");

        let two = read_from_config("TWO", &path).unwrap();
        assert_eq!(two.unwrap(), "World");
    }
}
