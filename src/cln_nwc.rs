use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use cln_rpc::model::requests::{ListfundsRequest, PayRequest};
use cln_rpc::model::responses::PayStatus;
use cln_rpc::model::Response;
use cln_rpc::primitives::Amount;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription};
use nip47::Request;
use nostr_sdk::nips::nip04::{decrypt, encrypt};
use nostr_sdk::nips::nip47::{
    self, GetBalanceResponseResult, NIP47Error, NostrWalletConnectURI, PayInvoiceResponseResult,
};
use nostr_sdk::{Client, Event, EventBuilder, EventId, JsonUtil, Keys, Kind, Tag, Url};
use tokio::sync::Mutex;

use crate::Limits;

#[derive(Clone)]
pub struct ClnNwc {
    /// Keys that the connecting nostr client will use to send messages
    client_keys: Keys,
    /// Keys that the nwc plugin will use to send and receive messages
    nwc_keys: Keys,
    relay: Url,
    limits: Limits,
    nwc_url: NostrWalletConnectURI,
    cln_rpc: Arc<Mutex<cln_rpc::ClnRpc>>,
    nostr_client: Client,
}

impl fmt::Debug for ClnNwc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ClnZapper")
            .field("limits", &self.limits)
            .finish()
    }
}

impl fmt::Display for ClnNwc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.limits)
    }
}

impl ClnNwc {
    pub async fn new(
        client_keys: Keys,
        nwc_keys: Keys,
        relay: Url,
        limits: Limits,
        cln_rpc_socket: PathBuf,
    ) -> Result<Self> {
        let cln_client = cln_rpc::ClnRpc::new(&cln_rpc_socket).await.map_err(|err| {
            log::error!("Could not start cln rpc: {}", err);
            anyhow!("Could not start cln rpc")
        })?;

        let wallet_connect_uri = NostrWalletConnectURI::new(
            nwc_keys.public_key(),
            relay.clone(),
            client_keys.secret_key()?.clone(),
            None,
        );

        let client = Client::new(nwc_keys.clone());
        client.add_relay(relay.clone()).await?;
        client.connect().await;

        Ok(Self {
            client_keys,
            nwc_keys,
            relay,
            limits,
            nwc_url: wallet_connect_uri,
            cln_rpc: Arc::new(Mutex::new(cln_client)),
            nostr_client: client,
        })
    }

    /// Checks that it is a valid event and from an authorized pubkey
    pub async fn verify_event(&self, event: &Event) -> Result<Request> {
        if event.verify().is_err() {
            log::info!("Event {} is invalid", event.id.to_hex());
            bail!("Event is not a valid nostr event")
        }

        // Decrypt bolt11 from content (NIP04)
        let content = match decrypt(
            &self.nwc_keys.secret_key()?.clone(),
            &self.client_keys.public_key(),
            &event.content,
        ) {
            Ok(content) => content,
            Err(err) => {
                log::info!("Could not decrypt: {err}");
                bail!("Could not decrypt event");
            }
        };

        let request = match nip47::Request::from_json(&content) {
            Ok(req) => req,
            Err(err) => {
                log::warn!("Could not decode request {:?}", err);
                bail!("Could not decrypt event");
            }
        };

        // Check event is from correct pubkey
        if event.pubkey.ne(&self.client_keys.public_key()) {
            log::info!("Event from incorrect pubkey: {}", event.pubkey.to_string());
            let response = nip47::Response {
                result_type: request.method,
                error: Some(NIP47Error {
                    code: nip47::ErrorCode::Unauthorized,
                    message: "Unauthorized pubkey".to_string(),
                }),
                result: None,
            };

            let encrypted_response = encrypt(
                self.nwc_keys.secret_key()?,
                &self.client_keys.public_key(),
                response.as_json(),
            );
            let event_builder = EventBuilder::new(
                Kind::WalletConnectResponse,
                encrypted_response?,
                [
                    Tag::public_key(self.client_keys.public_key()),
                    Tag::event(event.id),
                ],
            );

            let event = event_builder.to_event(&self.nwc_keys)?;

            self.nostr_client.send_event(event).await?;
        }

        Ok(request)
    }

    pub async fn auth(&self, challenge: &str, relay: Url) -> Result<()> {
        if let Ok(auth_event) =
            EventBuilder::auth(challenge, relay.clone()).to_event(&self.nwc_keys)
        {
            if let Err(err) = self.nostr_client.send_event(auth_event).await {
                log::warn!("Could not broadcast event: {err}");
                bail!("Could not broadcast auth event")
            }
        }

        Ok(())
    }

