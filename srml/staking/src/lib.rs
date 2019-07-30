// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.


#![recursion_limit = "128"]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(all(feature = "bench", test), feature(test))]

#[cfg(all(feature = "bench", test))]
extern crate test;

use parity_codec::{Decode, Encode, HasCompact};
#[cfg(feature = "std")]
use primitives::{Deserialize, Serialize};
use primitives::Perbill;
use primitives::traits::{
    Bounded, CheckedShl, CheckedSub, Convert, One, Saturating, StaticLookup, Zero,
};
use rstd::{collections::btree_map::BTreeMap, prelude::*, result};
#[cfg(feature = "std")]
use runtime_io::with_storage;
use session::{OnSessionEnding, SessionIndex};
use srml_support::{
    decl_event, decl_module, decl_storage, ensure, EnumerableStorageMap,
    StorageMap, StorageValue, traits::{
        Currency, Get, Imbalance, LockableCurrency, LockIdentifier,
        OnDilution, OnFreeBalanceZero, OnUnbalanced, WithdrawReasons,
    },
};
use system::ensure_signed;

use dsupport::traits::SystemCurrency;
use phragmen::{ACCURACY, elect, equalize, ExtendedBalance};


mod minting;

#[cfg(any(feature = "bench", test))]
mod mock;

#[cfg(test)]
mod tests;

mod phragmen;

#[cfg(all(feature = "bench", test))]
mod benches;

const RECENT_OFFLINE_COUNT: usize = 32;
const DEFAULT_MINIMUM_VALIDATOR_COUNT: u32 = 4;
const MAX_NOMINATIONS: usize = 16;
const MAX_UNSTAKE_THRESHOLD: u32 = 10;
const MAX_UNLOCKING_CHUNKS: usize = 32;
const STAKING_ID: LockIdentifier = *b"staking ";

/// Counter for the number of eras that have passed.
pub type EraIndex = u32;
// customed: counter for number of eras per epoch.
pub type ErasNums = u32;

/// Indicates the initial status of the staker.
#[cfg_attr(feature = "std", derive(Debug, Serialize, Deserialize))]
pub enum StakerStatus<AccountId> {
    /// Chilling.
    Idle,
    /// Declared desire in validating or already participating in it.
    Validator,
    /// Nominating for a group of other stakers.
    Nominator(Vec<AccountId>),
}

/// A destination account for payment.
#[derive(PartialEq, Eq, Copy, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum RewardDestination {
    /// Pay into the stash account, increasing the amount at stake accordingly.
    StakedDeprecated,
    /// Pay into the stash account, not increasing the amount at stake.
    Stash,
    /// Pay into the controller account.
    Controller,
}

impl Default for RewardDestination {
    fn default() -> Self {
        RewardDestination::Stash
    }
}

/// Preference of what happens on a slash event.
#[derive(PartialEq, Eq, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct ValidatorPrefs<Balance: HasCompact> {
    /// Validator should ensure this many more slashes than is necessary before being unstaked.
    #[codec(compact)]
    pub unstake_threshold: u32,
    /// Reward that validator takes up-front; only the rest is split between themselves and
    /// nominators.
    #[codec(compact)]
    pub validator_payment: Balance,

    pub name: Vec<u8>,
}

impl<B: Default + HasCompact + Copy> Default for ValidatorPrefs<B> {
    fn default() -> Self {
        ValidatorPrefs {
            unstake_threshold: 3,
            validator_payment: Default::default(),
            name: [0;8].to_vec()
        }
    }
}

/// Just a Balance/BlockNumber tuple to encode when a chunk of funds will be unlocked.
#[derive(PartialEq, Eq, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct UnlockChunk<Balance: HasCompact> {
    /// Amount of funds to be unlocked.
    #[codec(compact)]
    value: Balance,
    /// Era number at which point it'll be unlocked.
    #[codec(compact)]
    era: EraIndex,
}

/// The ledger of a (bonded) stash.
#[derive(PartialEq, Eq, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct StakingLedger<AccountId, Balance: HasCompact> {
    /// The stash account whose balance is actually locked and at stake.
    pub stash: AccountId,
    /// The total amount of the stash's balance that we are currently accounting for.
    /// It's just `active` plus all the `unlocking` balances.
    #[codec(compact)]
    pub total: Balance,
    /// The total amount of the stash's balance that will be at stake in any forthcoming
    /// rounds.
    #[codec(compact)]
    pub active: Balance,
    /// Any balance that is becoming free, which may eventually be transferred out
    /// of the stash (assuming it doesn't get slashed first).
    pub unlocking: Vec<UnlockChunk<Balance>>,
}

