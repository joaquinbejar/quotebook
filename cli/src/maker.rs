//! Running a maker service against a cluster: read the series and the maker's window, resume
//! the issuer (fail-closed cold start), and refresh it from chain when the cold start ends.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use quotebook_core::{BaseLots, Seconds, UnixTimestamp, MAX_CLOCK_SKEW};
use quotebook_maker::issuer::NonceIssuer;
use quotebook_maker::keys::MakerKey;
use quotebook_maker::pricing::PricingParams;
use quotebook_maker::server::{FixedInventory, FixedMid, MakerService, ServiceConfig, TimeSource};
use rust_decimal::Decimal;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::market;
use crate::rpc::{clock, Rpc};
use crate::CliError;

/// Maker settings.
#[derive(Debug, Clone, Copy)]
pub struct MakerSettings {
    /// The quotebook program.
    pub program_id: Address,
    /// The series quoted.
    pub series: Address,
    /// The maker's base token account.
    pub maker_base: Address,
    /// The maker's quote token account.
    pub maker_quote: Address,
    /// Fixed mid, quote tokens per base token (Pyth Hermes is not in v1).
    pub mid: Decimal,
    /// Model and bounds.
    pub pricing: PricingParams,
    /// Quote lifetime.
    pub ttl: Seconds,
    /// Largest size quoted, base lots.
    pub max_size: BaseLots,
    /// The longest lifetime this key ever signed on any series; at least the series'
    /// `max_quote_ttl`. Sets the cold start.
    pub max_ttl_any_series: Seconds,
}

/// A clock that follows the cluster: wall time plus an offset measured against the Clock
/// sysvar. The quotes' `issued_at` must track the cluster clock within `MAX_CLOCK_SKEW`.
#[derive(Debug, Clone, Default)]
pub struct ClusterClock(Arc<AtomicI64>);

fn wall() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    i64::try_from(secs).unwrap_or(i64::MAX)
}

impl ClusterClock {
    /// Re-measures the offset from the Clock sysvar.
    ///
    /// # Errors
    ///
    /// The RPC errors.
    pub fn sync(&self, rpc: &mut dyn Rpc) -> Result<UnixTimestamp, CliError> {
        let cluster = clock(rpc)?;
        self.0
            .store(cluster.get().saturating_sub(wall()), Ordering::SeqCst);
        Ok(cluster)
    }
}

impl TimeSource for ClusterClock {
    fn now(&self) -> UnixTimestamp {
        UnixTimestamp::new(wall().saturating_add(self.0.load(Ordering::SeqCst)))
    }
}

/// A prepared service and the end of its cold start.
#[derive(Debug)]
pub struct Prepared {
    /// The service, ready to `serve`; it refuses to quote until [`refresh`] succeeds.
    pub service: Arc<MakerService>,
    /// The first cluster time at which it may quote.
    pub cold_start_until: UnixTimestamp,
}

/// Reads the market and the maker's window and builds a resumed service. Every start is a
/// restart as far as the issuer knows, so it always goes through `NonceIssuer::resume`.
///
/// # Errors
///
/// The chain reads; `Maker` for settings the series rejects.
pub fn prepare(
    rpc: &mut dyn Rpc,
    settings: &MakerSettings,
    key: Keypair,
    time: Box<dyn TimeSource>,
) -> Result<Prepared, CliError> {
    let market = market::market(rpc, &settings.program_id, &settings.series)?;
    if settings.max_ttl_any_series < market.max_quote_ttl {
        return Err(CliError::Maker(
            "max_ttl_any_series is below the series' max_quote_ttl".into(),
        ));
    }
    let window = market::maker_state(rpc, &settings.program_id, &key.pubkey())?.nonces;
    let started_at = clock(rpc)?;
    let issuer = NonceIssuer::resume(&window, started_at, settings.max_ttl_any_series)
        .map_err(|e| CliError::Maker(e.to_string()))?;
    let until = started_at
        .get()
        .checked_add(settings.max_ttl_any_series.get())
        .and_then(|t| t.checked_add(MAX_CLOCK_SKEW.get()))
        .ok_or_else(|| CliError::Maker("cold start overflows".into()))?;
    let config = ServiceConfig {
        market,
        pricing: settings.pricing,
        ttl: settings.ttl,
        max_size: settings.max_size,
        maker_base: settings.maker_base,
        maker_quote: settings.maker_quote,
    };
    let service = MakerService::new(
        MakerKey::new(key),
        config,
        issuer,
        Box::new(FixedMid(settings.mid)),
        Box::new(FixedInventory(Decimal::ZERO)),
        time,
    )
    .map_err(|e| CliError::Maker(e.to_string()))?;
    Ok(Prepared {
        service: Arc::new(service),
        cold_start_until: UnixTimestamp::new(until),
    })
}

/// Once the cluster clock has passed the cold start, reads the maker's window and refreshes
/// the service. Returns `false` (and does nothing) while the cold start is still running.
///
/// The clock is read before the window, so the window read happens at or after `read_at`.
///
/// # Errors
///
/// The chain reads; `Maker` if the issuer refuses the refresh.
pub fn refresh(rpc: &mut dyn Rpc, prepared: &Prepared) -> Result<bool, CliError> {
    let read_at = clock(rpc)?;
    if read_at < prepared.cold_start_until {
        return Ok(false);
    }
    let program_id = prepared.service.config().market.program_id;
    let window = market::maker_state(rpc, &program_id, &prepared.service.maker())?.nonces;
    prepared
        .service
        .refresh_window(&window, read_at)
        .map_err(|e| CliError::Maker(e.to_string()))?;
    Ok(true)
}
