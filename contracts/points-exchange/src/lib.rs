//! Stellarcade Points Exchange Contract
//!
//! Converts accumulated off-chain game points into on-chain XLM tokens.
//! Admin loads a funding allocation; players redeem points at a configured
//! exchange rate. Rate can be updated by admin to manage token supply.
//!
//! ## Flow
//! 1. Admin `init`s with token address and initial rate.
//! 2. Admin calls `fund` to deposit XLM into the contract.
//! 3. Authorised callers `record_points` for players (game contracts proxy via admin).
//! 4. Players call `redeem` to convert points to XLM.
#![no_std]
#![allow(unexpected_cfgs)]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token::TokenClient,
    Address, Env,
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
    InsufficientPoints = 4,
    InsufficientFunds = 5,
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
    Token,
    /// Points required to receive 1 token unit (stroops).
    PointsPerToken,
    PlayerPoints(Address),
    TotalRedeemed,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct PointsRecorded {
    #[topic]
    pub player: Address,
    pub points_added: u64,
    pub total_points: u64,
}

#[contractevent]
pub struct PointsRedeemed {
    #[topic]
    pub player: Address,
    pub points_spent: u64,
    pub tokens_received: i128,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct PointsExchange;

#[contractimpl]
impl PointsExchange {
    pub fn init(
        env: Env,
        admin: Address,
        token: Address,
        points_per_token: u64,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        if points_per_token == 0 {
            return Err(Error::InvalidAmount);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage()
            .instance()
            .set(&DataKey::PointsPerToken, &points_per_token);
        Ok(())
    }

    /// Admin deposits tokens into the exchange pool.
    pub fn fund(env: Env, admin: Address, amount: i128) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        TokenClient::new(&env, &token).transfer(&admin, &env.current_contract_address(), &amount);
        Ok(())
    }

    /// Update the exchange rate (admin only).
    pub fn set_rate(env: Env, admin: Address, points_per_token: u64) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if points_per_token == 0 {
            return Err(Error::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::PointsPerToken, &points_per_token);
        Ok(())
    }

    /// Authorised caller credits points to a player.
    pub fn record_points(
        env: Env,
        caller: Address,
        player: Address,
        points: u64,
    ) -> Result<u64, Error> {
        require_admin(&env, &caller)?;
        let key = DataKey::PlayerPoints(player.clone());
        let current: u64 = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(0u64);
        let new_total = current.checked_add(points).ok_or(Error::Overflow)?;
        env.storage().persistent().set(&key, &new_total);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        PointsRecorded {
            player,
            points_added: points,
            total_points: new_total,
        }
        .publish(&env);

        Ok(new_total)
    }

    /// Player redeems `points` for tokens at the current rate.
    pub fn redeem(env: Env, player: Address, points: u64) -> Result<i128, Error> {
        require_initialized(&env)?;
        player.require_auth();

        if points == 0 {
            return Err(Error::InvalidAmount);
        }

        let key = DataKey::PlayerPoints(player.clone());
        let balance: u64 = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(0u64);

        if balance < points {
            return Err(Error::InsufficientPoints);
        }

        let rate: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PointsPerToken)
            .unwrap();

        let token_units = (points / rate) as i128;
        if token_units == 0 {
            return Err(Error::InvalidAmount);
        }

        // Deduct before transfer
        let new_balance = balance.saturating_sub(points);
        env.storage().persistent().set(&key, &new_balance);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        let total_redeemed: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalRedeemed)
            .unwrap_or(0i128);
        env.storage().instance().set(
            &DataKey::TotalRedeemed,
            &total_redeemed.checked_add(token_units).ok_or(Error::Overflow)?,
        );

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        TokenClient::new(&env, &token).transfer(
            &env.current_contract_address(),
            &player,
            &token_units,
        );

        PointsRedeemed {
            player,
            points_spent: points,
            tokens_received: token_units,
        }
        .publish(&env);

        Ok(token_units)
    }

    pub fn get_player_points(env: Env, player: Address) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::PlayerPoints(player))
            .unwrap_or(0u64)
    }

    pub fn get_rate(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PointsPerToken)
            .unwrap_or(0u64)
    }

    pub fn get_total_redeemed(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalRedeemed)
            .unwrap_or(0i128)
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

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    #[test]
    fn record_and_check_points() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(PointsExchange, ());
        let client = PointsExchangeClient::new(&env, &id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        let player = Address::generate(&env);

        client.init(&admin, &token, &100u64);
        client.record_points(&admin, &player, &500u64);
        assert_eq!(client.get_player_points(&player), 500u64);
    }

    #[test]
    fn get_rate_returns_configured_value() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(PointsExchange, ());
        let client = PointsExchangeClient::new(&env, &id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.init(&admin, &token, &200u64);
        assert_eq!(client.get_rate(), 200u64);
    }
}