impl<
    AccountId,
    Balance: HasCompact + Copy + Saturating,
> StakingLedger<AccountId, Balance> {
    /// Remove entries from `unlocking` that are sufficiently old and reduce the
    /// total by the sum of their balances.
    fn consolidate_unlocked(self, current_era: EraIndex) -> Self {
        let mut total = self.total;
        let unlocking = self.unlocking.into_iter()
            .filter(|chunk| if chunk.era > current_era {
                true
            } else {
                total = total.saturating_sub(chunk.value);
                false
            })
            .collect();
        Self { total, active: self.active, stash: self.stash, unlocking }
    }
}

/// The amount of exposure (to slashing) than an individual nominator has.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct IndividualExposure<AccountId, Balance: HasCompact> {
    /// The stash account of the nominator in question.
    who: AccountId,
    /// Amount of funds exposed.
    #[codec(compact)]
    value: Balance,
}

/// A snapshot of the stake backing a single validator in the system.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, Default)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct Exposure<AccountId, Balance: HasCompact> {
    /// The total balance backing this validator.
    #[codec(compact)]
    pub total: Balance,
    /// The validator's own stash that is exposed.
    #[codec(compact)]
    pub own: Balance,
    /// The portions of nominators stashes that are exposed.
    pub others: Vec<IndividualExposure<AccountId, Balance>>,
}

// for kton
type BalanceOf<T> = <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::Balance;
// for ring
type RewardBalanceOf<T> = <<T as Trait>::RewardCurrency as Currency<<T as system::Trait>::AccountId>>::Balance;
// imbalance of ring -for rewarding
type PositiveImbalanceOf<T> = <<T as Trait>::RewardCurrency as Currency<<T as system::Trait>::AccountId>>::PositiveImbalance;
// imbalance of kton - for slashing
type NegativeImbalanceOf<T> = <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::NegativeImbalance;


type RawAssignment<T> = (<T as system::Trait>::AccountId, ExtendedBalance);
type Assignment<T> = (<T as system::Trait>::AccountId, ExtendedBalance, BalanceOf<T>);
type ExpoMap<T> = BTreeMap<
    <T as system::Trait>::AccountId,
    Exposure<<T as system::Trait>::AccountId, BalanceOf<T>>
>;

pub const DEFAULT_SESSIONS_PER_ERA: u32 = 3;
pub const DEFAULT_BONDING_DURATION: u32 = 1;

pub trait Trait: system::Trait + session::Trait {
    /// The staking balance.
    type Currency: LockableCurrency<Self::AccountId, Moment=Self::BlockNumber> +
    SystemCurrency<Self::AccountId, <Self::RewardCurrency as Currency<Self::AccountId>>::Balance>;

    // Customed: for ring
    type RewardCurrency: Currency<Self::AccountId>;

    type CurrencyToVote: Convert<BalanceOf<Self>, u64> + Convert<u128, BalanceOf<Self>>;

    /// Some tokens minted.
    type OnRewardMinted: OnDilution<RewardBalanceOf<Self>>;

    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

    /// Handler for the unbalanced reduction when slashing a staker.
    type Slash: OnUnbalanced<NegativeImbalanceOf<Self>>;

    /// Handler for the unbalanced increment when rewarding a staker.
    type Reward: OnUnbalanced<PositiveImbalanceOf<Self>>;

    /// Number of sessions per era.
    type SessionsPerEra: Get<SessionIndex>;

    /// Number of eras that staked funds must remain bonded for.
    type BondingDuration: Get<EraIndex>;

    // customed
    type Cap: Get<<Self::RewardCurrency as Currency<Self::AccountId>>::Balance>;
    type ErasPerEpoch: Get<ErasNums>;
}

