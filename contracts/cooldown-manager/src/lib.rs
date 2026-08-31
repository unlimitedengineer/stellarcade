//! Stellarcade Cooldown Manager Contract
//!
//! Shared cooldown enforcement across multiple game contracts. Each game
//! has a registered cooldown window (in seconds); after a player acts, they
//! must wait before acting again in the same game. Game contracts call
//! `check_and_consume` which atomically verifies and records the usage.
//!
//! ## Flow
//! 1. Admin `init`s the contract.
//! 2. Admin registers cooldown windows per game ID via `register_game`.
//! 3. Game contracts call `check_and_consume` before accepting a player action.
//! 4. Admins can update cooldown windows without affecting in-flight cooldowns.
#![no_std]
#![allow(unexpected_cfgs)]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Symbol,
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
    GameNotRegistered = 4,
    CooldownActive = 5,
    InvalidDuration = 6,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    GameCooldown(Symbol),
    PlayerCooldown(Symbol, Address),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CooldownState {
    pub last_action_ts: u64,
    pub action_count: u32,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct CooldownConsumed {
    #[topic]
    pub game_id: Symbol,
    #[topic]
    pub player: Address,
    pub next_available_ts: u64,
}

#[contractevent]
pub struct GameRegistered {
    #[topic]
    pub game_id: Symbol,
    pub cooldown_secs: u64,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct CooldownManager;

#[contractimpl]
impl CooldownManager {
    pub fn init(env: Env, admin: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Admin registers or updates a game's cooldown window.
    pub fn register_game(
        env: Env,
        admin: Address,
        game_id: Symbol,
        cooldown_secs: u64,
    ) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if cooldown_secs == 0 {
            return Err(Error::InvalidDuration);
        }
        env.storage()
            .instance()
            .set(&DataKey::GameCooldown(game_id.clone()), &cooldown_secs);

        GameRegistered { game_id, cooldown_secs }.publish(&env);
        Ok(())
    }

    /// Game contract checks if a player can act and records the action atomically.
    /// Caller must be authorised by admin (pass the game contract address as `caller`
    /// and have it auth'd via `require_auth`; admin acts as allow-list proxy).
    pub fn check_and_consume(
        env: Env,
        caller: Address,
        game_id: Symbol,
        player: Address,
    ) -> Result<u64, Error> {
        require_admin(&env, &caller)?;

        let cooldown_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::GameCooldown(game_id.clone()))
            .ok_or(Error::GameNotRegistered)?;

        let now = env.ledger().timestamp();
        let key = DataKey::PlayerCooldown(game_id.clone(), player.clone());

        let state: Option<CooldownState> = env.storage().persistent().get(&key);
        if let Some(ref s) = state {
            let elapsed = now.saturating_sub(s.last_action_ts);
            if elapsed < cooldown_secs {
                return Err(Error::CooldownActive);
            }
        }

        let action_count = state.as_ref().map(|s| s.action_count).unwrap_or(0);
        let new_state = CooldownState {
            last_action_ts: now,
            action_count: action_count.saturating_add(1),
        };

        env.storage().persistent().set(&key, &new_state);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        let next_available_ts = now.saturating_add(cooldown_secs);
        CooldownConsumed {
            game_id,
            player,
            next_available_ts,
        }
        .publish(&env);

        Ok(next_available_ts)
    }

    /// Returns the timestamp when the player can next act, or 0 if no cooldown is active.
    pub fn get_next_available(env: Env, game_id: Symbol, player: Address) -> u64 {
        let cooldown_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::GameCooldown(game_id.clone()))
            .unwrap_or(0u64);

        let key = DataKey::PlayerCooldown(game_id, player);
        let state: Option<CooldownState> = env.storage().persistent().get(&key);

        match state {
            Some(s) => {
                let next = s.last_action_ts.saturating_add(cooldown_secs);
                let now = env.ledger().timestamp();
                if next > now { next } else { 0 }
            }
            None => 0,
        }
    }

    pub fn get_action_count(env: Env, game_id: Symbol, player: Address) -> u32 {
        env.storage()
            .persistent()
            .get::<DataKey, CooldownState>(&DataKey::PlayerCooldown(game_id, player))
            .map(|s| s.action_count)
            .unwrap_or(0u32)
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
    use soroban_sdk::{testutils::Address as _, Address, Env, Symbol};

    #[test]
    fn register_and_consume_cooldown() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(CooldownManager, ());
        let client = CooldownManagerClient::new(&env, &id);

        let admin = Address::generate(&env);
        client.init(&admin);

        let game_id = Symbol::new(&env, "COIN_FLIP");
        client.register_game(&admin, &game_id, &60u64);

        let player = Address::generate(&env);
        let next = client.check_and_consume(&admin, &game_id, &player);
        assert!(next > 0u64);
    }

    #[test]
    fn second_consume_within_cooldown_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(CooldownManager, ());
        let client = CooldownManagerClient::new(&env, &id);

        let admin = Address::generate(&env);
        client.init(&admin);

        let game_id = Symbol::new(&env, "DICE_ROLL");
        client.register_game(&admin, &game_id, &3600u64);

        let player = Address::generate(&env);
        client.check_and_consume(&admin, &game_id, &player);

        let result = client.try_check_and_consume(&admin, &game_id, &player);
        assert!(result.is_err());
    }
}
