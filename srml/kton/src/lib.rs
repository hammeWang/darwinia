#![cfg_attr(not(feature = "std"), no_std)]

use parity_codec::{Codec, Decode, Encode};
use primitives::traits::{
    Bounded, CheckedAdd, CheckedSub, MaybeSerializeDebug, Member, Saturating,
    SimpleArithmetic, StaticLookup, Zero,
};
use rstd::{cmp, result};
use rstd::prelude::*;

use srml_support::{decl_event, decl_module, decl_storage, Parameter, StorageMap, StorageValue};
use srml_support::dispatch::Result;
use srml_support::traits::{
    Currency, ExistenceRequirement, Imbalance, LockableCurrency, LockIdentifier,
    OnUnbalanced, SignedImbalance, UpdateBalanceOutcome,
    WithdrawReason, WithdrawReasons,
};
use system::ensure_signed;

// customed
use imbalance::{NegativeImbalance, PositiveImbalance};

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

mod imbalance;

/// Struct to encode the vesting schedule of an individual account.
#[derive(Encode, Decode, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct VestingSchedule<Balance> {
    /// Locked amount at genesis.
    pub offset: Balance,
    /// Amount that gets unlocked every block from genesis.
    pub per_block: Balance,
}

impl<Balance: SimpleArithmetic + Copy> VestingSchedule<Balance> {
    /// Amount locked at block `n`.
    pub fn locked_at<BlockNumber>(&self, n: BlockNumber) -> Balance
        where Balance: From<BlockNumber>
    {
        if let Some(x) = Balance::from(n).checked_mul(&self.per_block) {
            self.offset.max(x) - x
        } else {
            Zero::zero()
        }
    }
}

#[derive(Encode, Decode, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct BalanceLock<Balance, BlockNumber> {
    pub id: LockIdentifier,
    pub amount: Balance,
    pub until: BlockNumber,
    pub reasons: WithdrawReasons,
}

pub trait Trait: timestamp::Trait {
    type Balance: Parameter + Member + SimpleArithmetic + Codec + Default + Copy +
    MaybeSerializeDebug + From<Self::BlockNumber>;

    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

    // kton
    type OnMinted: OnUnbalanced<PositiveImbalance<Self>>;
    type OnRemoval: OnUnbalanced<NegativeImbalance<Self>>;
}

decl_event!(
    pub enum Event<T> where
        < T as system::Trait>::AccountId,
        < T as Trait>::Balance,
    {
        /// Transfer succeeded (from, to, value, fees).
        TokenTransfer(AccountId, AccountId, Balance),
    }
);


decl_storage! {
	trait Store for Module<T: Trait> as Kton {

        /// For Currency and LockableCurrency Trait
		/// The total `units issued in the system.
		// like `existential_deposit`, but always set to 0
		pub MinimumBalance get(minimum_balance): T::Balance = 0.into();

		pub TotalIssuance get(total_issuance) build(|config: &GenesisConfig<T>| {
			config.balances.iter().fold(Zero::zero(), |acc: T::Balance, &(_, n)| acc + n)
		}): T::Balance;

		pub FreeBalance get(free_balance) build(|config: &GenesisConfig<T>| config.balances.clone()):
			map T::AccountId => T::Balance;

		pub ReservedBalance get(reserved_balance): map T::AccountId => T::Balance;

		pub Locks get(locks): map T::AccountId => Vec<BalanceLock<T::Balance, T::BlockNumber>>;

		pub TotalLock get(total_lock): T::Balance;

		pub Vesting get(vesting) build(|config: &GenesisConfig<T>| {
			config.vesting.iter().filter_map(|&(ref who, begin, length)| {
				let begin = <T::Balance as From<T::BlockNumber>>::from(begin);
				let length = <T::Balance as From<T::BlockNumber>>::from(length);

				config.balances.iter()
					.find(|&&(ref w, _)| w == who)
					.map(|&(_, balance)| {
						// <= begin it should be >= balance
						// >= begin+length it should be <= 0

						let per_block = balance / length.max(primitives::traits::One::one());
						let offset = begin * per_block + balance;

						(who.clone(), VestingSchedule { offset, per_block })
					})
			}).collect::<Vec<_>>()
		}): map T::AccountId => Option<VestingSchedule<T::Balance>>;
	}
	add_extra_genesis {
        config(balances): Vec<(T::AccountId, T::Balance)>;
        config(vesting): Vec<(T::AccountId, T::BlockNumber, T::BlockNumber)>;		// begin, length
}
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        fn deposit_event<T>() = default;

        pub fn transfer(origin,
            dest: <T::Lookup as StaticLookup>::Source,
			#[compact] value: T::Balance
		) {
			let transactor = ensure_signed(origin)?;
			let dest = T::Lookup::lookup(dest)?;

            <Self as Currency<_>>::transfer(&transactor, &dest, value)?;
        }
    }
}

