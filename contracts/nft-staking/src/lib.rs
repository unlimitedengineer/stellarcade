//! Stellarcade NFT Staking Contract
//!
//! Players stake NFT token IDs (represented as u64 asset IDs from the
//! Stellar token interface) to accumulate yield over time. The yield rate
//! is expressed in reward-token units per staked NFT per ledger second.
//! Players can stake multiple NFTs, unstake individually, and claim accrued
//! rewards at any time.
//!
//! ## Flow
//! 1. Admin `init`s with NFT contract, reward token, and yield rate.
//! 2. Players call `stake` to lock an NFT and begin earning.
//! 3. Players call `claim_rewards` to collect accrued yield (NFT stays staked).
//! 4. Players call `unstake` to withdraw the NFT and claim remaining rewards.
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
    NftAlreadyStaked = 4,
    NftNotStaked = 5,
    NotOwner = 6,
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
    NftContract,
    RewardToken,
    YieldRatePerSecond,
    StakeRecord(u64),
    PlayerStakes(Address),
    TotalStaked,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakeRecord {
    pub owner: Address,
    pub nft_id: u64,
    pub staked_at_ts: u64,
    pub last_claim_ts: u64,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct NftStaked {
    #[topic]
    pub player: Address,
    #[topic]
    pub nft_id: u64,
}

#[contractevent]
pub struct NftUnstaked {
    #[topic]
    pub player: Address,
    #[topic]
    pub nft_id: u64,
    pub rewards_claimed: i128,
}

#[contractevent]
pub struct RewardsClaimed {
    #[topic]
    pub player: Address,
    pub amount: i128,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct NftStaking;

#[contractimpl]
impl NftStaking {
    pub fn init(
        env: Env,
        admin: Address,
        nft_contract: Address,
        reward_token: Address,
        yield_rate_per_second: i128,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        if yield_rate_per_second <= 0 {
            return Err(Error::InvalidAmount);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::NftContract, &nft_contract);
        env.storage()
            .instance()
            .set(&DataKey::RewardToken, &reward_token);
        env.storage()
            .instance()
            .set(&DataKey::YieldRatePerSecond, &yield_rate_per_second);
        Ok(())
    }

    /// Player stakes an NFT. The NFT is transferred from the player to this contract.
    pub fn stake(env: Env, player: Address, nft_id: u64) -> Result<(), Error> {
        require_initialized(&env)?;
        player.require_auth();

        let stake_key = DataKey::StakeRecord(nft_id);
        if env.storage().persistent().has(&stake_key) {
            return Err(Error::NftAlreadyStaked);
        }

        // Transfer NFT to this contract (NFT treated as fungible token with amount=1)
        let nft_contract: Address = env
            .storage()
            .instance()
            .get(&DataKey::NftContract)
            .unwrap();
        TokenClient::new(&env, &nft_contract).transfer(
            &player,
            &env.current_contract_address(),
            &1i128,
        );

        let now = env.ledger().timestamp();
        let record = StakeRecord {
            owner: player.clone(),
            nft_id,
            staked_at_ts: now,
            last_claim_ts: now,
        };

        env.storage().persistent().set(&stake_key, &record);
        env.storage()
            .persistent()
            .extend_ttl(&stake_key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        // Track player's staked NFT list
        let list_key = DataKey::PlayerStakes(player.clone());
        let mut list: Vec<u64> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or(Vec::new(&env));
        list.push_back(nft_id);
        env.storage().persistent().set(&list_key, &list);
        env.storage()
            .persistent()
            .extend_ttl(&list_key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        let total: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TotalStaked)
            .unwrap_or(0u32);
        env.storage()
            .instance()
            .set(&DataKey::TotalStaked, &total.saturating_add(1));

        NftStaked { player, nft_id }.publish(&env);
        Ok(())
    }

    /// Player claims accrued yield for a staked NFT without unstaking.
    pub fn claim_rewards(env: Env, player: Address, nft_id: u64) -> Result<i128, Error> {
        require_initialized(&env)?;
        player.require_auth();

        let stake_key = DataKey::StakeRecord(nft_id);
        let mut record: StakeRecord = env
            .storage()
            .persistent()
            .get(&stake_key)
            .ok_or(Error::NftNotStaked)?;

        if record.owner != player {
            return Err(Error::NotOwner);
        }

        let now = env.ledger().timestamp();
        let rewards = compute_rewards(&env, &record, now)?;

        record.last_claim_ts = now;
        env.storage().persistent().set(&stake_key, &record);
        env.storage()
            .persistent()
            .extend_ttl(&stake_key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        if rewards > 0 {
            let reward_token: Address = env
                .storage()
                .instance()
                .get(&DataKey::RewardToken)
                .unwrap();
            TokenClient::new(&env, &reward_token).transfer(
                &env.current_contract_address(),
                &player,
                &rewards,
            );
        }

        RewardsClaimed { player, amount: rewards }.publish(&env);
        Ok(rewards)
    }

    /// Player unstakes an NFT, claiming all remaining rewards and returning the NFT.
    pub fn unstake(env: Env, player: Address, nft_id: u64) -> Result<i128, Error> {
        require_initialized(&env)?;
        player.require_auth();

        let stake_key = DataKey::StakeRecord(nft_id);
        let record: StakeRecord = env
            .storage()
            .persistent()
            .get(&stake_key)
            .ok_or(Error::NftNotStaked)?;

        if record.owner != player {
            return Err(Error::NotOwner);
        }

        let now = env.ledger().timestamp();
        let rewards = compute_rewards(&env, &record, now)?;

        env.storage().persistent().remove(&stake_key);

        let list_key = DataKey::PlayerStakes(player.clone());
        let list: Vec<u64> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or(Vec::new(&env));
        let mut new_list = Vec::new(&env);
        for id in list.iter() {
            if id != nft_id {
                new_list.push_back(id);
            }
        }
        env.storage().persistent().set(&list_key, &new_list);

        let total: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TotalStaked)
            .unwrap_or(0u32);
        env.storage()
            .instance()
            .set(&DataKey::TotalStaked, &total.saturating_sub(1));

        let nft_contract: Address = env
            .storage()
            .instance()
            .get(&DataKey::NftContract)
            .unwrap();
        TokenClient::new(&env, &nft_contract).transfer(
            &env.current_contract_address(),
            &player,
            &1i128,
        );

        if rewards > 0 {
            let reward_token: Address = env
                .storage()
                .instance()
                .get(&DataKey::RewardToken)
                .unwrap();
            TokenClient::new(&env, &reward_token).transfer(
                &env.current_contract_address(),
                &player,
                &rewards,
            );
        }

        NftUnstaked { player, nft_id, rewards_claimed: rewards }.publish(&env);
        Ok(rewards)
    }

    pub fn get_stake(env: Env, nft_id: u64) -> Option<StakeRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::StakeRecord(nft_id))
    }

    pub fn get_player_stakes(env: Env, player: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::PlayerStakes(player))
            .unwrap_or(Vec::new(&env))
    }

    pub fn get_pending_rewards(env: Env, nft_id: u64) -> i128 {
        let record: Option<StakeRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::StakeRecord(nft_id));
        match record {
            Some(r) => {
                let now = env.ledger().timestamp();
                compute_rewards(&env, &r, now).unwrap_or(0)
            }
            None => 0,
        }
    }

    pub fn get_total_staked(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TotalStaked)
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

fn compute_rewards(env: &Env, record: &StakeRecord, now: u64) -> Result<i128, Error> {
    let elapsed = now.saturating_sub(record.last_claim_ts) as i128;
    let rate: i128 = env
        .storage()
        .instance()
        .get(&DataKey::YieldRatePerSecond)
        .unwrap_or(0i128);
    elapsed.checked_mul(rate).ok_or(Error::Overflow)
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    #[test]
    fn init_stores_config() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(NftStaking, ());
        let client = NftStakingClient::new(&env, &id);

        let admin = Address::generate(&env);
        let nft = Address::generate(&env);
        let reward = Address::generate(&env);
        client.init(&admin, &nft, &reward, &10i128);

        assert_eq!(client.get_total_staked(), 0u32);
    }

    #[test]
    fn pending_rewards_zero_before_stake() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(NftStaking, ());
        let client = NftStakingClient::new(&env, &id);

        let admin = Address::generate(&env);
        let nft = Address::generate(&env);
        let reward = Address::generate(&env);
        client.init(&admin, &nft, &reward, &10i128);

        assert_eq!(client.get_pending_rewards(&42u64), 0i128);
    }
}
