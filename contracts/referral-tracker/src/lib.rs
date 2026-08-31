//! Stellarcade Referral Tracker Contract
//!
//! Records multi-level referral chains on-chain. When a new player registers,
//! they supply a referrer address. The contract walks up to N levels and
//! attributes commission rewards to each level. Reward payouts are handled
//! by an external reward contract; this contract only tracks attribution.
//!
//! ## Flow
//! 1. Admin `init`s with commission rates per level (bps).
//! 2. New player calls `register` with their referrer (or zero address).
//! 3. Authorised callers call `attribute_revenue` to log a referral event.
//! 4. Callers read `get_referral_chain` to determine payout destinations.
#![no_std]
#![allow(unexpected_cfgs)]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Vec,
};

pub const PERSISTENT_BUMP_LEDGERS: u32 = 518_400;
pub const MAX_DEPTH: u32 = 5;

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
    AlreadyRegistered = 4,
    SelfReferral = 5,
    ReferrerNotRegistered = 6,
    InvalidConfig = 7,
    Overflow = 8,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    CommissionRatesBps,
    Referrer(Address),
    ReferralCount(Address),
    TotalAttributed,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferralChainEntry {
    pub address: Address,
    pub level: u32,
    pub commission_bps: u32,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct PlayerRegistered {
    #[topic]
    pub player: Address,
    pub referrer: Option<Address>,
}

#[contractevent]
pub struct RevenueAttributed {
    #[topic]
    pub player: Address,
    pub gross_amount: i128,
    pub levels_attributed: u32,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct ReferralTracker;

#[contractimpl]
impl ReferralTracker {
    /// `commission_rates_bps`: one entry per referral level (index 0 = direct referrer).
    pub fn init(env: Env, admin: Address, commission_rates_bps: Vec<u32>) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        if commission_rates_bps.len() == 0 || commission_rates_bps.len() > MAX_DEPTH {
            return Err(Error::InvalidConfig);
        }
        let total: u32 = commission_rates_bps
            .iter()
            .fold(0u32, |a, b| a.saturating_add(b));
        if total > 10_000 {
            return Err(Error::InvalidConfig);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::CommissionRatesBps, &commission_rates_bps);
        Ok(())
    }

    /// Player registers (optionally with a referrer).
    pub fn register(env: Env, player: Address, referrer: Option<Address>) -> Result<(), Error> {
        require_initialized(&env)?;
        player.require_auth();

        let key = DataKey::Referrer(player.clone());
        if env.storage().persistent().has(&key) {
            return Err(Error::AlreadyRegistered);
        }

        if let Some(ref ref_addr) = referrer {
            if *ref_addr == player {
                return Err(Error::SelfReferral);
            }
            let ref_key = DataKey::Referrer(ref_addr.clone());
            if !env.storage().persistent().has(&ref_key) {
                return Err(Error::ReferrerNotRegistered);
            }

            // Increment referrer's direct referral count
            let count_key = DataKey::ReferralCount(ref_addr.clone());
            let count: u32 = env
                .storage()
                .persistent()
                .get(&count_key)
                .unwrap_or(0u32);
            env.storage()
                .persistent()
                .set(&count_key, &count.saturating_add(1));
        }

        // Store `None` as a sentinel meaning "registered, no referrer"
        env.storage().persistent().set(&key, &referrer);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        PlayerRegistered {
            player,
            referrer: referrer.clone(),
        }
        .publish(&env);

        Ok(())
    }

    /// Returns the referral chain for `player` up to `MAX_DEPTH` levels.
    pub fn get_referral_chain(env: Env, player: Address) -> Vec<ReferralChainEntry> {
        let rates: Vec<u32> = env
            .storage()
            .instance()
            .get(&DataKey::CommissionRatesBps)
            .unwrap_or(Vec::new(&env));

        let mut chain = Vec::new(&env);
        let mut current: Option<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Referrer(player));

        let mut level = 0u32;
        while let Some(ref_addr) = current {
            if level >= rates.len() {
                break;
            }
            let bps = rates.get(level).unwrap_or(0);
            chain.push_back(ReferralChainEntry {
                address: ref_addr.clone(),
                level,
                commission_bps: bps,
            });
            level += 1;
            current = env
                .storage()
                .persistent()
                .get(&DataKey::Referrer(ref_addr));
        }
        chain
    }

    /// Authorised caller logs that `player` generated `gross_amount` in revenue.
    /// Returns the full referral chain so the caller can distribute commissions.
    pub fn attribute_revenue(
        env: Env,
        caller: Address,
        player: Address,
        gross_amount: i128,
    ) -> Result<Vec<ReferralChainEntry>, Error> {
        require_admin(&env, &caller)?;

        let chain = Self::get_referral_chain(env.clone(), player.clone());
        let levels_attributed = chain.len();

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalAttributed)
            .unwrap_or(0i128);
        env.storage().instance().set(
            &DataKey::TotalAttributed,
            &total.checked_add(gross_amount).ok_or(Error::Overflow)?,
        );

        RevenueAttributed {
            player,
            gross_amount,
            levels_attributed,
        }
        .publish(&env);

        Ok(chain)
    }

    pub fn get_referral_count(env: Env, referrer: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ReferralCount(referrer))
            .unwrap_or(0u32)
    }

    pub fn get_total_attributed(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalAttributed)
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
    use soroban_sdk::{testutils::Address as _, vec, Address, Env};

    #[test]
    fn register_without_referrer() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(ReferralTracker, ());
        let client = ReferralTrackerClient::new(&env, &id);

        let admin = Address::generate(&env);
        let rates = vec![&env, 500u32, 200u32];
        client.init(&admin, &rates);

        let player = Address::generate(&env);
        client.register(&player, &None);

        let chain = client.get_referral_chain(&player);
        assert_eq!(chain.len(), 0u32);
    }

    #[test]
    fn register_with_referrer_builds_chain() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(ReferralTracker, ());
        let client = ReferralTrackerClient::new(&env, &id);

        let admin = Address::generate(&env);
        let rates = vec![&env, 500u32, 200u32];
        client.init(&admin, &rates);

        let referrer = Address::generate(&env);
        let player = Address::generate(&env);

        client.register(&referrer, &None);
        client.register(&player, &Some(referrer.clone()));

        let chain = client.get_referral_chain(&player);
        assert_eq!(chain.len(), 1u32);
        assert_eq!(chain.get(0).unwrap().commission_bps, 500u32);
    }
}