decl_storage! {
	trait Store for Module<T: Trait> as Staking {

		/// The ideal number of staking participants.
		pub ValidatorCount get(validator_count) config(): u32;
		/// Minimum number of staking participants before emergency conditions are imposed.
		pub MinimumValidatorCount get(minimum_validator_count) config():
			u32 = DEFAULT_MINIMUM_VALIDATOR_COUNT;
		/// Maximum reward, per validator, that is provided per acceptable session.
		pub SessionReward get(session_reward) config(): Perbill = Perbill::from_parts(60);
		/// Slash, per validator that is taken for the first time they are found to be offline.
		pub OfflineSlash get(offline_slash) config(): Perbill = Perbill::from_millionths(1000);
		/// Number of instances of offline reports before slashing begins for validators.
		pub OfflineSlashGrace get(offline_slash_grace) config(): u32;

		/// Any validators that may never be slashed or forcibly kicked. It's a Vec since they're
		/// easy to initialize and the performance hit is minimal (we expect no more than four
		/// invulnerables) and restricted to testnets.
		pub Invulnerables get(invulnerables) config(): Vec<T::AccountId>;

		/// Map from all locked "stash" accounts to the controller account.
		pub Bonded get(bonded): map T::AccountId => Option<T::AccountId>;
		/// Map from all (unlocked) "controller" accounts to the info regarding the staking.
		pub Ledger get(ledger):
			map T::AccountId => Option<StakingLedger<T::AccountId, BalanceOf<T>>>;

		/// Where the reward payment should be made. Keyed by stash.
		pub Payee get(payee): map T::AccountId => RewardDestination;

		/// The map from (wannabe) validator stash key to the preferences of that validator.
		pub Validators get(validators): linked_map T::AccountId => ValidatorPrefs<RewardBalanceOf<T>>;

		/// The map from nominator stash key to the set of stash keys of all validators to nominate.
		pub Nominators get(nominators): linked_map T::AccountId => Vec<T::AccountId>;

		/// Nominators for a particular account that is in action right now. You can't iterate
		/// through validators here, but you can find them in the Session module.
		///
		/// This is keyed by the stash account.
		pub Stakers get(stakers): map T::AccountId => Exposure<T::AccountId, BalanceOf<T>>;

		// The historical validators and their nominations for a given era. Stored as a trie root
		// of the mapping `T::AccountId` => `Exposure<T::AccountId, BalanceOf<T>>`, which is just
		// the contents of `Stakers`, under a key that is the `era`.
		//
		// Every era change, this will be appended with the trie root of the contents of `Stakers`,
		// and the oldest entry removed down to a specific number of entries (probably around 90 for
		// a 3 month history).
		// pub HistoricalStakers get(historical_stakers): map T::BlockNumber => Option<H256>;

		/// The currently elected validator set keyed by stash account ID.
		pub CurrentElected get(current_elected): Vec<T::AccountId>;

		/// The current era index.
		pub CurrentEra get(current_era) config(): EraIndex;

		/// Maximum reward, per validator, that is provided per acceptable session.
		pub CurrentSessionReward get(current_session_reward) config(): RewardBalanceOf<T>;

		/// The accumulated reward for the current era. Reset to zero at the beginning of the era
		/// and increased for every successfully finished session.
		pub CurrentEraTotalReward get(current_era_total_reward) config(): RewardBalanceOf<T>;

		/// The accumulated reward for the current era. Reset to zero at the beginning of the era
		/// and increased for every successfully finished session.
		pub CurrentEraReward get(current_era_reward): RewardBalanceOf<T>;

		/// The amount of balance actively at stake for each validator slot, currently.
		///
		/// This is used to derive rewards and punishments.
		pub SlotStake get(slot_stake) build(|config: &GenesisConfig<T>| {
			config.stakers.iter().map(|&(_, _, value, _)| value).min().unwrap_or_default()
		}): BalanceOf<T>;

		/// The number of times a given validator has been reported offline. This gets decremented
		/// by one each era that passes.
		pub SlashCount get(slash_count): map T::AccountId => u32;

		/// Most recent `RECENT_OFFLINE_COUNT` instances. (Who it was, when it was reported, how
		/// many instances they were offline for).
		pub RecentlyOffline get(recently_offline): Vec<(T::AccountId, T::BlockNumber, u32)>;

		/// True if the next session change will be a new era regardless of index.
		pub ForceNewEra get(forcing_new_era): bool;

		pub EpochIndex get(epoch_index): T::BlockNumber = 0.into();

		pub ShouldOffline get(should_offline): Vec<T::AccountId>;
	}
	add_extra_genesis {
		config(stakers):
			Vec<(T::AccountId, T::AccountId, BalanceOf<T>, StakerStatus<T::AccountId>)>;
		build(|
			storage: &mut primitives::StorageOverlay,
			_: &mut primitives::ChildrenStorageOverlay,
			config: &GenesisConfig<T>
		| {
			with_storage(storage, || {
				for &(ref stash, ref controller, balance, ref status) in &config.stakers {
//					assert!(T::Currency::free_balance(&stash) >= balance);
					let _ = <Module<T>>::bond(
						T::Origin::from(Some(stash.clone()).into()),
						T::Lookup::unlookup(controller.clone()),
						balance,
						RewardDestination::Stash
					);
					let _ = match status {
						StakerStatus::Validator => {
							<Module<T>>::validate(
								T::Origin::from(Some(controller.clone()).into()),
								Default::default()
							)
						}, StakerStatus::Nominator(votes) => {
							<Module<T>>::nominate(
								T::Origin::from(Some(controller.clone()).into()),
								votes.iter().map(|l| {T::Lookup::unlookup(l.clone())}).collect()
							)
						}, _ => Ok(())
					};
				}

				if let (_, Some(validators)) = <Module<T>>::select_validators() {
					<session::Validators<T>>::put(&validators);
				}
			});
		});
	}
}

