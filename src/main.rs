use std::net::TcpStream;
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
use log::{info, warn};
use nostr_sdk::nips::nip04::{decrypt, encrypt};
use nostr_sdk::nips::nip47;
use nostr_sdk::prelude::{GetBalanceResponseResult, PayInvoiceResponseResult};
use nostr_sdk::{
    ClientMessage, EventBuilder, EventId, Filter, JsonUtil, Kind, PublicKey, RelayMessage,
    SecretKey, SubscriptionId, Tag, Url,
};
use tokio::io::{stdin, stdout};
use tokio::sync::Mutex;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message as WsMessage, WebSocket};

mod cln_nwc;
mod limits;
mod utils;

// Config Names
const WALLET_NSEC: &str = "nostr_connect_wallet_nsec";
const CLIENT_SECRET: &str = "nostr_connect_client_secret";
const RELAY: &str = "nostr_connect_relay";
const MAX_INVOICE_SAT: &str = "nostr_connect_max_invoice_sat";
const HOUR_LIMIT_SAT: &str = "nostr_connect_hour_limit_sat";
const DAY_LIMIT_SAT: &str = "nostr_connect_day_limit_sat";
const CONFIG_PATH: &str = "nostr_connect_config_path";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    info!("Starting cln-nostr-connect");
    let config_option =
        ConfigOption::new_str_no_default(CONFIG_PATH, "Nostr wallet connect config path");

    let day_limit_option =
        ConfigOption::new_i64_with_default(DAY_LIMIT_SAT, 35000000, "Max msats to spend per day");

    let hour_limt_option =
        ConfigOption::new_i64_with_default(HOUR_LIMIT_SAT, 10000000, "Max msats to spend per hour");

    let relay_option =
        ConfigOption::new_str_with_default(RELAY, "ws://localhost:8080", "Default nostr relay");

    let client_secret_option =
        ConfigOption::new_str_no_default(CLIENT_SECRET, "Nostr pubkey to accept requests from");

    let wallet_nsec_option =
        ConfigOption::new_str_no_default(WALLET_NSEC, "Nsec to publish success/failure events");

    let max_invoice_option = ConfigOption::new_i64_with_default(
        MAX_INVOICE_SAT,
        50000000,
        "Max size of an invoice (msat)",
    );

    let plugin = if let Some(plugin) = cln_plugin::Builder::new(stdin(), stdout())
        .option(wallet_nsec_option.clone())
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
            info!("Received \"shutdown\" notification from lightningd ... requesting cln_plugin shutdown");
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

    info!("Nostr Wallet Connect Config: {:?}", config_path);

    // Wallet key keys
    // If key is defined in CLN config that is used if not checks if key in plugin
    // config if no key found generate a new key and write to config
    let keys = match plugin.option(&wallet_nsec_option) {
        Ok(Some(wallet_nsec)) => utils::handle_keys(Some(wallet_nsec))?,
        _ => {
            let wallet_nsec = match utils::read_from_config("WALLET_NSEC", &config_path) {
                Ok(nsec) => nsec,
                Err(_) => None,
            };

            let keys = utils::handle_keys(wallet_nsec)?;

            if let Err(err) = utils::write_to_config(
                "WALLET_NSEC",
                &keys.secret_key()?.display_secret().to_string(),
                &config_path,
            ) {
                warn!("Could not write to config: {:?}", err);
            };

            keys
        }
    };

    let connect_client_keys = match plugin.option(&client_secret_option) {
        Ok(Some(client_secret)) => utils::handle_keys(Some(client_secret))?,
        _ => {
            let client_secret = match utils::read_from_config("CLIENT_SECRET", &config_path) {
                Ok(secret) => secret,
                Err(_) => None,
            };

            let keys = utils::handle_keys(client_secret)?;

            utils::write_to_config(
                "CLIENT_SECRET",
                &keys.secret_key()?.display_secret().to_string(),
                &config_path,
            )
            .ok();
            keys
        }
    };

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

    let client = utils::create_client(&keys, vec![relay.to_string()]).await?;

    let wallet_connect_uri: nip47::NostrWalletConnectURI = nip47::NostrWalletConnectURI::new(
        keys.public_key(),
        relay.clone(),
        connect_client_keys.secret_key()?.clone(),
        None,
    );

    log::info!("{}", wallet_connect_uri.to_string());
    let limits = Limits::new(
        Amount::from_sat(max_invoice_amount as u64),
        Amount::from_sat(hour_limit as u64),
        Amount::from_sat(day_limit as u64),
    );

    let cln_nwc = ClnNwc::new(limits, rpc_socket).await?;

    // Publish Wallet Connect info
    let info_event =
        EventBuilder::new(Kind::WalletConnectInfo, "pay_invoice", vec![]).to_event(&keys)?;

    // Publish info Event
    client.send_event(info_event).await?;

    loop {
        let mut events = match event_stream(connect_client_keys.public_key(), relay.clone()).await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("Event Stream Error: {:?}", err);
                continue;
            }
        };

        while let Some(msg) = events.next().await {
            match msg {
                RelayMessage::Auth { challenge } => {
                    if let Ok(auth_event) =
                        EventBuilder::auth(challenge, relay.clone()).to_event(&keys)
                    {
                        if let Err(err) = client.send_event(auth_event).await {
                            warn!("Could not broadcast event: {err}");
                        }
                    }
                }
                RelayMessage::Event {
                    subscription_id: _,
                    event,
                } => {
                    // Check event is valid
                    if event.verify().is_err() {
                        info!("Event {} is invalid", event.id.to_hex());
                        continue;
                    }

                    // info!("Got event: {:?}", serde_json::to_string_pretty(&event));

                    // Check event is from correct pubkey
                    if event.pubkey.ne(&connect_client_keys.public_key()) {
                        // TODO: Should respond with unauth
                        info!("Event from incorrect pubkey: {}", event.pubkey.to_string());
                        continue;
                    }

                    // Decrypt bolt11 from content (NIP04)
                    let content = match decrypt(
                        &keys.secret_key()?.clone(),
                        &connect_client_keys.public_key(),
                        &event.content,
                    ) {
                        Ok(content) => content,
                        Err(err) => {
                            info!("Could not decrypt: {err}");
                            continue;
                        }
                    };

                    // info!("Decrypted Content: {:?}", content);

                    let request = match nip47::Request::from_json(&content) {
                        Ok(req) => req,
                        Err(err) => {
                            warn!("Could not decode request {:?}", err);
                            continue;
                        }
                    };
                    // info!("request: {:?}", request);

                    let event_builder = match request.params {
                        nip47::RequestParams::PayInvoice(pay_invoice_param) => {
                            match cln_nwc.pay_invoice(pay_invoice_param.invoice).await {
                                Ok(preimage) => create_success_note(
                                    &event.pubkey,
                                    &event.id,
                                    keys.secret_key()?,
                                    preimage,
                                ),
                                Err(err) => create_failure_note(
                                    &event.pubkey,
                                    &event.id,
                                    keys.secret_key()?,
                                    &err.to_string(),
                                ),
                            }
                        }
                        nip47::RequestParams::GetBalance => match cln_nwc.get_balance().await {
                            Ok(amount) => {
                                let response = nip47::Response {
                                    result_type: nip47::Method::GetBalance,
                                    error: None,
                                    result: Some(nip47::ResponseResult::GetBalance(
                                        GetBalanceResponseResult {
                                            balance: amount.msat(),
                                        },
                                    )),
                                };

                                let encrypted_response = encrypt(
                                    &keys.secret_key()?.clone(),
                                    &event.pubkey,
                                    response.as_json(),
                                )?;

                                Ok(EventBuilder::new(
                                    Kind::WalletConnectResponse,
                                    encrypted_response,
                                    vec![
                                        Tag::public_key(event.pubkey.to_owned()),
                                        Tag::event(event.id.to_owned()),
                                    ],
                                ))
                            }
                            Err(_) => create_failure_note(
                                &event.pubkey,
                                &event.id,
                                keys.secret_key()?,
                                "CL RPC Error",
                            ),
                        },
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
                    };

                    // Build event response
                    let event_builder = match event_builder {
                        Ok(event_builder) => {
                            // Add spend value to daily and hourly limit tracking
                            info!("Payment success: {}", event.id);
                            event_builder
                        }
                        Err(err) => {
                            warn!("Failed: {}, RPC Error: {}", event.id, err);

                            match create_failure_note(
                                &event.pubkey,
                                &event.id,
                                &keys.secret_key()?.clone(),
                                "CLN RPC Error",
                            ) {
                                Ok(note) => note,
                                Err(err) => {
                                    warn!("Could not create failure note: {:?}", err);
                                    continue;
                                }
                            }
                        }
                    };

                    if let Ok(event) = event_builder.to_event(&keys) {
                        if let Err(err) = client.send_event(event).await {
                            warn!("Could not broadcast event: {}", err);
                        }
                    } else {
                        info!("Could not create event");
                    }
                }
                _ => continue,
            }
        }
        warn!("Event stream has ended");
    }

    //    Ok(())
}

