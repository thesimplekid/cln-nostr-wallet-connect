use std::net::TcpStream;
use std::ops::Add;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_plugin::Plugin;
use cln_rpc::model::{requests::PayRequest, Request, Response};
use cln_rpc::primitives::{Amount, Secret};
use dirs::config_dir;
use futures::{Stream, StreamExt};
use lightning_invoice::{Invoice, InvoiceDescription};
use log::{info, warn};
use nostr_sdk::secp256k1::{SecretKey, XOnlyPublicKey};
use nostr_sdk::{
    nips::{
        nip04::{decrypt, encrypt},
        nip47,
    },
    ClientMessage, EventBuilder, Filter, Kind, SubscriptionId,
};
use nostr_sdk::{EventId, RelayMessage, Tag, Url};
use tokio::io::{stdin, stdout};
use tokio::sync::Mutex;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message as WsMessage, WebSocket};

mod utils;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    info!("Starting cln-nostr-connect");
    let plugin = if let Some(plugin) = cln_plugin::Builder::new(stdin(), stdout())
        .option(ConfigOption::new(
            "nostr_connect_wallet_nsec",
            Value::OptString,
            "Nsec to publish success/failure events",
        ))
        .option(ConfigOption::new(
            "nostr_connect_client_secret",
            Value::OptString,
            "Nostr pubkey to accept requests from",
        ))
        // TODO: Would be better to be a list
        .option(ConfigOption::new(
            "nostr_connect_relay",
            Value::String("ws://localhost:8080".to_string()),
            "Default nostr relay",
        ))
        .option(ConfigOption::new(
            "nostr_connect_max_invoice",
            Value::Integer(50000000),
            "Max size of an invoice (msat)",
        ))
        .option(ConfigOption::new(
            "nostr_connect_hour_limit",
            Value::Integer(10000000),
            "Max msats to spend per hour"
        ))
        .option(ConfigOption::new(
            "nostr_connect_day_limit",
            Value::Integer(35000000),
            "Max msats to spend per day"
        ))
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
    let mut cln_client = cln_rpc::ClnRpc::new(&rpc_socket).await?;

    let config_path = config_dir()
        .unwrap()
        .join("cln-nostr-wallet-connect")
        .join("config");

    info!("Nostr Wallet Connect Config: {:?}", config_path);

    // Wallet key keys
    // If key is defuined in CLN config that is used if not checks if key in plugin config
    // if no key found generate a new key and write to config
    let keys = match plugin.option("nostr_connect_wallet_nsec") {
        Some(Value::String(wallet_nsec)) => utils::handle_keys(Some(wallet_nsec))?,
        _ => {
            let wallet_nsec = match utils::read_from_config("WALLET_NSEC", &config_path) {
                Ok(nsec) => nsec,
                Err(_) => None,
            };

            let keys = utils::handle_keys(wallet_nsec)?;

            utils::write_to_config(
                "WALLET_NSEC",
                &keys.secret_key()?.display_secret().to_string(),
                &config_path,
            )
            .ok();
            keys
        }
    };

    let connect_client_keys = match plugin.option("nostr_connect_client_secret") {
        Some(Value::String(client_secret)) => utils::handle_keys(Some(client_secret))?,
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
        .option("nostr_connect_relay")
        .expect("Option is defined")
        .as_str()
        .expect("Option is a string")
        .to_owned();

    let max_invoice_amount = plugin
        .option("nostr_connect_max_invoice")
        .expect("Option is defined")
        .as_i64()
        .expect("Option is a i64")
        .to_owned();

    let hour_limit = plugin
        .option("nostr_connect_hour_limit")
        .expect("Option is defined")
        .as_i64()
        .expect("Option is a i64")
        .to_owned();

    let day_limit = plugin
        .option("nostr_connect_day_limit")
        .expect("Option is defined")
        .as_i64()
        .expect("Option is a i64")
        .to_owned();

    // Relay to listen for events and publish events to
    let relay = Url::from_str(&nostr_relay)?;

    let client = utils::create_client(&keys, vec![relay.to_string()]).await?;

    let wallet_connect_uri: nip47::NostrWalletConnectURI = nip47::NostrWalletConnectURI::new(
        keys.public_key(),
        relay.clone(),
        Some(connect_client_keys.secret_key()?),
    )?;

    info!("{}", wallet_connect_uri.to_string());

    // Publish Wallet Connect info
    let info_event =
        EventBuilder::new(Kind::WalletConnectInfo, "pay_invoice", &[]).to_event(&keys)?;

    // Publish info Event
    client.send_event(info_event).await?;

    let mut limits = Limits::new(
        Amount::from_msat(hour_limit as u64),
        Amount::from_sat(day_limit as u64),
    );

    let mut events = event_stream(connect_client_keys.public_key(), relay.clone()).await?;
    while let Some(msg) = events.next().await {
        match msg {
            RelayMessage::Auth { challenge } => {
                if let Ok(auth_event) = EventBuilder::auth(challenge, relay.clone()).to_event(&keys)
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
                    &keys.secret_key()?,
                    &connect_client_keys.public_key(),
                    event.content,
                ) {
                    Ok(content) => content,
                    Err(err) => {
                        info!("Could not decrypt: {err}");
                        continue;
                    }
                };

                // info!("Decrypted Content: {:?}", content);

                let request = nip47::Request::from_json(&content)?;
                // info!("request: {:?}", request);

                let bolt11 = match request.params.invoice.parse::<Invoice>() {
                    Ok(invoice) => invoice,
                    Err(_err) => {
                        info!("Could not parse invoice");
                        continue;
                    }
                };

                if let Some(amount) = bolt11.amount_milli_satoshis() {
                    let amount = Amount::from_msat(amount);

                    // Check amount is < then config max
                    if amount.msat().gt(&(max_invoice_amount as u64)) {
                        bail!("Invoice too large: {amount:?} > {max_invoice_amount}")
                    }

                    // Check spend does not exceed daily or hourly limit
                    if limits.check_limit(amount).is_err() {
                        info!("Sending {} msat will exceed limit", amount.msat());
                        info!(
                            "Hour limit: {} msats, Hour spent: {} msats",
                            limits.hour_limit.msat(),
                            limits.hour_value.msat()
                        );

                        info!(
                            "Day limit: {} msats, Day Spent: {} msats",
                            limits.day_limit.msat(),
                            limits.day_value.msat()
                        );
                        continue;
                    }

                    // Currently there is some debate over whether the description hash should be known or not before paying.
                    // The change in CLN requiring the description was reverted but it is still deprecated
                    // With NIP47 as of now the description is unknown
                    // This may need to be reviewed in the future
                    // https://github.com/ElementsProject/lightning/pull/6092
                    // https://github.com/ElementsProject/lightning/releases/tag/v23.02.2
                    let _description = match bolt11.description() {
                        InvoiceDescription::Direct(des) => des.to_string(),
                        InvoiceDescription::Hash(hash) => hash.0.to_string(),
                    };

                    // Send payment
                    let cln_response = cln_client
                        .call(Request::Pay(PayRequest {
                            bolt11: bolt11.to_string(),
                            amount_msat: None,
                            label: None,
                            riskfactor: None,
                            maxfeepercent: None,
                            retry_for: None,
                            maxdelay: None,
                            exemptfee: None,
                            localinvreqid: None,
                            exclude: None,
                            maxfee: None,
                            description: None,
                        }))
                        .await;

                    // Build event response
                    let event_builder = match cln_response {
                        Ok(Response::Pay(res)) => {
                            // Add spend value to daily and hourly limit tracking
                            limits.add_spend(amount);

                            create_success_note(
                                &event.pubkey,
                                &event.id,
                                &keys.secret_key()?,
                                res.payment_preimage,
                            )?
                        }
                        Err(err) => {
                            info!("Payment failed: {}, RPC Error: {}", event.id, err);
                            create_failure_note(
                                &event.pubkey,
                                &event.id,
                                &keys.secret_key()?,
                                "CL RPC Error",
                            )?
                        }
                        Ok(res) => {
                            info!(
                                "Payment failed: {}: Unexpected CL response: {:?}",
                                event.id, res
                            );
                            create_failure_note(
                                &event.pubkey,
                                &event.id,
                                &keys.secret_key()?,
                                "CL Unexpected Response",
                            )?
                        }
                    };

                    if let Ok(event) = event_builder.to_event(&keys) {
                        if let Err(err) = client.send_event(event).await {
                            warn!("Could not broadcast event: {}", err);
                        }
                    }
                }
            }
            _ => continue,
        }
    }

    Ok(())
}

