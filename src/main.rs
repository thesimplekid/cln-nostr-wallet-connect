mod utils;

use anyhow::{bail, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_plugin::Plugin;
use cln_rpc::model::{requests::PayRequest, Request, Response};
use cln_rpc::primitives::{Amount, Secret};
use futures::{Stream, StreamExt};
use lightning_invoice::{Invoice, InvoiceDescription};
use nostr_sdk::secp256k1::XOnlyPublicKey;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::io::{stdin, stdout};

use nostr_sdk::nips::nip04::decrypt;
use nostr_sdk::{event::Event, ClientMessage, EventBuilder, Filter, Kind, SubscriptionId};
use nostr_sdk::{RelayMessage, Url};

use tungstenite::{connect, Message as WsMessage};

use log::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    info!("Starting cln-nostr-connect");
    let plugin = if let Some(plugin) = cln_plugin::Builder::new(stdin(), stdout())
        .option(ConfigOption::new(
            "nostr_connect_nsec",
            Value::String("".into()),
            "Nsec to publish success/failure events",
        ))
        .option(ConfigOption::new(
            "nostr_connect_client_pubkey",
            Value::String("".into()),
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
            Value::Integer(1000000),
            "Max size of an invoice (msat)",
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

    let keys = match plugin.option("nostr_connect_nsec") {
        Some(Value::String(nsec)) => utils::handle_keys(Some(nsec))?,
        _ => utils::handle_keys(None)?,
    };

    let nostr_client_pubkey = plugin
        .option("nostr_connect_client_pubkey")
        .expect("Option is defined")
        .as_str()
        .expect("Option is a string")
        .to_owned();

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

    // Realy to listen for events and publish events to
    let nostr_relay = Url::from_str(&nostr_relay)?;

    // pub key of client
    let nostr_pubkey = XOnlyPublicKey::from_str(&nostr_client_pubkey)?;

    let client = utils::create_client(&keys, vec![nostr_relay.to_string()]).await?;

    let mut invoices = event_stream(nostr_pubkey, nostr_relay).await?;
    while let Some(event) = invoices.next().await {
        // Check event is valid
        if event.verify().is_err() {
            info!("Event {} is invalid", event.id.to_hex());
            continue;
        }

        // Check event is from correct pubkey
        if event.pubkey.ne(&nostr_pubkey) {
            info!("Event from incorrect pubkey: {}", event.pubkey.to_string());
            continue;
        }

        // Decrypt bolt11 from content (NIP04)
        let content = decrypt(&keys.secret_key()?, &nostr_pubkey, event.content)?;

        let bolt11 = content.parse::<Invoice>()?;

        if let Some(amount) = bolt11.amount_milli_satoshis() {
            let amount = Amount::from_msat(amount);

            // Check amount is < then config max
            if amount.msat().gt(&(max_invoice_amount as u64)) {
                bail!("Invoice too large: {amount:?} > {max_invoice_amount}")
            }

            // Currently there is some debate over whether the description hash should be known or before paying.
            // The change in CLN requiring the description was reverted
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
                Ok(Response::Pay(res)) => create_succuss_note(res.payment_preimage),
                _ => {
                    info!("Payment failed: {}", event.id);
                    create_failure_note("")
                }
            };

            let event = event_builder.to_event(&keys)?;

            client.send_event(event).await?;
        }
    }

    Ok(())
}

/// Build NIP47 success event
fn create_succuss_note(preimage: Secret) -> EventBuilder {
    EventBuilder::new(Kind::Custom(23195), hex::encode(preimage.to_vec()), &[])
}

/// Build NIP47 failure event
fn create_failure_note(reason: &str) -> EventBuilder {
    EventBuilder::new(Kind::Custom(23196), reason, &[])
}

async fn event_stream(
    connect_client_pubkey: XOnlyPublicKey,
    relay: Url,
) -> Result<impl Stream<Item = Box<Event>>> {
    let (mut socket, _response) = connect(relay).expect("Can't connect");

    // Subscription filter
    let subscribe_to_requests = ClientMessage::new_req(
        SubscriptionId::new("1234567"),
        vec![Filter::new()
            .authors(vec![connect_client_pubkey])
            .kind(Kind::Custom(23194))],
    );

    socket.write_message(WsMessage::Text(subscribe_to_requests.as_json()))?;

    info!("Waiting for requests");

    let socket = Arc::new(Mutex::new(socket));

    Ok(futures::stream::unfold(socket, |socket| async move {
        loop {
            let msg = socket
                .lock()
                .unwrap()
                .read_message()
                .expect("Error reading message");
            let msg_text = msg.to_text().expect("Failed to conver message to text");
            if let Ok(handled_message) = RelayMessage::from_json(msg_text) {
                match handled_message {
                    RelayMessage::Event { event, .. } => {
                        info!("Got an event: {}", event.id);
                        break Some((event, socket));
                    }
                    _ => continue,
                }
            } else {
                info!("Got unexpected message: {}", msg_text);
            }
        }
    })
    .boxed())
}
