//! Stellarcade Wager Limits Contract
//!
//! Enforces per-player wager caps across game contracts. Each player has a
//! rolling 24-hour limit and a per-session limit. Game contracts check
//! `can_wager` before accepting a bet and call `record_wager` after.
//!
//! Admin can adjust the global defaults and override limits for individual
//! players (e.g., VIP raised limits or self-exclusion).
#![no_std]
#![allow(unexpected_cfgs)]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env,
};

pub const PERSISTENT_BUMP_LEDGERS: u32 = 518_400;
/// One day in seconds (approximate — used with ledger timestamps).
pub const WINDOW_SECS: u64 = 86_400;

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
    DailyLimitExceeded = 4,
    PerBetLimitExceeded = 5,
    InvalidAmount = 6,
    Overflow = 7,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    DefaultDailyLimit,
    DefaultPerBetLimit,
    PlayerOverride(Address),
    PlayerWindow(Address),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitOverride {
    pub daily_limit: i128,
    pub per_bet_limit: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerWindowState {
    pub window_start_ts: u64,
    pub wagered_in_window: i128,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct WagerRecorded {
    #[topic]
    pub player: Address,
    pub amount: i128,
    pub daily_total: i128,
}

#[contractevent]
pub struct LimitOverrideSet {
    #[topic]
    pub player: Address,
    pub daily_limit: i128,
    pub per_bet_limit: i128,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct WagerLimits;

#[contractimpl]
impl WagerLimits {
    pub fn init(
        env: Env,
        admin: Address,
        default_daily_limit: i128,
        default_per_bet_limit: i128,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::DefaultDailyLimit, &default_daily_limit);
        env.storage()
            .instance()
            .set(&DataKey::DefaultPerBetLimit, &default_per_bet_limit);
        Ok(())
    }

    /// Set a per-player limit override (admin only).
    pub fn set_player_override(
        env: Env,
        admin: Address,
        player: Address,
        daily_limit: i128,
        per_bet_limit: i128,
    ) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        let override_val = LimitOverride { daily_limit, per_bet_limit };
        let key = DataKey::PlayerOverride(player.clone());
        env.storage().persistent().set(&key, &override_val);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        LimitOverrideSet {
            player,
            daily_limit,
            per_bet_limit,
        }
        .publish(&env);
        Ok(())
    }

    /// Returns `Ok(())` if the player may place this wager, otherwise an error.
    pub fn can_wager(env: Env, player: Address, amount: i128) -> Result<(), Error> {
        require_initialized(&env)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let (daily_limit, per_bet_limit) = get_limits(&env, &player);

        if amount > per_bet_limit {
            return Err(Error::PerBetLimitExceeded);
        }

        let now = env.ledger().timestamp();
        let window_state = get_window(&env, &player, now);

        let new_total = window_state
            .wagered_in_window
            .checked_add(amount)
            .ok_or(Error::Overflow)?;
        if new_total > daily_limit {
            return Err(Error::DailyLimitExceeded);
        }

        Ok(())
    }

    /// Called by an authorised game contract after a bet is accepted.
    pub fn record_wager(
        env: Env,
        caller: Address,
        player: Address,
        amount: i128,
    ) -> Result<i128, Error> {
        require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let now = env.ledger().timestamp();
        let mut state = get_window(&env, &player, now);
        state.wagered_in_window = state
            .wagered_in_window
            .checked_add(amount)
            .ok_or(Error::Overflow)?;

        let key = DataKey::PlayerWindow(player.clone());
        env.storage().persistent().set(&key, &state);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        let daily_total = state.wagered_in_window;
        WagerRecorded {
            player,
            amount,
            daily_total,
        }
        .publish(&env);

        Ok(daily_total)
    }

    pub fn get_player_daily_total(env: Env, player: Address) -> i128 {
        let now = env.ledger().timestamp();
        get_window(&env, &player, now).wagered_in_window
    }

    pub fn get_limits_for(env: Env, player: Address) -> (i128, i128) {
        get_limits(&env, &player)
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

fn require_admin(env: &Env, caller: &Address) -> Result<(), Error> {
    require_initialized(env)?;
    caller.require_auth();
    let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
    if *caller != admin {
        return Err(Error::NotAuthorized);
    }
    Ok(())
}

fn get_limits(env: &Env, player: &Address) -> (i128, i128) {
    if let Some(ov) = env
        .storage()
        .persistent()
        .get::<DataKey, LimitOverride>(&DataKey::PlayerOverride(player.clone()))
    {
        return (ov.daily_limit, ov.per_bet_limit);
    }
    let daily: i128 = env
        .storage()
        .instance()
        .get(&DataKey::DefaultDailyLimit)
        .unwrap_or(10_000);
    let per_bet: i128 = env
        .storage()
        .instance()
        .get(&DataKey::DefaultPerBetLimit)
        .unwrap_or(1_000);
    (daily, per_bet)
}

fn get_window(env: &Env, player: &Address, now: u64) -> PlayerWindowState {
    let key = DataKey::PlayerWindow(player.clone());
    let state: Option<PlayerWindowState> = env.storage().persistent().get(&key);
    match state {
        Some(s) if now.saturating_sub(s.window_start_ts) < WINDOW_SECS => s,
        _ => PlayerWindowState {
            window_start_ts: now,
            wagered_in_window: 0,
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    #[test]
    fn can_wager_within_limits() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(WagerLimits, ());
        let client = WagerLimitsClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.init(&admin, &10_000i128, &1_000i128);

        let player = Address::generate(&env);
        client.can_wager(&player, &500i128);
    }

    #[test]
    fn rejects_over_per_bet_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(WagerLimits, ());
        let client = WagerLimitsClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.init(&admin, &10_000i128, &1_000i128);

        let player = Address::generate(&env);
        let result = client.try_can_wager(&player, &2_000i128);
        assert!(result.is_err());
    }
}