/// Build NIP47 success event
fn create_success_note(
    connect_client_pubkey: &XOnlyPublicKey,
    request_id: &EventId,
    connect_sk: &SecretKey,
    preimage: Secret,
) -> Result<EventBuilder> {
    let response = nip47::Response {
        result_type: nip47::Method::PayInvoice,
        error: None,
        result: Some(nip47::ResponseResult {
            preimage: hex::encode(preimage.to_vec()),
        }),
    };

    let encrypted_response = encrypt(connect_sk, connect_client_pubkey, response.as_json());
    Ok(EventBuilder::new(
        Kind::WalletConnectResponse,
        encrypted_response?,
        &[
            Tag::PubKey(connect_client_pubkey.to_owned(), None),
            Tag::Event(request_id.to_owned(), None, None),
        ],
    ))
}

/// Build NIP47 failure event
fn create_failure_note(
    connect_client_pubkey: &XOnlyPublicKey,
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
        &[
            Tag::PubKey(connect_client_pubkey.to_owned(), None),
            Tag::Event(request_id.to_owned(), None, None),
        ],
    ))
}

async fn connect_relay(
    url: Url,
    connect_client_pubkey: &XOnlyPublicKey,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let max_retries = 500;
    let mut delay = Duration::from_secs(10);

    for attempt in 0..max_retries {
        if let Ok((mut socket, _response)) = connect(url.clone()) {
            // Subscription filter
            let subscribe_to_requests = ClientMessage::new_req(
                SubscriptionId::generate(),
                vec![Filter::new()
                    .author(connect_client_pubkey.to_string())
                    .kind(Kind::WalletConnectRequest)],
            );

            socket.write_message(WsMessage::Text(subscribe_to_requests.as_json()))?;

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
    connect_client_pubkey: XOnlyPublicKey,
    relay: Url,
) -> Result<impl Stream<Item = RelayMessage>> {
    let socket = connect_relay(relay.clone(), &connect_client_pubkey).await?;

    let socket = Arc::new(Mutex::new(socket));

    Ok(futures::stream::unfold(
        (socket, relay, connect_client_pubkey),
        |(mut socket, relay, connect_client_pubkey)| async move {
            loop {
                let msg = match socket.clone().lock().await.read_message() {
                    Ok(msg) => msg,
                    Err(err) => {
                        // Handle disconnection
                        info!("WebSocket disconnected: {}", err);
                        info!("Attempting to reconnect ...");
                        match connect_relay(relay.clone(), &connect_client_pubkey).await {
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

                if let Ok(handled_message) = RelayMessage::from_json(msg_text) {
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
                    info!("Got unexpected message: {}", msg_text);
                }
            }
        },
    )
    .boxed())
}

struct Limits {
    /// Instant of our start
    hour_start: Instant,
    /// Value sent in hour
    hour_value: Amount,
    /// Hour limit
    hour_limit: Amount,
    /// Instant of day start
    day_start: Instant,
    /// Value sent in day
    day_value: Amount,
    /// Day limit
    day_limit: Amount,
}

const SECONDS_IN_HOUR: Duration = Duration::new(3600, 0);
const SECONDS_IN_DAY: Duration = Duration::new(86400, 0);

impl Limits {
    fn new(hour_limit: Amount, day_limit: Amount) -> Self {
        Self {
            hour_start: Instant::now(),
            hour_value: Amount::from_msat(0),
            hour_limit,
            day_start: Instant::now(),
            day_value: Amount::from_msat(0),
            day_limit,
        }
    }

    fn check_limit(&mut self, amount: Amount) -> Result<()> {
        if self.hour_start.elapsed() > SECONDS_IN_HOUR {
            self.reset_hour();
        }

        if self.day_start.elapsed() > SECONDS_IN_DAY {
            self.reset_day();
        }

        if (self.day_value + amount).msat().gt(&self.day_limit.msat()) {
            bail!("Daily spend limit exceeded")
        }

        if (self.hour_value + amount)
            .msat()
            .gt(&self.hour_limit.msat())
        {
            bail!("Hour spend limit exceeded")
        }

        Ok(())
    }

    fn reset_hour(&mut self) {
        self.hour_value = Amount::from_msat(0);
        self.hour_start = Instant::now();
    }

    fn reset_day(&mut self) {
        self.day_value = Amount::from_msat(0);
        self.day_start = Instant::now();
    }

    fn add_spend(&mut self, amount: Amount) {
        // Add amount to hour spend
        self.hour_value = self.hour_value.add(amount);

        // Add amount to day spend
        self.day_value = self.day_value.add(amount);
    }
}
