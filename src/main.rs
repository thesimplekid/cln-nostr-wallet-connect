mod utils;

use anyhow::{bail, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_plugin::Plugin;
use cln_rpc::model::requests::SendpayRoute;
use cln_rpc::model::{
    requests::{GetrouteRequest, PayRequest},
    Request, Response,
};
use cln_rpc::model::{GetrouteRoute, SendpayRequest, WaitsendpayRequest, WaitsendpayResponse};
use cln_rpc::primitives::{Amount, PublicKey, Secret, Sha256};
use cln_rpc::ClnRpc;
use futures::{Stream, StreamExt};
use lightning_invoice::{Description, Invoice, InvoiceDescription};
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

    info!("Client pub key: {}", nostr_client_pubkey);
    info!("###########");
    info!("My priv: {}", keys.secret_key()?.display_secret());
    info!("My pub: {}", keys.public_key());

    let nostr_relay = Url::from_str(&nostr_relay)?;
    let nostr_pubkey = XOnlyPublicKey::from_str(&nostr_client_pubkey)?;

    let client = utils::create_client(&keys, vec![nostr_relay.to_string()]).await?;
    info!("Nostr client created");

    let mut invoices = event_stream(nostr_pubkey, nostr_relay).await?;
    while let Some(event) = invoices.next().await {
        // Check event is valid
        if let Err(_) = event.verify() {
            info!("Event {} is invalid", event.id.to_hex());
            continue;
        }

        // Check event is from correct pubkey
        if event.pubkey.ne(&nostr_pubkey) {
            info!("Event from incorrect pubkey: {}", event.pubkey.to_string());
            continue;
        }

        let content = decrypt(&keys.secret_key()?, &nostr_pubkey, event.content)?;

        info!("Event content: {}", content);

        info!("Event Tags: {:?}", event.tags);

        let bolt11 = content.parse::<Invoice>()?;

        info!("Bolt11: {bolt11}");

        info!("payee pub {:?}", bolt11.recover_payee_pub_key().to_string());
        info!("payment hash {}", bolt11.payment_hash());

        // let mut response_event = None;

        if let (Some(amount), payee_pub_key, payment_hash) = (
            bolt11.amount_milli_satoshis(),
            bolt11.recover_payee_pub_key(),
            bolt11.payment_hash(),
        ) {
            let amount = Amount::from_msat(amount);

            // Check amount is > then config max
            if amount.msat().gt(&(max_invoice_amount as u64)) {
                bail!("Invoice too large: {amount:?} > {max_invoice_amount}")
            }

            /*
            info!("Getting route");
            let route = match get_route(&mut cln_client, &payee_pub_key, amount).await {
                Ok(route) => route,
                Err(err) => {
                    info!("get route error: {err}");
                    continue;
                }
            };

            info!("Got route {:?}", route);

            let mut retries = 0;
            while retries < 5 {
                let route: Vec<SendpayRoute> = route
                    .iter()
                    .map(|r| SendpayRoute {
                        amount_msat: r.amount_msat,
                        id: r.id,
                        delay: r.delay as u16,
                        channel: r.channel,
                    })
                    .collect();

                let hash =
                    match send_payment(&mut cln_client, &bolt11.to_string(), *payment_hash, route)
                        .await
                    {
                        Ok(send) => send,
                        Err(err) => {
                            info!("Send error: {err}");
                            retries += 1;
                            continue;
                        }
                    };
                info!("Started payment: {:?}", hash);
                match wait_send(&mut cln_client, hash).await {
                    Ok(res) => {
                        if let Some(preimage) = res.payment_preimage {
                            response_event = Some(create_succuss_note(preimage));
                        } else {
                            response_event = Some(create_failure_note("No preimage"));
                        }
                        break;
                    }
                    Err(err) => {
                        info!("Wait send {:?}", err);
                        retries += 1;
                    }
                }
            }

            if let Some(response_event) = response_event {
                let event = response_event.to_event(&keys)?;
                client.send_event(event).await.ok();
            } else {
                let event = create_failure_note("").to_event(&keys)?;
                client.send_event(event).await.ok();
            }

            */
            let description = match bolt11.description() {
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

            info!("cln response {:?}", cln_response);

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

// async fn decode_invoice(cln_client: &mut ClnRpc, bolt11: &str) -> Result<Decod>

async fn get_route(
    cln_client: &mut ClnRpc,
    id: &PublicKey,
    amount: Amount,
) -> Result<Vec<GetrouteRoute>> {
    let cln_response = cln_client
        .call(Request::GetRoute(GetrouteRequest {
            id: id.to_owned(),
            amount_msat: amount,
            riskfactor: 10,
            cltv: None,
            fromid: None,
            fuzzpercent: None,
            exclude: None,
            maxhops: None,
        }))
        .await;

    if let Ok(Response::GetRoute(route)) = cln_response {
        return Ok(route.route);
    }

    info!("Could not find route to {}", id);
    bail!("Could not find route")
}

async fn send_payment(
    cln_client: &mut ClnRpc,
    bolt11: &str,
    payment_hash: Sha256,
    route: Vec<SendpayRoute>,
) -> Result<Sha256> {
    let cln_response = cln_client
        .call(Request::SendPay(SendpayRequest {
            route,
            payment_hash,
            label: None,
            amount_msat: None,
            bolt11: Some(bolt11.to_string()),
            payment_secret: None,
            partid: None,
            localinvreqid: None,
            groupid: None,
        }))
        .await;

    info!("send res {:?}", cln_response);

    if let Ok(Response::SendPay(res)) = cln_response {
        return Ok(res.payment_hash);
    }

    info!("Could not pay {}", payment_hash);
    bail!("Could not pay")
}

async fn wait_send(cln_client: &mut ClnRpc, payment_hash: Sha256) -> Result<WaitsendpayResponse> {
    let cln_response = cln_client
        .call(Request::WaitSendPay(WaitsendpayRequest {
            payment_hash,
            timeout: None,
            partid: None,
            groupid: None,
        }))
        .await;

    match cln_response {
        Ok(Response::WaitSendPay(res)) => Ok(res),
        Err(err) => {
            info!("wait send error: {:?}", err);
            bail!("Wait send Error")
        }
        _ => {
            info!("CLN retunred wrong wait send res");
            bail!("Wait send error ");
        }
    }
}

fn create_succuss_note(preimage: Secret) -> EventBuilder {
    EventBuilder::new(Kind::Custom(23195), hex::encode(preimage.to_vec()), &[])
}

fn create_failure_note(reason: &str) -> EventBuilder {
    EventBuilder::new(Kind::Custom(23196), reason, &[])
}

async fn event_stream(
    connect_client_pubkey: XOnlyPublicKey,
    relay: Url,
) -> Result<impl Stream<Item = Box<Event>>> {
    let (mut socket, _response) = connect(relay).expect("Can't connect");
    let subscribe_to_requests = ClientMessage::new_req(
        SubscriptionId::new("1234567"),
        vec![Filter::new()
            .authors(vec![connect_client_pubkey])
            .kind(Kind::Custom(23194))],
    );

    socket.write_message(WsMessage::Text(subscribe_to_requests.as_json()))?;

    info!("Waiting for requests");

    let socket = Arc::new(Mutex::new(socket));

    Ok(
        futures::stream::unfold(socket.clone(), |socket| async move {
            // We loop here since some invoices aren't zaps, in which case we wait for the next one and don't yield
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
                        RelayMessage::Auth { challenge } => {
                            info!("Got non event message: {}", challenge)
                        }
                        RelayMessage::Empty => info!("got empty"),
                        RelayMessage::EndOfStoredEvents(id) => info!("End of stored {:?}", id),
                        RelayMessage::Notice { message } => info!("Notice: {message}"),
                        RelayMessage::Ok {
                            event_id,
                            status,
                            message,
                        } => info!("OK: {} {} {}", event_id, status, message),
                    }
                } else {
                    info!("Got unexpected message: {}", msg_text);
                }
            }
        })
        .boxed(),
    )
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_decode_invoice() {
        let bolt11 = "lnbc210n1pjztxwgpp5wf86xhpt998udqhz5cgsqu6hu27rxzjk5yexhxujzxncl34xeuxqhp5swd4f4ae95wr4e7plknudsr40dv9psyp0w79zhkz4chejl7354gscqzpgxqzfvsp5274r8p4uykxhp84fswevtjxpfcpw5vevdndhve76m8m8x08se4js9qyyssqqhse6xqxwvqg387jmu32swsz0yz4maxjsy6r8puzjgap0evav97xlyq6s0qpx7ejdsuarw5ma8e6msm0u8vx002emh3hux6h26xlqzcq9plszm";
        let invoice = bolt11.parse::<Invoice>().unwrap();

        println!("{:?}", invoice.recover_payee_pub_key().to_owned());
        panic!()
    }
}