decl_event!(
	pub enum Event<T> where
		RewardBalance = RewardBalanceOf<T>,
		Balance = BalanceOf<T>,
		<T as system::Trait>::AccountId
		{
		/// All validators have been rewarded by the given balance.
		Reward(RewardBalance),
		/// One validator (and its nominators) has been given an offline-warning (it is still
		/// within its grace). The accrued number of slashes is recorded, too.
		OfflineWarning(AccountId, u32),
		/// One validator (and its nominators) has been slashed by the given amount.
		OfflineSlash(AccountId, Balance),
	}
);

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		/// Number of sessions per era.
		const SessionsPerEra: SessionIndex = T::SessionsPerEra::get();

		/// Number of eras that staked funds must remain bonded for.
		const BondingDuration: EraIndex = T::BondingDuration::get();

		fn deposit_event<T>() = default;

		fn bond(origin,
			controller: <T::Lookup as StaticLookup>::Source,
			#[compact] value: BalanceOf<T>,
			payee: RewardDestination
		) {
			let stash = ensure_signed(origin)?;

			if <Bonded<T>>::exists(&stash) {
				return Err("stash already bonded")
			}

			let controller = T::Lookup::lookup(controller)?;

			if <Ledger<T>>::exists(&controller) {
				return Err("controller already paired")
			}

			// You're auto-bonded forever, here. We might improve this by only bonding when
			// you actually validate/nominate and remove once you unbond __everything__.
			<Bonded<T>>::insert(&stash, controller.clone());
			<Payee<T>>::insert(&stash, payee);

			let stash_balance = T::Currency::free_balance(&stash);
			let value = value.min(stash_balance);
			let item = StakingLedger { stash, total: value, active: value, unlocking: vec![] };
			Self::update_ledger(&controller, &item);
		}


		fn bond_extra(origin, #[compact] max_additional: BalanceOf<T>) {
			let stash = ensure_signed(origin)?;

			let controller = Self::bonded(&stash).ok_or("not a stash")?;
			let mut ledger = Self::ledger(&controller).ok_or("not a controller")?;

			let stash_balance = T::Currency::free_balance(&stash);

			if let Some(extra) = stash_balance.checked_sub(&ledger.total) {
				let extra = extra.min(max_additional);
				ledger.total += extra;
				ledger.active += extra;
				Self::update_ledger(&controller, &ledger);
			}
		}


		fn unbond(origin, #[compact] value: BalanceOf<T>) {
			let controller = ensure_signed(origin)?;
			let mut ledger = Self::ledger(&controller).ok_or("not a controller")?;
			ensure!(
				ledger.unlocking.len() < MAX_UNLOCKING_CHUNKS,
				"can not schedule more unlock chunks"
			);

			let mut value = value.min(ledger.active);

			if !value.is_zero() {
				ledger.active -= value;

				// Avoid there being a dust balance left in the staking system.
				if ledger.active < T::Currency::minimum_balance() {
					value += ledger.active;
					ledger.active = Zero::zero();
				}

				let era = Self::current_era() + T::BondingDuration::get();
				ledger.unlocking.push(UnlockChunk { value, era });
				Self::update_ledger(&controller, &ledger);
			}
		}


		fn withdraw_unbonded(origin) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let ledger = ledger.consolidate_unlocked(Self::current_era());
			Self::update_ledger(&controller, &ledger);
		}


		fn validate(origin, unstake_threshold: u32, validator_payment: RewardBalanceOf<T>, name: Vec<u8>) {
			let controller = ensure_signed(origin)?;
			let prefs = ValidatorPrefs {unstake_threshold, validator_payment, name};
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			ensure!(
				prefs.unstake_threshold <= MAX_UNSTAKE_THRESHOLD,
				"unstake threshold too large"
			);
			<Nominators<T>>::remove(stash);
			<Validators<T>>::insert(stash, prefs);
		}


		fn nominate(origin, targets: Vec<<T::Lookup as StaticLookup>::Source>) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			ensure!(!targets.is_empty(), "targets cannot be empty");
			let targets = targets.into_iter()
				.take(MAX_NOMINATIONS)
				.map(T::Lookup::lookup)
				.collect::<result::Result<Vec<T::AccountId>, &'static str>>()?;

			<Validators<T>>::remove(stash);
			<Nominators<T>>::insert(stash, targets);
		}


		fn chill(origin) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			<Validators<T>>::remove(stash);
			<Nominators<T>>::remove(stash);
		}


		fn set_payee(origin, payee: RewardDestination) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			<Payee<T>>::insert(stash, payee);
		}


		fn set_controller(origin, controller: <T::Lookup as StaticLookup>::Source) {
			let stash = ensure_signed(origin)?;
			let old_controller = Self::bonded(&stash).ok_or("not a stash")?;
			let controller = T::Lookup::lookup(controller)?;
			if <Ledger<T>>::exists(&controller) {
				return Err("controller already paired")
			}
			if controller != old_controller {
				<Bonded<T>>::insert(&stash, &controller);
				if let Some(l) = <Ledger<T>>::take(&old_controller) {
					<Ledger<T>>::insert(&controller, l);
				}
			}
		}

		/// The ideal number of validators.
		fn set_validator_count(#[compact] new: u32) {
			ValidatorCount::put(new);
		}

		// ----- Root calls.

		fn force_new_era() {
			Self::apply_force_new_era()
		}

		/// Set the offline slash grace period.
		fn set_offline_slash_grace(#[compact] new: u32) {
			OfflineSlashGrace::put(new);
		}

		/// Set the validators who cannot be slashed (if any).
		fn set_invulnerables(validators: Vec<T::AccountId>) {
			<Invulnerables<T>>::put(validators);
		}
	}
}

