//! Stellarcade Game Subscription Contract
//!
//! Provides time-gated access to premium game content. Players pay a
//! subscription fee for a configurable duration. Game contracts call
//! `is_subscribed` to gate features.
//!
//! ## Flow
//! 1. Admin `init`s with token address and subscription plans.
//! 2. Players call `subscribe` with the desired plan ID.
//! 3. Game contracts call `is_subscribed` to gate access.
//! 4. Admin can add or update plans at any time.
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
    PlanNotFound = 4,
    InvalidAmount = 5,
    Overflow = 6,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Token,
    Plan(u32),
    Subscription(Address),
    TotalRevenue,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPlan {
    pub plan_id: u32,
    pub duration_secs: u64,
    pub price: i128,
    pub is_active: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionRecord {
    pub plan_id: u32,
    pub expires_ts: u64,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
pub struct Subscribed {
    #[topic]
    pub player: Address,
    #[topic]
    pub plan_id: u32,
    pub expires_ts: u64,
    pub price: i128,
}

#[contractevent]
pub struct PlanUpdated {
    #[topic]
    pub plan_id: u32,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct GameSubscription;

#[contractimpl]
impl GameSubscription {
    pub fn init(env: Env, admin: Address, token: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        Ok(())
    }

    /// Admin creates or updates a subscription plan.
    pub fn set_plan(env: Env, admin: Address, plan: SubscriptionPlan) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::Plan(plan.plan_id), &plan);
        PlanUpdated { plan_id: plan.plan_id }.publish(&env);
        Ok(())
    }

    /// Player subscribes to a plan. If already subscribed, extends from the
    /// later of `now` or the current expiry.
    pub fn subscribe(env: Env, player: Address, plan_id: u32) -> Result<u64, Error> {
        require_initialized(&env)?;
        player.require_auth();

        let plan: SubscriptionPlan = env
            .storage()
            .instance()
            .get(&DataKey::Plan(plan_id))
            .ok_or(Error::PlanNotFound)?;

        if !plan.is_active {
            return Err(Error::PlanNotFound);
        }

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        TokenClient::new(&env, &token).transfer(&player, &env.current_contract_address(), &plan.price);

        let now = env.ledger().timestamp();
        let existing_expiry = env
            .storage()
            .persistent()
            .get::<DataKey, SubscriptionRecord>(&DataKey::Subscription(player.clone()))
            .map(|r| r.expires_ts)
            .unwrap_or(0u64);

        let start = core::cmp::max(now, existing_expiry);
        let expires_ts = start
            .checked_add(plan.duration_secs)
            .ok_or(Error::Overflow)?;

        let rec = SubscriptionRecord { plan_id, expires_ts };
        let key = DataKey::Subscription(player.clone());
        env.storage().persistent().set(&key, &rec);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_BUMP_LEDGERS, PERSISTENT_BUMP_LEDGERS);

        let revenue: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalRevenue)
            .unwrap_or(0i128);
        env.storage().instance().set(
            &DataKey::TotalRevenue,
            &revenue.checked_add(plan.price).ok_or(Error::Overflow)?,
        );

        Subscribed {
            player,
            plan_id,
            expires_ts,
            price: plan.price,
        }
        .publish(&env);

        Ok(expires_ts)
    }

    /// Returns `true` if the player's subscription is currently active.
    pub fn is_subscribed(env: Env, player: Address) -> bool {
        let now = env.ledger().timestamp();
        env.storage()
            .persistent()
            .get::<DataKey, SubscriptionRecord>(&DataKey::Subscription(player))
            .map(|r| r.expires_ts > now)
            .unwrap_or(false)
    }

    pub fn get_subscription(env: Env, player: Address) -> Option<SubscriptionRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Subscription(player))
    }

    pub fn get_plan(env: Env, plan_id: u32) -> Result<SubscriptionPlan, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Plan(plan_id))
            .ok_or(Error::PlanNotFound)
    }

    pub fn get_total_revenue(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalRevenue)
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
    fn set_and_get_plan() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(GameSubscription, ());
        let client = GameSubscriptionClient::new(&env, &id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.init(&admin, &token);

        client.set_plan(
            &admin,
            &SubscriptionPlan {
                plan_id: 1,
                duration_secs: 2_592_000,
                price: 500,
                is_active: true,
            },
        );

        let plan = client.get_plan(&1u32);
        assert_eq!(plan.duration_secs, 2_592_000u64);
    }

    #[test]
    fn unsubscribed_player_is_not_subscribed() {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(GameSubscription, ());
        let client = GameSubscriptionClient::new(&env, &id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.init(&admin, &token);

        let player = Address::generate(&env);
        assert!(!client.is_subscribed(&player));
    }
}
