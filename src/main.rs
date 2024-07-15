use std::net::TcpStream;
use std::ops::Deref;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use cln_nwc::ClnNwc;
use cln_plugin::options::ConfigOption;
use cln_plugin::Plugin;
use cln_rpc::primitives::Amount;
use dirs::config_dir;
use futures::{Stream, StreamExt};
use limits::Limits;
use nostr_sdk::nips::nip47;
use nostr_sdk::{
    ClientMessage, Filter, JsonUtil, Keys, Kind, PublicKey, RelayMessage, SubscriptionId, Url,
};
use tokio::io::{stdin, stdout};
use tokio::sync::Mutex;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message as WsMessage, WebSocket};

mod cln_nwc;
mod limits;

// Config Names
const NWC_SECRET: &str = "nostr_connect_wallet_secret";
const CLIENT_SECRET: &str = "nostr_connect_client_secret";
const RELAY: &str = "nostr_connect_relay";
const MAX_INVOICE_SAT: &str = "nostr_connect_max_invoice_sat";
const HOUR_LIMIT_SAT: &str = "nostr_connect_hour_limit_sat";
const DAY_LIMIT_SAT: &str = "nostr_connect_day_limit_sat";
const CONFIG_PATH: &str = "nostr_connect_config_path";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing::info!("Starting cln-nostr-connect");
    println!("Starting cln");
    let config_option =
        ConfigOption::new_str_no_default(CONFIG_PATH, "Nostr wallet connect config path");

    let day_limit_option =
        ConfigOption::new_i64_with_default(DAY_LIMIT_SAT, 35000000, "Max msats to spend per day");

    let hour_limt_option =
        ConfigOption::new_i64_with_default(HOUR_LIMIT_SAT, 10000000, "Max msats to spend per hour");

    let relay_option =
        ConfigOption::new_str_with_default(RELAY, "ws://localhost:8080", "Default nostr relay");

    let client_secret_option =
        ConfigOption::new_str_no_default(CLIENT_SECRET, "Secret the nostr client will use to send and receive messages. Can be nsec of 32 byte hex encoded");

    let nwc_secret_option =
        ConfigOption::new_str_no_default(NWC_SECRET, "Secret that the plugin will use to send and receive messages. Can be nsec or 32 byte hex encoded");

    let max_invoice_option = ConfigOption::new_i64_with_default(
        MAX_INVOICE_SAT,
        50000000,
        "Max size of an invoice (msat)",
    );

    let plugin = if let Some(plugin) = cln_plugin::Builder::new(stdin(), stdout())
        .option(nwc_secret_option.clone())
        .option(client_secret_option.clone())
        // TODO: Would be better to be a list
        .option(relay_option.clone())
        .option(max_invoice_option.clone())
        .option(hour_limt_option.clone())
        .option(day_limit_option.clone())
        .option(config_option.clone())
        .subscribe("shutdown",
            // Handle CLN `shutdown` if it is sent 
            |plugin: Plugin<()>, _: serde_json::Value| async move {
            tracing::info!("Received \"shutdown\" notification from lightningd ... requesting cln_plugin shutdown");
            plugin.shutdown().ok();
            plugin.join().await
        })
        .dynamic()
        .start(())
        .await?
    {
        plugin
    } else {
        return Ok(());
    };

    let rpc_socket: PathBuf = plugin.configuration().rpc_file.parse()?;

    let config_path = match plugin.option(&config_option) {
        Ok(Some(config_path)) => PathBuf::from_str(&config_path)?,
        _ => config_dir()
            .unwrap()
            .join("cln-nostr-wallet-connect")
            .join("config"),
    };

    tracing::info!("Nostr Wallet Connect Config: {:?}", config_path);
    let nwc_keys = plugin
        .option(&nwc_secret_option)
        .expect("NWC Secret must be defined")
        .expect("NWC Secret must be defined");

    let nwc_keys = Keys::from_str(&nwc_keys)?;

    let client_keys = plugin
        .option(&client_secret_option)
        .expect("Client Secret must be defined")
        .expect("Client Secret must be defined");

    let client_keys = Keys::from_str(&client_keys)?;

    let nostr_relay = plugin
        .option(&relay_option)
        .expect("Option is defined")
        .as_str()
        .to_owned();

    let max_invoice_amount = plugin
        .option(&max_invoice_option)
        .expect("Option is defined");

    let hour_limit = plugin.option(&hour_limt_option).expect("Option is defined");

    let day_limit = plugin.option(&day_limit_option).expect("Option is defined");

    // Relay to listen for events and publish events to
    let relay = Url::from_str(&nostr_relay)?;

    //let client = utils::create_client(&keys, vec![relay.to_string()]).await?;

    let limits = Limits::new(
        Amount::from_sat(max_invoice_amount as u64),
        Amount::from_sat(hour_limit as u64),
        Amount::from_sat(day_limit as u64),
    );

    let cln_nwc = ClnNwc::new(
        client_keys.clone(),
        nwc_keys.clone(),
        relay.clone(),
        limits,
        rpc_socket,
    )
    .await?;
    /*
        // Publish Wallet Connect info
        let info_event =
            EventBuilder::new(Kind::WalletConnectInfo, "pay_invoice", vec![]).to_event(&nkeys)?;
    */
    // Publish info Event
    //client.send_event(info_event).await?;
    let relay_clone = relay.clone();
    loop {
        let mut events =
            match event_stream(client_keys.clone().public_key(), relay_clone.clone()).await {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!("Event Stream Error: {:?}", err);
                    continue;
                }
            };

        while let Some(msg) = events.next().await {
            match msg {
                RelayMessage::Auth { challenge } => {
                    cln_nwc.auth(&challenge, relay_clone.clone()).await.ok();
                }
                RelayMessage::Event {
                    subscription_id: _,
                    event,
                } => {
                    let request = match cln_nwc.verify_event(&event).await {
                        Ok(request) => {
                            tracing::info!("Received a valid nostr event: {}", event.id);
                            request
                        }
                        Err(_) => {
                            tracing::info!("Received an invalid nostr event: {}", event.id);
                            continue;
                        }
                    };

                    match request.params {
                        nip47::RequestParams::PayInvoice(pay_invoice_param) => {
                            match cln_nwc
                                .pay_invoice(event.id, pay_invoice_param.invoice)
                                .await
                            {
                                Ok(_) => {
                                    tracing::info!("Paid invoice for event: {}", event.id);
                                }
                                Err(_) => {
                                    tracing::error!("Failed to pay invoice for event {}", event.id);
                                }
                            }
                        }
                        nip47::RequestParams::GetBalance => {
                            match cln_nwc.get_balance(event.deref()).await {
                                Ok(_) => {
                                    tracing::info!("Got balance for: {}", event.id);
                                }
                                Err(_) => {
                                    tracing::info!("Failed to get balance for event {}:", event.id);
                                }
                            }
                        }
                        nip47::RequestParams::MakeInvoice(_) => {
                            bail!("NIP47 Make invoice is not implemented")
                        }
                        nip47::RequestParams::LookupInvoice(_) => {
                            bail!("NIP47 Lookup invoice is not implemented")
                        }
                        nip47::RequestParams::MultiPayInvoice(_) => {
                            bail!("NIP47 Multi pay is not implemented")
                        }
                        nip47::RequestParams::PayKeysend(_) => {
                            bail!("NIP47 Pay keysend is not implemented")
                        }
                        nip47::RequestParams::MultiPayKeysend(_) => {
                            bail!("NIP47 Multi Pay Keysend is not implemented")
                        }
                        nip47::RequestParams::ListTransactions(_) => {
                            bail!("NIP47 List transactions is not implmneted")
                        }
                        nip47::RequestParams::GetInfo => {
                            bail!("NIP47 Get info is not implemented")
                        }
                    }
                }
                _ => continue,
            }
        }
        tracing::warn!("Event stream has ended");
    }

    //    Ok(())
}

