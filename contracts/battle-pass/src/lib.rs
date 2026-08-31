//! Stellarcade Battle Pass Contract
//!
//! Tracks player progress through a seasonal battle pass. Players complete
//! missions to earn XP and unlock tiers. Admins configure tiers and reward
//! thresholds; players claim tier rewards once per tier per season.
//!
//! ## Flow
//! 1. Admin calls `init` with season config, then `set_tier` for each tier.
//! 2. Authorised game contracts call `record_xp` to credit XP.
//! 3. Player calls `claim_tier_reward` once they meet the XP threshold.
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
    TierNotFound = 4,
    TierAlreadyClaimed = 5,
    InsufficientXp = 6,
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
    SeasonId,
    SeasonEndLedger,
    Tier(u32),
    PlayerXp(Address),
    PlayerClaimed(Address, u32),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierConfig {
    pub tier_id: u32,
    pub xp_required: u64,
    pub reward_amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerProgress {
    pub total_xp: u64,
    pub season_id: u32,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct XpRecorded {
    #[topic]
    pub player: Address,
    pub xp_added: u64,
    pub total_xp: u64,
}

#[contractevent]
pub struct TierRewardClaimed {
    #[topic]
    pub player: Address,
    #[topic]
    pub tier_id: u32,
    pub reward_amount: i128,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct BattlePass;

#[contractimpl]
impl BattlePass {
    pub fn init(
        env: Env,
        admin: Address,
        token: Address,
        season_id: u32,
        season_end_ledger: u32,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::SeasonId, &season_id);
        env.storage()
            .instance()
            .set(&DataKey::SeasonEndLedger, &season_end_ledger);
        Ok(())
    }

    /// Upsert a tier definition. Admin only.
    pub fn set_tier(env: Env, admin: Address, tier: TierConfig) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::Tier(tier.tier_id), &tier);
        Ok(())
    }

    /// Authorised caller credits XP to a player. The caller must be the admin
    /// or a pre-registered game contract (admin acts as proxy here).
    pub fn record_xp(
        env: Env,
        caller: Address,
        player: Address,
        xp: u64,
    ) -> Result<PlayerProgress, Error> {
        require_admin(&env, &caller)?;
        let season_id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::SeasonId)
            .ok_or(Error::NotInitialized)?;

        let key = DataKey::PlayerXp(player.clone());
        let mut progress: PlayerProgress = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(PlayerProgress { total_xp: 0, season_id });

        if progress.season_id != season_id {
            progress.total_xp = 0;
            progress.season_id = season_id;
        }
        progress.total_xp = progress.total_xp.checked_add(xp).ok_or(Error::Overflow)?;

        env.storage().persistent().set(&key, &progress);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        XpRecorded {
            player,
            xp_added: xp,
            total_xp: progress.total_xp,
        }
        .publish(&env);

        Ok(progress)
    }

    /// Player claims the reward for a tier they have unlocked.
    pub fn claim_tier_reward(
        env: Env,
        player: Address,
        tier_id: u32,
    ) -> Result<i128, Error> {
        require_initialized(&env)?;
        player.require_auth();

        let tier: TierConfig = env
            .storage()
            .instance()
            .get(&DataKey::Tier(tier_id))
            .ok_or(Error::TierNotFound)?;

        let season_id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::SeasonId)
            .unwrap();

        let claimed_key = DataKey::PlayerClaimed(player.clone(), tier_id);
        if env.storage().persistent().has(&claimed_key) {
            return Err(Error::TierAlreadyClaimed);
        }

        let xp_key = DataKey::PlayerXp(player.clone());
        let progress: PlayerProgress = env
            .storage()
            .persistent()
            .get(&xp_key)
            .unwrap_or(PlayerProgress { total_xp: 0, season_id });

        if progress.total_xp < tier.xp_required {
            return Err(Error::InsufficientXp);
        }

        // Mark claimed before transfer
        env.storage().persistent().set(&claimed_key, &true);
        env.storage().persistent().extend_ttl(
            &claimed_key,
            PERSISTENT_BUMP_LEDGERS,
            PERSISTENT_BUMP_LEDGERS,
        );

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        TokenClient::new(&env, &token).transfer(
            &env.current_contract_address(),
            &player,
            &tier.reward_amount,
        );

        TierRewardClaimed {
            player,
            tier_id,
            reward_amount: tier.reward_amount,
        }
        .publish(&env);

        Ok(tier.reward_amount)
    }

    pub fn get_player_progress(env: Env, player: Address) -> PlayerProgress {
        let season_id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::SeasonId)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .get(&DataKey::PlayerXp(player))
            .unwrap_or(PlayerProgress { total_xp: 0, season_id })
    }

    pub fn get_tier(env: Env, tier_id: u32) -> Result<TierConfig, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Tier(tier_id))
            .ok_or(Error::TierNotFound)
    }

    pub fn has_claimed(env: Env, player: Address, tier_id: u32) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::PlayerClaimed(player, tier_id))
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
    fn init_and_set_tier() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(BattlePass, ());
        let client = BattlePassClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.init(&admin, &token, &1u32, &100_000u32);

        client.set_tier(
            &admin,
            &TierConfig {
                tier_id: 1,
                xp_required: 500,
                reward_amount: 100,
            },
        );

        let tier = client.get_tier(&1u32);
        assert_eq!(tier.xp_required, 500);
    }

    #[test]
    fn record_xp_accumulates() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(BattlePass, ());
        let client = BattlePassClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let player = Address::generate(&env);
        let token = Address::generate(&env);
        client.init(&admin, &token, &1u32, &100_000u32);

        client.record_xp(&admin, &player, &300u64);
        client.record_xp(&admin, &player, &250u64);
        let progress = client.get_player_progress(&player);
        assert_eq!(progress.total_xp, 550);
    }
}
