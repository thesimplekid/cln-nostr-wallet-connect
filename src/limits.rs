use std::ops::Add;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use cln_rpc::primitives::Amount;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Max invoice size
    pub max_invoice: Amount,
    /// Instant of our start
    pub hour_start: Instant,
    /// Value sent in hour
    pub hour_value: Amount,
    /// Hour limit
    pub hour_limit: Amount,
    /// Instant of day start
    pub day_start: Instant,
    /// Value sent in day
    pub day_value: Amount,
    /// Day limit
    pub day_limit: Amount,
}

const SECONDS_IN_HOUR: Duration = Duration::new(3600, 0);
const SECONDS_IN_DAY: Duration = Duration::new(86400, 0);

impl Limits {
    pub fn new(max_invoice: Amount, hour_limit: Amount, day_limit: Amount) -> Self {
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

    pub fn check_limit(&mut self, amount: Amount) -> Result<()> {
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

    pub fn reset_hour(&mut self) {
        self.hour_value = Amount::from_msat(0);
        self.hour_start = Instant::now();
    }

    pub fn reset_day(&mut self) {
        self.day_value = Amount::from_msat(0);
        self.day_start = Instant::now();
    }

    pub fn add_spend(&mut self, amount: Amount) {
        // Add amount to hour spend
        self.hour_value = self.hour_value.add(amount);

        // Add amount to day spend
        self.day_value = self.day_value.add(amount);
    }
}