impl<T: Trait> Module<T> {
    pub fn vesting_balance(who: &T::AccountId) -> T::Balance {
        if let Some(v) = Self::vesting(who) {
            Self::free_balance(who)
                .min(v.locked_at::<T::BlockNumber>(<system::Module<T>>::block_number()))
        } else {
            Zero::zero()
        }
    }


    // PRIVATE MUTABLE
    // NOTE: different from balances module
    fn set_free_balance(who: &T::AccountId, balance: T::Balance) -> UpdateBalanceOutcome {
        //TODO: check the value of balance, but no ensure!(...)
        <FreeBalance<T>>::insert(who, balance);
        UpdateBalanceOutcome::Updated
    }

    fn set_reserved_balance(who: &T::AccountId, balance: T::Balance) -> UpdateBalanceOutcome {
        <ReservedBalance<T>>::insert(who, balance);
        UpdateBalanceOutcome::Updated
    }
}


impl<T: Trait> Currency<T::AccountId> for Module<T> {
    type Balance = T::Balance;
    type PositiveImbalance = PositiveImbalance<T>;
    type NegativeImbalance = NegativeImbalance<T>;

    fn total_balance(who: &T::AccountId) -> Self::Balance {
        Self::free_balance(who) + Self::reserved_balance(who)
    }

    fn can_slash(who: &T::AccountId, value: Self::Balance) -> bool {
        Self::free_balance(who) >= value
    }

    fn total_issuance() -> Self::Balance {
        Self::total_issuance()
    }

    fn minimum_balance() -> Self::Balance {
        Self::minimum_balance()
    }

    fn free_balance(who: &T::AccountId) -> Self::Balance {
        <FreeBalance<T>>::get(who)
    }

    fn ensure_can_withdraw(
        who: &T::AccountId,
        _amount: T::Balance,
        reason: WithdrawReason,
        new_balance: T::Balance,
    ) -> Result {
        match reason {
            WithdrawReason::Reserve | WithdrawReason::Transfer if Self::vesting_balance(who) > new_balance =>
                return Err("vesting balance too high to send value"),
            _ => {}
        }
        let locks = Self::locks(who);
        if locks.is_empty() {
            return Ok(());
        }

        let now = <system::Module<T>>::block_number();
        if locks.into_iter()
            .all(|l|
                now >= l.until
                    || new_balance >= l.amount
                    || !l.reasons.contains(reason)
            )
        {
            Ok(())
        } else {
            Err("account liquidity restrictions prevent withdrawal")
        }
    }


    // TODO: add fee
    fn transfer(transactor: &T::AccountId, dest: &T::AccountId, value: Self::Balance) -> Result {
        let from_balance = Self::free_balance(transactor);
        let to_balance = Self::free_balance(dest);

        let new_from_balance = match from_balance.checked_sub(&value) {
            None => return Err("balance too low to send value"),
            Some(b) => b,
        };

        Self::ensure_can_withdraw(transactor, value, WithdrawReason::Transfer, new_from_balance)?;

        // NOTE: total stake being stored in the same type means that this could never overflow
        // but better to be safe than sorry.
        let new_to_balance = match to_balance.checked_add(&value) {
            Some(b) => b,
            None => return Err("destination balance too high to receive value"),
        };

        if transactor != dest {
            Self::set_free_balance(transactor, new_from_balance);
            Self::set_free_balance(dest, new_to_balance);
        }

        Self::deposit_event(RawEvent::TokenTransfer(transactor.clone(), dest.clone(), value));
        Ok(())
    }