async fn connect_relay(
    url: Url,
    connect_client_pubkey: PublicKey,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let max_retries = 500;
    let mut delay = Duration::from_secs(10);

    for attempt in 0..max_retries {
        if let Ok((mut socket, _response)) = connect(url.to_string()) {
            // Subscription filter
            let subscribe_to_requests = ClientMessage::req(
                SubscriptionId::generate(),
                vec![Filter::new()
                    .author(connect_client_pubkey)
                    .kind(Kind::WalletConnectRequest)],
            );

            socket.send(WsMessage::Text(subscribe_to_requests.as_json()))?;

            return Ok(socket);
        } else {
            tracing::info!("Attempted connection to {} failed", url);
        }

        if attempt == 99 {
            delay = Duration::from_secs(30);
        }

        tokio::time::sleep(delay).await;
    }

    bail!("Relay connection attempts exceeded");
}

async fn event_stream(
    connect_client_pubkey: PublicKey,
    relay: Url,
) -> Result<impl Stream<Item = RelayMessage>> {
    let socket = connect_relay(relay.clone(), connect_client_pubkey).await?;

    let socket = Arc::new(Mutex::new(socket));

    Ok(futures::stream::unfold(
        (socket, relay, connect_client_pubkey),
        |(mut socket, relay, connect_client_pubkey)| async move {
            loop {
                let msg = match socket.clone().lock().await.read() {
                    Ok(msg) => msg,
                    Err(err) => {
                        // Handle disconnection
                        tracing::info!("WebSocket disconnected: {}", err);
                        tracing::info!("Attempting to reconnect ...");
                        match connect_relay(relay.clone(), connect_client_pubkey).await {
                            Ok(new_socket) => socket = Arc::new(Mutex::new(new_socket)),
                            Err(err) => {
                                tracing::info!("{}", err);
                                return None;
                            }
                        }

                        continue;
                    }
                };

                let msg_text = match msg.to_text() {
                    Ok(msg_test) => msg_test,
                    Err(_) => {
                        tracing::info!("Failed to convert message to text");
                        continue;
                    }
                };

                if msg.is_ping() {
                    socket.lock().await.send(WsMessage::Pong(vec![])).ok();
                } else if let Ok(handled_message) = RelayMessage::from_json(msg_text) {
                    match &handled_message {
                        RelayMessage::Event { .. } | RelayMessage::Auth { .. } => {
                            break Some((
                                handled_message,
                                (socket, relay.clone(), connect_client_pubkey),
                            ));
                        }
                        _ => continue,
                    }
                } else {
                    tracing::info!("Got unexpected message: {:?}", msg);
                }
            }
        },
    )
    .boxed())
}
