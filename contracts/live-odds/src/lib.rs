//! Stellarcade Live Odds Contract
//!
//! Stores and serves real-time odds for game events. An authorised oracle
//! pushes odds updates; players and frontends read them. Supports multiple
//! game types with per-outcome odds expressed in basis points of implied
//! probability (e.g., 5000 bps = 50%).
//!
//! ## Flow
//! 1. Admin `init`s the contract.
//! 2. Oracle calls `post_odds` with updated implied-probability bps per outcome.
//! 3. Frontends call `get_odds` to display live lines.
#![no_std]
#![allow(unexpected_cfgs)]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Symbol, Vec,
};

pub const PERSISTENT_BUMP_LEDGERS: u32 = 518_400;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAuthorized = 3,
    MarketNotFound = 4,
    InvalidOdds = 5,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Oracle,
    Market(Symbol),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OddsEntry {
    pub outcome_label: Symbol,
    /// Implied probability expressed in basis points (0–10_000).
    pub implied_prob_bps: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarketOdds {
    pub market_id: Symbol,
    pub outcomes: Vec<OddsEntry>,
    pub last_updated_ts: u64,
    pub is_open: bool,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct OddsPosted {
    #[topic]
    pub market_id: Symbol,
    pub last_updated_ts: u64,
}

#[contractevent]
pub struct MarketClosed {
    #[topic]
    pub market_id: Symbol,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct LiveOdds;

#[contractimpl]
impl LiveOdds {
    pub fn init(env: Env, admin: Address, oracle: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        Ok(())
    }

    /// Oracle posts or updates odds for a market.
    pub fn post_odds(
        env: Env,
        oracle: Address,
        market_id: Symbol,
        outcomes: Vec<OddsEntry>,
    ) -> Result<(), Error> {
        require_oracle(&env, &oracle)?;

        // Validate probabilities sum ≤ 10_000 (over-round is fine; under-round is not)
        let total: u32 = outcomes.iter().fold(0u32, |a, e| a.saturating_add(e.implied_prob_bps));
        if total == 0 || total > 20_000 {
            return Err(Error::InvalidOdds);
        }

        let now = env.ledger().timestamp();
        let market = MarketOdds {
            market_id: market_id.clone(),
            outcomes,
            last_updated_ts: now,
            is_open: true,
        };

        let key = DataKey::Market(market_id.clone());
        env.storage().persistent().set(&key, &market);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        OddsPosted { market_id, last_updated_ts: now }.publish(&env);
        Ok(())
    }

    /// Admin or oracle closes a market (no more odds updates, results pending).
    pub fn close_market(env: Env, caller: Address, market_id: Symbol) -> Result<(), Error> {
        require_initialized(&env)?;
        caller.require_auth();
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let oracle: Address = env.storage().instance().get(&DataKey::Oracle).unwrap();
        if caller != admin && caller != oracle {
            return Err(Error::NotAuthorized);
        }

        let key = DataKey::Market(market_id.clone());
        let mut market: MarketOdds = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::MarketNotFound)?;

        market.is_open = false;
        env.storage().persistent().set(&key, &market);

        MarketClosed { market_id }.publish(&env);
        Ok(())
    }

    pub fn get_odds(env: Env, market_id: Symbol) -> Result<MarketOdds, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Market(market_id))
            .ok_or(Error::MarketNotFound)
    }

    pub fn get_oracle(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Oracle)
            .expect("LiveOdds: oracle not set")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_initialized(env: &Env) -> Result<(), Error> {
    if !env.storage().instance().has(&DataKey::Admin) {
        return Err(Error::NotInitialized);
    }
    Ok(())
}

fn require_oracle(env: &Env, caller: &Address) -> Result<(), Error> {
    require_initialized(env)?;
    caller.require_auth();
    let oracle: Address = env.storage().instance().get(&DataKey::Oracle).unwrap();
    if *caller != oracle {
        return Err(Error::NotAuthorized);
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Address, Env, Symbol};

    #[test]
    fn post_and_get_odds() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LiveOdds, ());
        let client = LiveOddsClient::new(&env, &id);

        let admin = Address::generate(&env);
        let oracle = Address::generate(&env);
        client.init(&admin, &oracle);

        let market_id = Symbol::new(&env, "COIN_FLIP");
        let outcomes = vec![
            &env,
            OddsEntry {
                outcome_label: Symbol::new(&env, "HEADS"),
                implied_prob_bps: 5000u32,
            },
            OddsEntry {
                outcome_label: Symbol::new(&env, "TAILS"),
                implied_prob_bps: 5000u32,
            },
        ];

        client.post_odds(&oracle, &market_id, &outcomes);
        let market = client.get_odds(&market_id);
        assert!(market.is_open);
        assert_eq!(market.outcomes.len(), 2);
    }
}
