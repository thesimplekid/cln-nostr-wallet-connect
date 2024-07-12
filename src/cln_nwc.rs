use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use cln_rpc::model::requests::{ListfundsRequest, PayRequest};
use cln_rpc::model::responses::PayStatus;
use cln_rpc::model::Response;
use cln_rpc::primitives::Amount;
use cln_rpc::Request;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::Limits;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Limit exceeded")]
    LimitExceeded,
    #[error("Invoice max amount")]
    InvoiceOverMaxAmount,
    #[error("Invoice amount undefinded")]
    InvoiceAmountundefinded,
    #[error("Invalid Invoice")]
    InvalidInvoice,
    #[error("Payment Pending")]
    PaymentPending,
    #[error("Payment Failed")]
    PaymentFailed,
    #[error("Wrong Cln response")]
    WrongClnResponse,
    #[error("RPC Error")]
    RpcErrorResponse,
}

pub struct ClnNwc {
    limits: Limits,
    cln_rpc: Arc<Mutex<cln_rpc::ClnRpc>>,
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
    pub async fn new(limits: Limits, cln_rpc_socket: PathBuf) -> Result<Self, Error> {
        let cln_client = cln_rpc::ClnRpc::new(&cln_rpc_socket).await.map_err(|err| {
            log::error!("Could not start cln rpc: {}", err);
            Error::RpcErrorResponse
        })?;

        Ok(Self {
            limits,
            cln_rpc: Arc::new(Mutex::new(cln_client)),
        })
    }

    pub async fn pay_invoice<S>(&self, invoice: S) -> Result<String, Error>
    where
        S: Into<String>,
    {
        let bolt11 = Bolt11Invoice::from_str(&invoice.into()).map_err(|_| Error::InvalidInvoice)?;

        let amount = bolt11
            .amount_milli_satoshis()
            .ok_or(Error::InvoiceAmountundefinded)?;

        let mut limits = self.limits;
        let amount = Amount::from_msat(amount);

        // Check amount is < then config max
        if amount.msat().gt(&(limits.max_invoice.msat())) {
            log::info!("Invoice too large: {amount:?} > {:?}", limits.max_invoice);
            return Err(Error::InvoiceOverMaxAmount);
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
            return Err(Error::LimitExceeded);
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
                partial_msat: None,
            }))
            .await;

        match cln_response {
            Ok(Response::Pay(res)) => {
                // Add spend value to daily and hourly limit tracking
                match res.status {
                    PayStatus::COMPLETE => {
                        limits.add_spend(amount);
                        Ok(hex::encode(res.payment_preimage.to_vec()))
                    }
                    PayStatus::PENDING => {
                        log::info!("CLN returned pending response");
                        limits.add_spend(amount);
                        Err(Error::PaymentPending)
                    }
                    PayStatus::FAILED => {
                        log::error!("Payment failed: {}", bolt11.payment_hash());
                        Err(Error::PaymentFailed)
                    }
                }
            }
            Ok(response) => {
                log::error!("Wrong cln response for pay request: {:?}", response);
                Err(Error::WrongClnResponse)
            }
            Err(err) => {
                log::error!("Cln error response for pay: {:?}", err);
                Err(Error::RpcErrorResponse)
            }
        }

        // Build event response
    }

    pub async fn get_balance(&self) -> Result<Amount, Error> {
        let cln_response = self
            .cln_rpc
            .lock()
            .await
            .call(Request::ListFunds(ListfundsRequest { spent: None }))
            .await;

        match cln_response {
            Ok(Response::ListFunds(funds)) => {
                let balance: u64 = funds
                    .channels
                    .iter()
                    .map(|c| c.our_amount_msat.msat())
                    .sum();

                let balance = Amount::from_msat(balance);

                Ok(balance)
            }
            Ok(response) => {
                log::error!("Wrong cln response for pay request: {:?}", response);
                Err(Error::WrongClnResponse)
            }
            Err(err) => {
                log::error!("Cln error response for pay: {:?}", err);
                Err(Error::RpcErrorResponse)
            }
        }
    }
}