impl<T: Trait> Module<T> {
    // PUBLIC IMMUTABLES

    /// The total balance that can be slashed from a validator controller account as of
    /// right now.
    pub fn slashable_balance(who: &T::AccountId) -> BalanceOf<T> {
        Self::stakers(who).total
    }

    // MUTABLES (DANGEROUS)

    /// Update the ledger for a controller. This will also update the stash lock.
    fn update_ledger(
        controller: &T::AccountId,
        ledger: &StakingLedger<T::AccountId, BalanceOf<T>>,
    ) {
        T::Currency::set_lock(
            STAKING_ID,
            &ledger.stash,
            ledger.total,
            T::BlockNumber::max_value(),
            WithdrawReasons::all(),
        );
        <Ledger<T>>::insert(controller, ledger);
    }

    /// Slash a given validator by a specific amount. Removes the slash from the validator's
    /// balance by preference, and reduces the nominators' balance if needed.
    fn slash_validator(stash: &T::AccountId, slash: BalanceOf<T>) {
        // The exposure (backing stake) information of the validator to be slashed.
        let mut exposure = Self::stakers(stash);
        // The amount we are actually going to slash (can't be bigger than the validator's total
        // exposure)
        let slash = slash.min(exposure.total);
        // The amount we'll slash from the validator's stash directly.
        let own_slash = exposure.own.min(slash);

//        // customed
//        // for validator, first slash bonded value
//        let exposure_own = exposure.own;
//
//        if exposure.own > 0.into() {
//            exposure.own -= own_slash;
//            if slash > own_slash {
//                own_slash = slash - own_slash;
//            }
//        }

        let (mut imbalance, missing) = T::Currency::slash(stash, own_slash);
        let own_slash = own_slash - missing;
        // The amount remaining that we can't slash from the validator, that must be taken from the
        // nominators.
        let rest_slash = slash - own_slash;
        if !rest_slash.is_zero() {
            // The total to be slashed from the nominators.
            let total = exposure.total - exposure.own;
            if !total.is_zero() {
                for i in exposure.others.iter() {
                    let per_u64 = Perbill::from_rational_approximation(i.value, total);
                    // best effort - not much that can be done on fail.
                    imbalance.subsume(T::Currency::slash(&i.who, per_u64 * rest_slash).0)
                }
            } else {
                exposure.total = 0.into();
            }

//            <Stakers<T>>::insert(stash, exposure);
        }
        T::Slash::on_unbalanced(imbalance);
    }