    pub async fn pay_invoice<S>(&self, event_id: EventId, invoice: S) -> anyhow::Result<()>
    where
        S: Into<String>,
    {
        let bolt11 =
            Bolt11Invoice::from_str(&invoice.into()).map_err(|_| anyhow!("Invalid invoice"))?;

        let amount = bolt11
            .amount_milli_satoshis()
            .ok_or(anyhow!("Invoice amount is not defined"))?;

        let mut limits = self.limits;
        let amount = Amount::from_msat(amount);

        // Check amount is < then config max
        if amount.msat().gt(&(limits.max_invoice.msat())) {
            log::info!("Invoice too large: {amount:?} > {:?}", limits.max_invoice);
            bail!("Limit too large");
        }

        // Check spend does not exceed daily or hourly limit
        if limits.check_limit(amount).is_err() {
            log::info!("Sending {} msat will exceed limit", amount.msat());
            log::info!(
                "Hour limit: {} msats, Hour spent: {} msats",
                limits.hour_limit.msat(),
                limits.hour_value.msat()
            );

            log::info!(
                "Day limit: {} msats, Day Spent: {} msats",
                limits.day_limit.msat(),
                limits.day_value.msat()
            );
            bail!("Limits exceeded");
        }

        // Currently there is some debate over whether the description hash should be
        // known or not before paying. The change in CLN requiring the description
        // was reverted but it is still deprecated With NIP47 as of now the
        // description is unknown This may need to be reviewed in the future
        // https://github.com/ElementsProject/lightning/pull/6092
        // https://github.com/ElementsProject/lightning/releases/tag/v23.02.2
        let _description = match bolt11.description() {
            Bolt11InvoiceDescription::Direct(des) => des.to_string(),
            Bolt11InvoiceDescription::Hash(hash) => hash.0.to_string(),
        };

        log::debug!("Pay invoice request: {}", bolt11);
        // Send payment
        let cln_response = self
            .cln_rpc
            .lock()
            .await
            .call(cln_rpc::Request::Pay(PayRequest {
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
                partial_msat: None,
            }))
            .await;

        let event_builder = match cln_response {
            Ok(Response::Pay(res)) => {
                // Add spend value to daily and hourly limit tracking
                match res.status {
                    PayStatus::COMPLETE => {
                        limits.add_spend(amount);
                        let preimage = hex::encode(res.payment_preimage.to_vec());
                        let response = nip47::Response {
                            result_type: nip47::Method::PayInvoice,
                            error: None,
                            result: Some(nip47::ResponseResult::PayInvoice(
                                PayInvoiceResponseResult { preimage },
                            )),
                        };

                        let encrypted_response = encrypt(
                            self.nwc_keys.secret_key()?,
                            &self.client_keys.public_key(),
                            response.as_json(),
                        );
                        EventBuilder::new(
                            Kind::WalletConnectResponse,
                            encrypted_response?,
                            [
                                Tag::public_key(self.client_keys.public_key()),
                                Tag::event(event_id),
                            ],
                        )
                    }
                    PayStatus::PENDING => {
                        log::info!("CLN returned pending response");
                        limits.add_spend(amount);
                        bail!("Payment pending");
                    }
                    PayStatus::FAILED => {
                        log::error!("Payment failed: {}", bolt11.payment_hash());
                        bail!("Payment Failed");
                    }
                }
            }
            Ok(response) => {
                log::error!("Wrong cln response for pay request: {:?}", response);
                bail!("Wrong CLN response")
            }
            Err(err) => {
                log::error!("Cln error response for pay: {:?}", err);
                bail!("Rpc error response")
            }
        };

        let event = event_builder.to_event(&self.nwc_keys)?;

        self.nostr_client.send_event(event).await?;

        Ok(())
        // Build event response
    }

    pub async fn get_balance(&self, event: &Event) -> anyhow::Result<()> {
        let cln_response = self
            .cln_rpc
            .lock()
            .await
            .call(cln_rpc::Request::ListFunds(ListfundsRequest {
                spent: None,
            }))
            .await;

        let event_builder = match cln_response {
            Ok(Response::ListFunds(funds)) => {
                let balance: u64 = funds
                    .channels
                    .iter()
                    .map(|c| c.our_amount_msat.msat())
                    .sum();

                let balance = Amount::from_msat(balance);
                let response = nip47::Response {
                    result_type: nip47::Method::GetBalance,
                    error: None,
                    result: Some(nip47::ResponseResult::GetBalance(
                        GetBalanceResponseResult {
                            balance: balance.msat(),
                        },
                    )),
                };

                let encrypted_response = encrypt(
                    &self.nwc_keys.secret_key()?.clone(),
                    &event.pubkey,
                    response.as_json(),
                )?;

                EventBuilder::new(
                    Kind::WalletConnectResponse,
                    encrypted_response,
                    vec![
                        Tag::public_key(event.pubkey.to_owned()),
                        Tag::event(event.id.to_owned()),
                    ],
                )
            }
            Ok(response) => {
                log::error!("Wrong cln response for pay request: {:?}", response);
                bail!("Wrong CLN response")
            }
            Err(err) => {
                log::error!("Cln error response for pay: {:?}", err);
                bail!("Rpc error response")
            }
        };

        let event = event_builder.to_event(&self.nwc_keys)?;

        self.nostr_client.send_event(event).await?;
        Ok(())
    }
}
