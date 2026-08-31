//! Stellarcade Weekly Tournament Contract
//!
//! Runs a rolling 7-day tournament. Players pay an entry fee to register;
//! game contracts post scores. At week end, admin finalises the round and the
//! top-N players share the prize pool according to configured split ratios.
//!
//! ## Flow
//! 1. Admin `init`s then calls `open_round` to start a new week.
//! 2. Players call `enter` (fee deducted from their wallet).
//! 3. Authorised callers post scores via `post_score`.
//! 4. After deadline, admin calls `finalise_round` to distribute prizes.
#![no_std]
#![allow(unexpected_cfgs)]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token::TokenClient,
    Address, Env, Vec,
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
    NoActiveRound = 4,
    RoundAlreadyOpen = 5,
    RoundNotEnded = 6,
    AlreadyEntered = 7,
    NotEntered = 8,
    InvalidConfig = 9,
    Overflow = 10,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Token,
    EntryFee,
    PrizeRatiosBps,
    CurrentRound,
    RoundMeta(u32),
    PlayerScore(u32, Address),
    PlayerEntered(u32, Address),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoundMeta {
    pub round_id: u32,
    pub start_ts: u64,
    pub end_ts: u64,
    pub prize_pool: i128,
    pub participant_count: u32,
    pub finalised: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerScoreEntry {
    pub player: Address,
    pub score: u64,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct RoundOpened {
    #[topic]
    pub round_id: u32,
    pub end_ts: u64,
}

#[contractevent]
pub struct PlayerEntered {
    #[topic]
    pub round_id: u32,
    #[topic]
    pub player: Address,
    pub fee_paid: i128,
}

#[contractevent]
pub struct ScorePosted {
    #[topic]
    pub round_id: u32,
    #[topic]
    pub player: Address,
    pub score: u64,
}

#[contractevent]
pub struct RoundFinalised {
    #[topic]
    pub round_id: u32,
    pub total_prize: i128,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct WeeklyTournament;

#[contractimpl]
impl WeeklyTournament {
    pub fn init(
        env: Env,
        admin: Address,
        token: Address,
        entry_fee: i128,
        prize_ratios_bps: Vec<u32>,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        let total: u32 = prize_ratios_bps.iter().fold(0u32, |a, b| a.saturating_add(b));
        if total != 10_000 {
            return Err(Error::InvalidConfig);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::EntryFee, &entry_fee);
        env.storage()
            .instance()
            .set(&DataKey::PrizeRatiosBps, &prize_ratios_bps);
        Ok(())
    }

    pub fn open_round(env: Env, admin: Address, round_id: u32, duration_secs: u64) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if env.storage().instance().has(&DataKey::CurrentRound) {
            let current: u32 = env.storage().instance().get(&DataKey::CurrentRound).unwrap();
            let meta: RoundMeta = env
                .storage()
                .persistent()
                .get(&DataKey::RoundMeta(current))
                .ok_or(Error::NoActiveRound)?;
            if !meta.finalised {
                return Err(Error::RoundAlreadyOpen);
            }
        }

        let now = env.ledger().timestamp();
        let meta = RoundMeta {
            round_id,
            start_ts: now,
            end_ts: now.saturating_add(duration_secs),
            prize_pool: 0,
            participant_count: 0,
            finalised: false,
        };
        env.storage().instance().set(&DataKey::CurrentRound, &round_id);
        env.storage()
            .persistent()
            .set(&DataKey::RoundMeta(round_id), &meta);
        env.storage().persistent().extend_ttl(
            &DataKey::RoundMeta(round_id),
            PERSISTENT_BUMP_LEDGERS,
            PERSISTENT_BUMP_LEDGERS,
        );

        RoundOpened { round_id, end_ts: meta.end_ts }.publish(&env);
        Ok(())
    }

    pub fn enter(env: Env, player: Address) -> Result<(), Error> {
        require_initialized(&env)?;
        player.require_auth();

        let round_id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentRound)
            .ok_or(Error::NoActiveRound)?;

        let entered_key = DataKey::PlayerEntered(round_id, player.clone());
        if env.storage().persistent().has(&entered_key) {
            return Err(Error::AlreadyEntered);
        }

        let fee: i128 = env.storage().instance().get(&DataKey::EntryFee).unwrap();
        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        TokenClient::new(&env, &token).transfer(&player, &env.current_contract_address(), &fee);

        env.storage().persistent().set(&entered_key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&entered_key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        let mut meta: RoundMeta = env
            .storage()
            .persistent()
            .get(&DataKey::RoundMeta(round_id))
            .unwrap();
        meta.prize_pool = meta.prize_pool.checked_add(fee).ok_or(Error::Overflow)?;
        meta.participant_count = meta.participant_count.saturating_add(1);
        env.storage()
            .persistent()
            .set(&DataKey::RoundMeta(round_id), &meta);

        PlayerEntered { round_id, player, fee_paid: fee }.publish(&env);
        Ok(())
    }

    pub fn post_score(env: Env, caller: Address, player: Address, score: u64) -> Result<(), Error> {
        require_admin(&env, &caller)?;

        let round_id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentRound)
            .ok_or(Error::NoActiveRound)?;

        let entered_key = DataKey::PlayerEntered(round_id, player.clone());
        if !env.storage().persistent().has(&entered_key) {
            return Err(Error::NotEntered);
        }

        let score_key = DataKey::PlayerScore(round_id, player.clone());
        let existing: u64 = env
            .storage()
            .persistent()
            .get(&score_key)
            .unwrap_or(0u64);
        let new_score = core::cmp::max(existing, score);

        env.storage().persistent().set(&score_key, &new_score);
        env.storage()
            .persistent()
            .extend_ttl(&score_key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        ScorePosted { round_id, player, score: new_score }.publish(&env);
        Ok(())
    }

    /// Finalises the round by paying out top winners. Admin supplies an
    /// ordered list of winners (highest score first) matching the ratio count.
    pub fn finalise_round(
        env: Env,
        admin: Address,
        winners: Vec<Address>,
    ) -> Result<i128, Error> {
        require_admin(&env, &admin)?;

        let round_id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentRound)
            .ok_or(Error::NoActiveRound)?;

        let mut meta: RoundMeta = env
            .storage()
            .persistent()
            .get(&DataKey::RoundMeta(round_id))
            .ok_or(Error::NoActiveRound)?;

        if meta.finalised {
            return Err(Error::RoundNotEnded);
        }

        let ratios: Vec<u32> = env
            .storage()
            .instance()
            .get(&DataKey::PrizeRatiosBps)
            .unwrap();

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = TokenClient::new(&env, &token);
        let prize_pool = meta.prize_pool;

        let mut idx = 0u32;
        while idx < ratios.len() && idx < winners.len() {
            let ratio = ratios.get(idx).unwrap_or(0);
            let share = prize_pool
                .checked_mul(ratio as i128)
                .and_then(|v| v.checked_div(10_000))
                .ok_or(Error::Overflow)?;
            if share > 0 {
                let winner = winners.get(idx).unwrap();
                token_client.transfer(&env.current_contract_address(), &winner, &share);
            }
            idx += 1;
        }

        meta.finalised = true;
        env.storage()
            .persistent()
            .set(&DataKey::RoundMeta(round_id), &meta);

        RoundFinalised { round_id, total_prize: prize_pool }.publish(&env);
        Ok(prize_pool)
    }

    pub fn get_current_round(env: Env) -> Option<RoundMeta> {
        let round_id: Option<u32> = env.storage().instance().get(&DataKey::CurrentRound);
        round_id.and_then(|id| {
            env.storage()
                .persistent()
                .get(&DataKey::RoundMeta(id))
        })
    }

    pub fn get_player_score(env: Env, round_id: u32, player: Address) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::PlayerScore(round_id, player))
            .unwrap_or(0u64)
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
    use soroban_sdk::{testutils::Address as _, vec, Address, Env};

    #[test]
    fn init_and_open_round() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(WeeklyTournament, ());
        let client = WeeklyTournamentClient::new(&env, &id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        let ratios = vec![&env, 6_000u32, 3_000u32, 1_000u32];
        client.init(&admin, &token, &100i128, &ratios);
        client.open_round(&admin, &1u32, &604_800u64);

        let meta = client.get_current_round().unwrap();
        assert_eq!(meta.round_id, 1u32);
        assert!(!meta.finalised);
    }
}