    /// Actually make a payment to a staker. This uses the currency's reward function
    /// to pay the right payee for the given staker account.
    fn make_payout(stash: &T::AccountId, amount: RewardBalanceOf<T>) -> Option<PositiveImbalanceOf<T>> {
        let dest = Self::payee(stash);
        match dest {
            RewardDestination::Controller => Self::bonded(stash)
                .and_then(|controller|
                    T::RewardCurrency::deposit_into_existing(&controller, amount).ok()
                ),
            RewardDestination::Stash =>
                T::RewardCurrency::deposit_into_existing(stash, amount).ok(),
            RewardDestination::StakedDeprecated => None,
        }
    }

    /// Reward a given validator by a specific amount. Add the reward to the validator's, and its
    /// nominators' balance, pro-rata based on their exposure, after having removed the validator's
    /// pre-payout cut.
    fn reward_validator(stash: &T::AccountId, reward: RewardBalanceOf<T>) {
        let off_the_table = reward.min(Self::validators(stash).validator_payment);
        let reward = reward - off_the_table;
        let mut imbalance = <PositiveImbalanceOf<T>>::zero();
        let validator_cut = if reward.is_zero() {
            Zero::zero()
        } else {
            let exposure = Self::stakers(stash);
            let total = exposure.total.max(One::one());

            for i in &exposure.others {
                let per_u64 = Perbill::from_rational_approximation(i.value, total);
                imbalance.maybe_subsume(Self::make_payout(&i.who, per_u64 * reward));
            }

            let per_u64 = Perbill::from_rational_approximation(exposure.own, total);
            per_u64 * reward
        };
        imbalance.maybe_subsume(Self::make_payout(stash, validator_cut + off_the_table));
        T::Reward::on_unbalanced(imbalance);
    }

    /// Session has just ended. Provide the validator set for the next session if it's an era-end.
    fn new_session(session_index: SessionIndex) -> Option<Vec<T::AccountId>> {
        // accumulate good session reward
        let reward = Self::current_session_reward();
        <CurrentEraReward<T>>::mutate(|r| *r += reward);

        if ForceNewEra::take() || session_index % T::SessionsPerEra::get() == 0 {
            Self::new_era()
        } else {
            None
        }
    }

    /// The era has changed - enact new staking set.
    ///
    /// NOTE: This always happens immediately before a session change to ensure that new validators
    /// get a chance to set their session keys.
    fn new_era() -> Option<Vec<T::AccountId>> {
        let reward = Self::session_reward() * Self::current_era_total_reward();
        if !reward.is_zero() {
            let validators = Self::current_elected();
            let len = validators.len() as u32; // validators length can never overflow u64
            let len: RewardBalanceOf<T> = len.into();
            let block_reward_per_validator = reward / len;
            for v in validators.iter() {
                Self::reward_validator(v, block_reward_per_validator);
            }
            Self::deposit_event(RawEvent::Reward(block_reward_per_validator));

            T::Currency::reward_to_pot(reward);
            // TODO: reward to treasury
        }

        // check if ok to change epoch
        if Self::current_era() % T::ErasPerEpoch::get() == 0 {
            Self::new_epoch();
        }
        // Increment current era.
        CurrentEra::mutate(|s| *s += 1);

        // Reassign all Stakers.
        let (_, maybe_new_validators) = Self::select_validators();

        maybe_new_validators
    }

