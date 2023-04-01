//! Nostr utils
// Modified from https://github.com/0xtrr/nostr-tool
// Copyright (c) 2022 0xtr
// Distributed under the MIT software license

use anyhow::Result;
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
    let relays = relays.iter().map(|url| (url, None)).collect();
    client.add_relays(relays).await?;
    client.connect().await;
    Ok(client)
}