/// Build NIP47 success event
fn create_success_note(
    connect_client_pubkey: &PublicKey,
    request_id: &EventId,
    connect_sk: &SecretKey,
    preimage: String,
) -> Result<EventBuilder> {
    let response = nip47::Response {
        result_type: nip47::Method::PayInvoice,
        error: None,
        result: Some(nip47::ResponseResult::PayInvoice(
            PayInvoiceResponseResult { preimage },
        )),
    };

    let encrypted_response = encrypt(connect_sk, connect_client_pubkey, response.as_json());
    Ok(EventBuilder::new(
        Kind::WalletConnectResponse,
        encrypted_response?,
        [
            Tag::public_key(connect_client_pubkey.to_owned()),
            Tag::event(request_id.to_owned()),
        ],
    ))
}

/// Build NIP47 failure event
fn create_failure_note(
    connect_client_pubkey: &PublicKey,
    request_id: &EventId,
    connect_sk: &SecretKey,
    reason: &str,
) -> Result<EventBuilder> {
    let response = nip47::Response {
        result_type: nip47::Method::PayInvoice,
        error: Some(nip47::NIP47Error {
            code: nip47::ErrorCode::Internal,
            message: reason.to_string(),
        }),
        result: None,
    };

    let encrypted_response = encrypt(connect_sk, connect_client_pubkey, response.as_json())?;
    Ok(EventBuilder::new(
        Kind::WalletConnectResponse,
        encrypted_response,
        [
            Tag::public_key(connect_client_pubkey.to_owned()),
            Tag::event(request_id.to_owned()),
        ],
    ))
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
            info!("Attempted connection to {} failed", url);
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
                        info!("WebSocket disconnected: {}", err);
                        info!("Attempting to reconnect ...");
                        match connect_relay(relay.clone(), connect_client_pubkey).await {
                            Ok(new_socket) => socket = Arc::new(Mutex::new(new_socket)),
                            Err(err) => {
                                info!("{}", err);
                                return None;
                            }
                        }

                        continue;
                    }
                };

                let msg_text = match msg.to_text() {
                    Ok(msg_test) => msg_test,
                    Err(_) => {
                        info!("Failed to convert message to text");
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
                    info!("Got unexpected message: {:?}", msg);
                }
            }
        },
    )
    .boxed())
}