    fn slashable_balance_of(stash: &T::AccountId) -> BalanceOf<T> {
        Self::bonded(stash).and_then(Self::ledger).map(|l| l.total).unwrap_or_default()
    }

    fn new_epoch() {
        <EpochIndex<T>>::put(Self::epoch_index() + One::one());
        if let Ok(next_era_reward) =  minting::compute_current_era_reward::<T>() {
            // TODO: change to CurrentEraReward
            <CurrentEraTotalReward<T>>::put(next_era_reward);
        }
    }

    /// Select a new validator set from the assembled stakers and their role preferences.
    ///
    /// Returns the new `SlotStake` value.
    fn select_validators() -> (BalanceOf<T>, Option<Vec<T::AccountId>>) {
        let maybe_elected_set = elect::<T, _, _, _>(
            Self::validator_count() as usize,
            Self::minimum_validator_count().max(1) as usize,
            <Validators<T>>::enumerate(),
            <Nominators<T>>::enumerate(),
            Self::slashable_balance_of,
        );

        if let Some(elected_set) = maybe_elected_set {
            let mut elected_stashes = elected_set.0;
            let assignments = elected_set.1;

            // helper closure.
            let to_balance = |b: ExtendedBalance|
                <T::CurrencyToVote as Convert<ExtendedBalance, BalanceOf<T>>>::convert(b);
            let to_votes = |b: BalanceOf<T>|
                <T::CurrencyToVote as Convert<BalanceOf<T>, u64>>::convert(b) as ExtendedBalance;

            // The return value of this is safe to be converted to u64.
            // The original balance, `b` is within the scope of u64. It is just extended to u128
            // to be properly multiplied by a ratio, which will lead to another value
            // less than u64 for sure. The result can then be safely passed to `to_balance`.
            // For now the backward convert is used. A simple `TryFrom<u64>` is also safe.
            let ratio_of = |b, p| (p as ExtendedBalance).saturating_mul(to_votes(b)) / ACCURACY;

            // Compute the actual stake from nominator's ratio.
            let assignments_with_stakes = assignments.iter().map(|(n, a)| (
                n.clone(),
                Self::slashable_balance_of(n),
                a.iter().map(|(acc, r)| (
                    acc.clone(),
                    *r,
                    to_balance(ratio_of(Self::slashable_balance_of(n), *r)),
                ))
                    .collect::<Vec<Assignment<T>>>()
            )).collect::<Vec<(T::AccountId, BalanceOf<T>, Vec<Assignment<T>>)>>();

            // update elected candidate exposures.
            let mut exposures = <ExpoMap<T>>::new();
            elected_stashes
                .iter()
                .map(|e| (e, Self::slashable_balance_of(e)))
                .for_each(|(e, s)| {
                    let item = Exposure { own: s, total: s, ..Default::default() };
                    exposures.insert(e.clone(), item);
                });

            for (n, _, assignment) in &assignments_with_stakes {
                for (c, _, s) in assignment {
                    if let Some(expo) = exposures.get_mut(c) {
                        // NOTE: simple example where this saturates:
                        // candidate with max_value stake. 1 nominator with max_value stake.
                        // Nuked. Sadly there is not much that we can do about this.
                        // See this test: phragmen_should_not_overflow_xxx()
                        expo.total = expo.total.saturating_add(*s);
                        expo.others.push(IndividualExposure { who: n.clone(), value: *s });
                    }
                }
            }

            if cfg!(feature = "equalize") {
                let tolerance = 0_u128;
                let iterations = 2_usize;
                let mut assignments_with_votes = assignments_with_stakes.iter()
                    .map(|a| (
                        a.0.clone(), a.1,
                        a.2.iter()
                            .map(|e| (e.0.clone(), e.1, to_votes(e.2)))
                            .collect::<Vec<(T::AccountId, ExtendedBalance, ExtendedBalance)>>()
                    ))
                    .collect::<Vec<(
                        T::AccountId,
                        BalanceOf<T>,
                        Vec<(T::AccountId, ExtendedBalance, ExtendedBalance)>
                    )>>();
                equalize::<T>(&mut assignments_with_votes, &mut exposures, tolerance, iterations);
            }

            // Clear Stakers and reduce their slash_count.
            for v in Self::current_elected().iter() {
                <Stakers<T>>::remove(v);
                let slash_count = <SlashCount<T>>::take(v);
                if slash_count > 1 {
                    <SlashCount<T>>::insert(v, slash_count - 1);
                }
            }

            // Populate Stakers and figure out the minimum stake behind a slot.
            let mut slot_stake = BalanceOf::<T>::max_value();
            for (c, e) in exposures.iter() {
                if e.total < slot_stake {
                    slot_stake = e.total;
                }
                <Stakers<T>>::insert(c.clone(), e.clone());
            }

            // Update slot stake.
            <SlotStake<T>>::put(&slot_stake);

//            for st in <ShouldOffline<T>>::take().iter() {
//                elected_stashes.retain(|ref s| s != &st);
//            }

            // Set the new validator set in sessions.
            <CurrentElected<T>>::put(&elected_stashes);
            let validators = elected_stashes.into_iter()
                .map(|s| Self::bonded(s).unwrap_or_default())
                .collect::<Vec<_>>();
            (slot_stake, Some(validators))
        } else {
            // There were not enough candidates for even our minimal level of functionality.
            // This is bad.
            // We should probably disable all functionality except for block production
            // and let the chain keep producing blocks until we can decide on a sufficiently
            // substantial set.
            // TODO: #2494
            (Self::slot_stake(), None)
        }
    }

