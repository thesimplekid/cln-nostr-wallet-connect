use std::net::TcpStream;
use std::ops::Add;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_plugin::Plugin;
use cln_rpc::model::ListfundsRequest;
use cln_rpc::model::{requests::PayRequest, Request, Response};
use cln_rpc::primitives::{Amount, Secret};
use cln_rpc::ClnRpc;
use dirs::config_dir;
use futures::{Stream, StreamExt};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription};
use log::{debug, info, warn};
use nostr_sdk::prelude::{GetBalanceResponseResult, PayInvoiceResponseResult};
use nostr_sdk::secp256k1::{SecretKey, XOnlyPublicKey};
use nostr_sdk::{
    nips::{
        nip04::{decrypt, encrypt},
        nip47,
    },
    ClientMessage, EventBuilder, Filter, Kind, SubscriptionId,
};
use nostr_sdk::{Event, EventId, Keys, RelayMessage, Tag, Url};
use tokio::io::{stdin, stdout};
use tokio::sync::Mutex;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message as WsMessage, WebSocket};

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
    let plugin = if let Some(plugin) = cln_plugin::Builder::new(stdin(), stdout())
        .option(ConfigOption::new(
            WALLET_NSEC,
            Value::OptString,
            "Nsec to publish success/failure events",
        ))
        .option(ConfigOption::new(
            CLIENT_SECRET,
            Value::OptString,
            "Nostr pubkey to accept requests from",
        ))
        // TODO: Would be better to be a list
        .option(ConfigOption::new(
            RELAY,
            Value::String("ws://localhost:8080".to_string()),
            "Default nostr relay",
        ))
        .option(ConfigOption::new(
            MAX_INVOICE_SAT,
            Value::Integer(5000),
            "Max size of an invoice (sat)",
        ))
        .option(ConfigOption::new(
            HOUR_LIMIT_SAT,
            Value::Integer(10000),
            "Max sats to spend per hour"
        ))
        .option(ConfigOption::new(
            DAY_LIMIT_SAT,
            Value::Integer(3500),
            "Max sats to spend per day"
        ))
        .option(ConfigOption::new(
            CONFIG_PATH,
            Value::OptString,
            "Nostr wallet connect config path",
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
    let cln_client = Arc::new(Mutex::new(cln_rpc::ClnRpc::new(&rpc_socket).await?));

    let config_path = match plugin.option(CONFIG_PATH) {
        Some(Value::String(config_path)) => PathBuf::from_str(&config_path)?,
        _ => config_dir()
            .unwrap()
            .join("cln-nostr-wallet-connect")
            .join("config"),
    };

    info!("Nostr Wallet Connect Config: {:?}", config_path);

    // Wallet key keys
    // If key is defined in CLN config that is used if not checks if key in plugin config
    // if no key found generate a new key and write to config
    let keys = match plugin.option(WALLET_NSEC) {
        Some(Value::String(wallet_nsec)) => utils::handle_keys(Some(wallet_nsec))?,
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

    let connect_client_keys = match plugin.option(CLIENT_SECRET) {
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
        .option(RELAY)
        .expect("Option is defined")
        .as_str()
        .expect("Option is a string")
        .to_owned();

    let max_invoice_amount = plugin
        .option(MAX_INVOICE_SAT)
        .expect("Option is defined")
        .as_i64()
        .expect("Option is a i64")
        .to_owned();

    let hour_limit = plugin
        .option(HOUR_LIMIT_SAT)
        .expect("Option is defined")
        .as_i64()
        .expect("Option is a i64")
        .to_owned();

    let day_limit = plugin
        .option(DAY_LIMIT_SAT)
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
        None,
    )?;

    info!("{}", wallet_connect_uri.to_string());

    // Publish Wallet Connect info
    let info_event =
        EventBuilder::new(Kind::WalletConnectInfo, "pay_invoice", &[]).to_event(&keys)?;

    // Publish info Event
    client.send_event(info_event).await?;

    let limits = Arc::new(Mutex::new(Limits::new(
        Amount::from_sat(max_invoice_amount as u64),
        Amount::from_sat(hour_limit as u64),
        Amount::from_sat(day_limit as u64),
    )));

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
                        &keys.secret_key()?,
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
                            handle_pay_invoice(
                                &event,
                                &keys,
                                pay_invoice_param,
                                cln_client.clone(),
                                limits.clone(),
                            )
                            .await
                        }
                        nip47::RequestParams::GetBalance => {
                            handle_get_balance(&event, &keys, cln_client.clone()).await
                        }
                        nip47::RequestParams::MakeInvoice(_) => {
                            bail!("NIP47 Make invoice is not implemented")
                        }
                        nip47::RequestParams::LookupInvoice(_) => {
                            bail!("NIP47 Lookup invoice is not implmented")
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
                                &keys.secret_key()?,
                                "CL RPC Error",
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

async fn handle_get_balance(
    event: &Event,
    keys: &Keys,
    cln_client: Arc<Mutex<ClnRpc>>,
) -> Result<EventBuilder> {
    let cln_response = cln_client
        .lock()
        .await
        .call(Request::ListFunds(ListfundsRequest { spent: None }))
        .await;

    let event_builder = match cln_response {
        Ok(Response::ListFunds(funds)) => {
            let balance: u64 = funds
                .channels
                .iter()
                .map(|c| c.our_amount_msat.msat())
                .sum();

            let balance = Amount::from_msat(balance);

            info!("Balance Requested: {:?}", balance);

            // Add spend value to daily and hourly limit tracking

            let response = nip47::Response {
                result_type: nip47::Method::GetBalance,
                error: None,
                result: Some(nip47::ResponseResult::GetBalance(
                    GetBalanceResponseResult {
                        balance: balance.msat(),
                        // TODO: Return limits
                        max_amount: None,
                        budget_renewal: None,
                    },
                )),
            };

            let encrypted_response =
                encrypt(&keys.secret_key()?, &event.pubkey, response.as_json())?;

            EventBuilder::new(
                Kind::WalletConnectResponse,
                encrypted_response,
                &[
                    Tag::PubKey(event.pubkey.to_owned(), None),
                    Tag::Event(event.id.to_owned(), None, None),
                ],
            )
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

    Ok(event_builder)
}

async fn handle_pay_invoice(
    event: &Event,
    keys: &Keys,
    params: nip47::PayInvoiceRequestParams,
    cln_client: Arc<Mutex<ClnRpc>>,
    limits: Arc<Mutex<Limits>>,
) -> Result<EventBuilder> {
    let bolt11 = params.invoice.parse::<Bolt11Invoice>()?;

    let amount = bolt11
        .amount_milli_satoshis()
        .ok_or(anyhow!("Invoice amount undefinded"))?;

    let mut limits = limits.lock().await;
    let amount = Amount::from_msat(amount);

    // Check amount is < then config max
    if amount.msat().gt(&(limits.max_invoice.msat())) {
        info!("Invoice too large: {amount:?} > {:?}", limits.max_invoice);
        bail!("Invoice over max");
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
        bail!("Limits exceeded");
    }

    // Currently there is some debate over whether the description hash should be known or not before paying.
    // The change in CLN requiring the description was reverted but it is still deprecated
    // With NIP47 as of now the description is unknown
    // This may need to be reviewed in the future
    // https://github.com/ElementsProject/lightning/pull/6092
    // https://github.com/ElementsProject/lightning/releases/tag/v23.02.2
    let _description = match bolt11.description() {
        Bolt11InvoiceDescription::Direct(des) => des.to_string(),
        Bolt11InvoiceDescription::Hash(hash) => hash.0.to_string(),
    };

    debug!("Pay invoice request: {:?}, {}", event.id, bolt11);
    // Send payment
    let cln_response = cln_client
        .lock()
        .await
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

    limits.add_spend(amount);
    info!("CL RPC Response: {:?}", cln_response);

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

    Ok(event_builder)
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
        result: Some(nip47::ResponseResult::PayInvoice(
            PayInvoiceResponseResult {
                preimage: hex::encode(preimage.to_vec()),
            },
        )),
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
    connect_client_pubkey: XOnlyPublicKey,
    relay: Url,
) -> Result<impl Stream<Item = RelayMessage>> {
    let socket = connect_relay(relay.clone(), &connect_client_pubkey).await?;

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

struct Limits {
    /// Max invoice size
    max_invoice: Amount,
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
    fn new(max_invoice: Amount, hour_limit: Amount, day_limit: Amount) -> Self {
        Self {
            max_invoice,
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