    fn withdraw(
        who: &T::AccountId,
        value: Self::Balance,
        reason: WithdrawReason,
        liveness: ExistenceRequirement,
    ) -> result::Result<Self::NegativeImbalance, &'static str> {
        let old_balance = Self::free_balance(who);
        if let Some(new_balance) = old_balance.checked_sub(&value) {
            if liveness == ExistenceRequirement::KeepAlive && new_balance < Self::minimum_balance() {
                return Err("payment would kill account");
            }

            Self::ensure_can_withdraw(who, value, reason, new_balance)?;
            Self::set_free_balance(who, new_balance);
            Ok(NegativeImbalance::new(value))
        } else {
            Err("too few free funds in account")
        }
    }


    fn slash(
        who: &T::AccountId,
        value: Self::Balance,
    ) -> (Self::NegativeImbalance, Self::Balance) {
        let free_balance = Self::free_balance(who);
        let free_slash = cmp::min(free_balance, value);

        let new_balance = free_balance - free_slash;

        Self::set_free_balance(who, new_balance);
        let remaining_slash = value - free_slash;

        if !remaining_slash.is_zero() {
            let reserved_balance = Self::reserved_balance(who);
            let reserved_slash = cmp::min(reserved_balance, remaining_slash);
            Self::set_reserved_balance(who, reserved_balance - reserved_slash);
            (NegativeImbalance::new(free_slash + reserved_slash), remaining_slash - reserved_slash)
        } else {
            (NegativeImbalance::new(value), Zero::zero())
        }
    }

    fn deposit_into_existing(
        who: &T::AccountId,
        value: Self::Balance,
    ) -> result::Result<Self::PositiveImbalance, &'static str> {
        if Self::total_balance(who).is_zero() {
            return Err("beneficiary account must pre-exist");
        }
        //add here 
        let old_balance = Self::free_balance(who);
        let new_balance = old_balance + value;

        Self::set_free_balance(who, new_balance);
        Ok(PositiveImbalance::new(value))
    }

    fn deposit_creating(
        who: &T::AccountId,
        value: Self::Balance,
    ) -> Self::PositiveImbalance {

        let old_balance = Self::free_balance(who);
        let new_balance = old_balance + value;

        let (imbalance, _) = Self::make_free_balance_be(who, new_balance);

        if let SignedImbalance::Positive(p) = imbalance {
            p
        } else {
            // Impossible, but be defensive.
            Self::PositiveImbalance::zero()
        }
    }

    fn make_free_balance_be(who: &T::AccountId, balance: Self::Balance) -> (
        SignedImbalance<Self::Balance, Self::PositiveImbalance>,
        UpdateBalanceOutcome
    ) {
        let original = Self::free_balance(who);

        let imbalance = if original <= balance {
            SignedImbalance::Positive(PositiveImbalance::new(balance - original))
        } else {
            SignedImbalance::Negative(NegativeImbalance::new(original - balance))
        };

        let outcome = {
            Self::set_free_balance(who, balance);
            UpdateBalanceOutcome::Updated
        };

        (imbalance, outcome)
    }

    // TODO: ready for hacking
    fn burn(mut amount: Self::Balance) -> Self::PositiveImbalance {
        <TotalIssuance<T>>::mutate(|issued|
            issued.checked_sub(&amount).unwrap_or_else(|| {
                amount = *issued;
                Zero::zero()
            })
        );
        PositiveImbalance::new(amount)
    }

    // TODO: ready for hacking
    fn issue(mut amount: Self::Balance) -> Self::NegativeImbalance {
        <TotalIssuance<T>>::mutate(|issued|
            *issued = issued.checked_add(&amount).unwrap_or_else(|| {
                amount = Self::Balance::max_value() - *issued;
                Self::Balance::max_value()
            })
        );
        NegativeImbalance::new(amount)
    }
}


impl<T: Trait> LockableCurrency<T::AccountId> for Module<T>
    where
        T::Balance: MaybeSerializeDebug
{
    type Moment = T::BlockNumber;

    fn set_lock(
        id: LockIdentifier,
        who: &T::AccountId,
        amount: T::Balance,
        until: T::BlockNumber,
        reasons: WithdrawReasons,
    ) {
        let now = <system::Module<T>>::block_number();
        let mut new_lock = Some(BalanceLock { id, amount, until, reasons });
        let mut locks = Self::locks(who).into_iter().filter_map(|l|
            if l.id == id {
                new_lock.take()
            } else if l.until > now {
                Some(l)
            } else {
                None
            }).collect::<Vec<_>>();
        if let Some(lock) = new_lock {
            locks.push(lock)
        }
        <Locks<T>>::insert(who, locks);
    }

    fn extend_lock(
        id: LockIdentifier,
        who: &T::AccountId,
        amount: T::Balance,
        until: T::BlockNumber,
        reasons: WithdrawReasons,
    ) {
        let now = <system::Module<T>>::block_number();
        let mut new_lock = Some(BalanceLock { id, amount, until, reasons });
        let mut locks = Self::locks(who).into_iter().filter_map(|l|
            if l.id == id {
                new_lock.take().map(|nl| {
                    BalanceLock {
                        id: l.id,
                        amount: l.amount.max(nl.amount),
                        until: l.until.max(nl.until),
                        reasons: l.reasons | nl.reasons,
                    }
                })
            } else if l.until > now {
                Some(l)
            } else {
                None
            }).collect::<Vec<_>>();
        if let Some(lock) = new_lock {
            locks.push(lock)
        }
        <Locks<T>>::insert(who, locks);
    }

    fn remove_lock(
        id: LockIdentifier,
        who: &T::AccountId,
    ) {
        let now = <system::Module<T>>::block_number();
        let locks = Self::locks(who).into_iter().filter_map(|l|
            if l.until > now && l.id != id {
                Some(l)
            } else {
                None
            }).collect::<Vec<_>>();
        <Locks<T>>::insert(who, locks);
    }
}