    fn apply_force_new_era() {
        ForceNewEra::put(true);
    }

    /// Call when a validator is determined to be offline. `count` is the
    /// number of offenses the validator has committed.
    ///
    /// NOTE: This is called with the controller (not the stash) account id.
    pub fn on_offline_validator(controller: T::AccountId, count: usize) {

        if let Some(l) = Self::ledger(&controller) {
            let stash = l.stash;

            // Early exit if validator is invulnerable.
            if Self::invulnerables().contains(&stash) {
                return;
            }

            let slash_count = Self::slash_count(&stash);
            let new_slash_count = slash_count + count as u32;
            <SlashCount<T>>::insert(&stash, new_slash_count);
            let grace = Self::offline_slash_grace();

            if RECENT_OFFLINE_COUNT > 0 {
                let item = (stash.clone(), <system::Module<T>>::block_number(), count as u32);
                <RecentlyOffline<T>>::mutate(|v| if v.len() >= RECENT_OFFLINE_COUNT {
                    let index = v.iter()
                        .enumerate()
                        .min_by_key(|(_, (_, block, _))| block)
                        .expect("v is non-empty; qed")
                        .0;
                    v[index] = item;
                } else {
                    v.push(item);
                });
            }

            let prefs = Self::validators(&stash);
            let unstake_threshold = prefs.unstake_threshold.min(MAX_UNSTAKE_THRESHOLD);
            let max_slashes = grace + unstake_threshold;

            let event = if new_slash_count > max_slashes {
                let slash_exposure = Self::stakers(&stash).total;
                let offline_slash_base = Self::offline_slash() * slash_exposure;
                // They're bailing.
                let slash = offline_slash_base
                    // Multiply slash_mantissa by 2^(unstake_threshold with upper bound)
                    .checked_shl(unstake_threshold)
                    .map(|x| x.min(slash_exposure))
                    .unwrap_or(slash_exposure);
                let _ = Self::slash_validator(&stash, slash);
//                <ShouldOffline<T>>::mutate(|s| s.push(stash.clone()));
                <Validators<T>>::remove(&stash);
                let _ = <session::Module<T>>::disable(&controller);

                RawEvent::OfflineSlash(stash.clone(), slash)
            } else {
                RawEvent::OfflineWarning(stash.clone(), slash_count)
            };

            Self::deposit_event(event);
        }
    }
}

impl<T: Trait> OnSessionEnding<T::AccountId> for Module<T> {
    fn on_session_ending(i: SessionIndex) -> Option<Vec<T::AccountId>> {
        Self::new_session(i + 1)
    }
}

impl<T: Trait> OnFreeBalanceZero<T::AccountId> for Module<T> {
    fn on_free_balance_zero(stash: &T::AccountId) {
        if let Some(controller) = <Bonded<T>>::take(stash) {
            <Ledger<T>>::remove(&controller);
        }
        <Payee<T>>::remove(stash);
        <SlashCount<T>>::remove(stash);
        <Validators<T>>::remove(stash);
        <Nominators<T>>::remove(stash);
    }
}
