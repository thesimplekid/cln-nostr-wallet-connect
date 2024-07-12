use std::fmt;

use nostr_zapper::prelude::*;
use thiserror::Error;

use crate::Limits;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    ZapperError(#[from] nostr_zapper::ZapperError),
}

impl From<Error> for nostr_zapper::ZapperError {
    fn from(err: Error) -> nostr_zapper::ZapperError {
        nostr_zapper::ZapperError::backend(err)
    }
}

pub struct ClnZapper {
    limits: Limits,
    cln_rpc: cln_rpc::ClnRpc,
}

impl fmt::Debug for ClnZapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ClnZapper")
            .field("limts", &self.limits)
            .finish()
    }
}

impl fmt::Display for ClnZapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.limits)
    }
}

#[async_trait]
impl NostrZapper for ClnZapper {
    type Err = Error;

    fn backend(&self) -> ZapperBackend {
        ZapperBackend::Custom("cln".to_string())
    }

    async fn pay(&self, invoice: String) -> Result<(), Self::Err> {
        println!("{}", invoice);
        todo!()
    }
}
