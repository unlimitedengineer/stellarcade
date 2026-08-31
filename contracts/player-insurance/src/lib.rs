//! Stellarcade Player Insurance Contract
//!
//! A loss-protection pool that partially reimburses players after a losing
//! streak. Players pay a small insurance premium when placing bets (via the
//! game contract); if they accumulate enough consecutive losses, they can
//! claim a reimbursement from the pool.
//!
//! ## Flow
//! 1. Admin `init`s with token and policy parameters.
//! 2. Game contracts call `record_loss` / `record_win` after each round.
//! 3. Players call `claim_reimbursement` once their streak qualifies.
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
    StreakNotQualified = 4,
    AlreadyClaimed = 5,
    InsufficientPool = 6,
    InvalidAmount = 7,
    Overflow = 8,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Token,
    /// Minimum consecutive losses to qualify for a claim.
    MinLossStreak,
    /// Reimbursement amount per qualifying claim (in token units).
    ReimbursementAmount,
    PlayerRecord(Address),
    /// Rolling count of claims paid out.
    TotalClaimsPaid,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerInsuranceRecord {
    pub consecutive_losses: u32,
    pub total_losses: u32,
    pub total_wins: u32,
    pub last_claim_streak_end: u32,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct LossRecorded {
    #[topic]
    pub player: Address,
    pub consecutive_losses: u32,
}

#[contractevent]
pub struct WinRecorded {
    #[topic]
    pub player: Address,
}

#[contractevent]
pub struct ReimbursementClaimed {
    #[topic]
    pub player: Address,
    pub amount: i128,
    pub at_streak: u32,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct PlayerInsurance;

#[contractimpl]
impl PlayerInsurance {
    pub fn init(
        env: Env,
        admin: Address,
        token: Address,
        min_loss_streak: u32,
        reimbursement_amount: i128,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage()
            .instance()
            .set(&DataKey::MinLossStreak, &min_loss_streak);
        env.storage()
            .instance()
            .set(&DataKey::ReimbursementAmount, &reimbursement_amount);
        Ok(())
    }

    /// Authorised caller records a loss for a player.
    pub fn record_loss(
        env: Env,
        caller: Address,
        player: Address,
    ) -> Result<u32, Error> {
        require_admin(&env, &caller)?;

        let key = DataKey::PlayerRecord(player.clone());
        let mut rec: PlayerInsuranceRecord = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(PlayerInsuranceRecord {
                consecutive_losses: 0,
                total_losses: 0,
                total_wins: 0,
                last_claim_streak_end: 0,
            });

        rec.consecutive_losses = rec
            .consecutive_losses
            .checked_add(1)
            .ok_or(Error::Overflow)?;
        rec.total_losses = rec.total_losses.saturating_add(1);

        env.storage().persistent().set(&key, &rec);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        let streak = rec.consecutive_losses;
        LossRecorded { player, consecutive_losses: streak }.publish(&env);

        Ok(streak)
    }

    /// Authorised caller records a win for a player (resets loss streak).
    pub fn record_win(env: Env, caller: Address, player: Address) -> Result<(), Error> {
        require_admin(&env, &caller)?;

        let key = DataKey::PlayerRecord(player.clone());
        let mut rec: PlayerInsuranceRecord = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(PlayerInsuranceRecord {
                consecutive_losses: 0,
                total_losses: 0,
                total_wins: 0,
                last_claim_streak_end: 0,
            });

        rec.consecutive_losses = 0;
        rec.total_wins = rec.total_wins.saturating_add(1);

        env.storage().persistent().set(&key, &rec);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        WinRecorded { player }.publish(&env);
        Ok(())
    }

    /// Player claims reimbursement once their loss streak meets the threshold.
    pub fn claim_reimbursement(env: Env, player: Address) -> Result<i128, Error> {
        require_initialized(&env)?;
        player.require_auth();

        let min_streak: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MinLossStreak)
            .unwrap();
        let amount: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReimbursementAmount)
            .unwrap();

        let key = DataKey::PlayerRecord(player.clone());
        let mut rec: PlayerInsuranceRecord = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::StreakNotQualified)?;

        if rec.consecutive_losses < min_streak {
            return Err(Error::StreakNotQualified);
        }
        if rec.last_claim_streak_end >= rec.consecutive_losses {
            return Err(Error::AlreadyClaimed);
        }

        rec.last_claim_streak_end = rec.consecutive_losses;
        env.storage().persistent().set(&key, &rec);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        let paid: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TotalClaimsPaid)
            .unwrap_or(0u32);
        env.storage()
            .instance()
            .set(&DataKey::TotalClaimsPaid, &paid.saturating_add(1));

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        TokenClient::new(&env, &token).transfer(
            &env.current_contract_address(),
            &player,
            &amount,
        );

        ReimbursementClaimed {
            player,
            amount,
            at_streak: rec.last_claim_streak_end,
        }
        .publish(&env);

        Ok(amount)
    }

    pub fn get_player_record(
        env: Env,
        player: Address,
    ) -> Option<PlayerInsuranceRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::PlayerRecord(player))
    }

    pub fn get_total_claims_paid(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TotalClaimsPaid)
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
    use soroban_sdk::{testutils::Address as _, Address, Env};

    #[test]
    fn loss_streak_increments() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(PlayerInsurance, ());
        let client = PlayerInsuranceClient::new(&env, &id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        let player = Address::generate(&env);

        client.init(&admin, &token, &3u32, &500i128);
        client.record_loss(&admin, &player);
        client.record_loss(&admin, &player);
        let streak = client.record_loss(&admin, &player);
        assert_eq!(streak, 3u32);
    }

    #[test]
    fn win_resets_streak() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(PlayerInsurance, ());
        let client = PlayerInsuranceClient::new(&env, &id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        let player = Address::generate(&env);

        client.init(&admin, &token, &3u32, &500i128);
        client.record_loss(&admin, &player);
        client.record_loss(&admin, &player);
        client.record_win(&admin, &player);

        let rec = client.get_player_record(&player).unwrap();
        assert_eq!(rec.consecutive_losses, 0u32);
    }
}
