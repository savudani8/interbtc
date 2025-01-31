use super::*;
use frame_support::{pallet_prelude::StorageVersion, traits::OnRuntimeUpgrade};

#[cfg(feature = "try-runtime")]
use sp_std::vec::Vec;

/// The log target.
const TARGET: &'static str = "runtime::vault_registry::migration";

pub mod vault_capacity {
    use super::*;
    use frame_support::pallet_prelude::*;

    type SignedFixedPoint<T> = <T as currency::Config>::SignedFixedPoint;

    fn clear_reward_storage<T: Config>(mut weight: Weight, item: &str) {
        let res = frame_support::migration::clear_storage_prefix(b"VaultRewards", item.as_bytes(), b"", None, None);
        weight.saturating_accrue(T::DbWeight::get().writes(res.backend.into()));

        log::info!(
            target: TARGET,
            "Cleared '{}' entries from '{item}' storage prefix",
            res.unique
        );

        if res.maybe_cursor.is_some() {
            log::error!(target: TARGET, "Storage prefix '{item}' is not completely cleared");
        }
    }

    #[derive(Debug, Encode, Decode)]
    struct RewardsState<SignedFixedPoint> {
        stake_entries: u32,
        total_rewards_native: SignedFixedPoint,
        total_rewards_wrapped: SignedFixedPoint,
    }

    pub struct RewardsMigration<Runtime, VaultCapacityInstance, VaultRewardsInstance>(
        sp_std::marker::PhantomData<(Runtime, VaultCapacityInstance, VaultRewardsInstance)>,
    );

    impl<
            Runtime: Config
                + reward::Config<
                    VaultCapacityInstance,
                    PoolId = (),
                    StakeId = CurrencyId<Runtime>,
                    CurrencyId = CurrencyId<Runtime>,
                    SignedFixedPoint = SignedFixedPoint<Runtime>,
                > + reward::Config<
                    VaultRewardsInstance,
                    PoolId = CurrencyId<Runtime>,
                    StakeId = DefaultVaultId<Runtime>,
                    CurrencyId = CurrencyId<Runtime>,
                    SignedFixedPoint = SignedFixedPoint<Runtime>,
                > + staking::Config<CurrencyId = CurrencyId<Runtime>, SignedFixedPoint = SignedFixedPoint<Runtime>>,
            VaultCapacityInstance: 'static,
            VaultRewardsInstance: 'static,
        > OnRuntimeUpgrade for RewardsMigration<Runtime, VaultCapacityInstance, VaultRewardsInstance>
    {
        #[cfg(feature = "try-runtime")]
        fn pre_upgrade() -> Result<Vec<u8>, &'static str> {
            let prev_count = reward::migration::v0::Stake::<Runtime, VaultRewardsInstance>::iter().count();
            log::info!(target: TARGET, "{} stake entries will be migrated", prev_count);

            let prev_count_nonzero = reward::migration::v0::Stake::<Runtime, VaultRewardsInstance>::iter()
                .filter(|(_id, amount)| !amount.is_zero())
                .count();
            log::info!(
                target: TARGET,
                "{prev_count_nonzero} non-zero stake entries will be migrated"
            );
            let num_vaults = crate::Vaults::<Runtime>::iter().count();
            log::info!(target: TARGET, "{num_vaults} vaults in the system");

            Ok(RewardsState {
                stake_entries: prev_count as u32,
                total_rewards_native: reward::TotalRewards::<Runtime, VaultRewardsInstance>::get(
                    <Runtime as currency::Config>::GetNativeCurrencyId::get(),
                )
                .saturating_add(
                    staking::TotalRewards::<Runtime>::iter_prefix_values(
                        <Runtime as currency::Config>::GetNativeCurrencyId::get(),
                    )
                    .reduce(|a, b| a.saturating_add(b))
                    .unwrap_or_default(),
                ),
                total_rewards_wrapped: reward::TotalRewards::<Runtime, VaultRewardsInstance>::get(
                    <Runtime as currency::Config>::GetWrappedCurrencyId::get(),
                )
                .saturating_add(
                    staking::TotalRewards::<Runtime>::iter_prefix_values(
                        <Runtime as currency::Config>::GetWrappedCurrencyId::get(),
                    )
                    .reduce(|a, b| a.saturating_add(b))
                    .unwrap_or_default(),
                ),
            }
            .encode())
        }

        fn on_runtime_upgrade() -> Weight {
            // NOTE: using substrate storage version instead of custom
            let version = StorageVersion::get::<Pallet<Runtime>>();
            if version != 0 {
                log::warn!(
                    target: TARGET,
                    "skipping v0 to v1 migration: executed on wrong storage version.\
            				Expected version 0, found {:?}",
                    version,
                );
                return Runtime::DbWeight::get().reads(1);
            }

            log::info!(target: TARGET, "Running migration...");

            let mut weight = Runtime::DbWeight::get().reads_writes(2, 1);

            // withdraw all rewards for all vaults
            for (vault_id, _) in Vaults::<Runtime>::iter() {
                weight.saturating_accrue(Runtime::DbWeight::get().reads(1));

                for currency_id in [
                    vault_id.wrapped_currency(),
                    <Runtime as currency::Config>::GetNativeCurrencyId::get(),
                ] {
                    let reward =
                        reward::migration::v0::compute_reward::<Runtime, VaultRewardsInstance>(&vault_id, currency_id)
                            .unwrap_or_default();
                    // reward::v0::Stake (VaultRewards) - 1 read
                    // reward::v0::RewardPerToken (VaultRewards) - 1 read
                    // reward::v0::RewardTally (VaultRewards) - 1 read
                    weight.saturating_accrue(Runtime::DbWeight::get().reads(3));
                    // NOTE: ignoring commission since nomination is not yet enabled
                    if let Err(err) = staking::Pallet::<Runtime>::distribute_reward(currency_id, &vault_id, reward) {
                        // TODO: accrue weight still?
                        log::error!(target: TARGET, "skipping error: {:?}", err);
                    } else {
                        // staking::Nonce - 1 read
                        // staking::TotalCurrentStake - 1 read
                        // staking::RewardPerToken - 1 read, 1 write
                        // staking::TotalRewards - 1 read, 1 write
                        weight.saturating_accrue(Runtime::DbWeight::get().reads_writes(4, 2));
                    }
                }
            }

            // TODO: do we want to do this now or later? as this
            // is potentially expensive we could get away with
            // only clearing select storage items
            clear_reward_storage::<Runtime>(weight, "TotalStake");
            clear_reward_storage::<Runtime>(weight, "TotalRewards");
            clear_reward_storage::<Runtime>(weight, "RewardPerToken");
            clear_reward_storage::<Runtime>(weight, "Stake");
            clear_reward_storage::<Runtime>(weight, "RewardTally");

            for (vault_id, _) in Vaults::<Runtime>::iter() {
                weight.saturating_accrue(Runtime::DbWeight::get().reads(1));

                let total_collateral = ext::staking::total_current_stake::<Runtime>(&vault_id).unwrap();
                let secure_threshold = Pallet::<Runtime>::get_vault_secure_threshold(&vault_id).unwrap();
                let expected_stake = total_collateral.checked_div(&secure_threshold).unwrap();

                log::info!(target: TARGET, "Setting stake to {:?}", expected_stake.amount());

                // TODO: handle error, this is fatal
                pool_manager::PoolManager::<Runtime>::update_reward_stake(&vault_id).unwrap();

                let stake_entries_after: u32 = reward::Stake::<Runtime, VaultRewardsInstance>::iter().count() as u32;
                log::info!(target: TARGET, "Now: {:?}", stake_entries_after);

                // staking::TotalStake - 1 read
                // vault_registry::Vaults - 1 read
                // vault_registry::SecureCollateralThreshold - 1 read
                // reward::Stake (VaultRewards) - 1 read, 1 write
                // reward::TotalStake (VaultRewards) - 1 read, 1 write
                // reward::RewardTally (VaultRewards) - 1 read, 1 write
                // reward::RewardPerToken (VaultRewards) - 1 read
                // reward::TotalStake (VaultRewards) - 1 write
                // oracle::Aggregate - 1 read
                // reward::Stake (CapacityRewards) - 1 read, 1 write
                // reward::TotalStake (CapacityRewards) - 1 read, 1 write
                // reward::RewardTally (CapacityRewards) - 1 read, 1 write
                // reward::RewardPerToken (CapacityRewards) - 1 read
                weight.saturating_accrue(Runtime::DbWeight::get().reads_writes(12, 7));
            }

            log::info!(target: TARGET, "Finished migration...");

            StorageVersion::new(1).put::<Pallet<Runtime>>();
            weight.saturating_add(Runtime::DbWeight::get().reads_writes(1, 2))
        }

        #[cfg(feature = "try-runtime")]
        fn post_upgrade(state: Vec<u8>) -> Result<(), &'static str> {
            let rewards_state: RewardsState<SignedFixedPoint<Runtime>> =
                Decode::decode(&mut state.as_slice()).expect("invalid state generated by pre_upgrade");

            let stake_entries_after: u32 = reward::Stake::<Runtime, VaultRewardsInstance>::iter().count() as u32;
            log::info!(
                target: TARGET,
                "number of stake entries after: {:?}",
                stake_entries_after
            );
            // ensure!(
            //     stake_entries_after == rewards_state.stake_entries,
            //     "Not all entries were migrated"
            // );

            ensure!(
                reward::TotalRewards::<Runtime, VaultRewardsInstance>::get(
                    <Runtime as currency::Config>::GetNativeCurrencyId::get()
                ) == Zero::zero(),
                "All rewards should be zero"
            );
            ensure!(
                reward::TotalRewards::<Runtime, VaultRewardsInstance>::get(
                    <Runtime as currency::Config>::GetWrappedCurrencyId::get()
                ) == Zero::zero(),
                "All rewards should be zero"
            );

            let native_staking_rewards_after = staking::TotalRewards::<Runtime>::iter_prefix_values(
                <Runtime as currency::Config>::GetNativeCurrencyId::get(),
            )
            .reduce(|a, b| a.saturating_add(b))
            .unwrap_or_default();
            log::info!(
                target: TARGET,
                "rewards (native) before: {:?}, after: {:?}",
                rewards_state.total_rewards_native,
                native_staking_rewards_after,
            );
            // NOTE: check lt due to rounding errors
            ensure!(
                native_staking_rewards_after < rewards_state.total_rewards_native,
                "Previous rewards should be in staking"
            );

            let wrapped_staking_rewards_after = staking::TotalRewards::<Runtime>::iter_prefix_values(
                <Runtime as currency::Config>::GetWrappedCurrencyId::get(),
            )
            .reduce(|a, b| a.saturating_add(b))
            .unwrap_or_default();
            log::info!(
                target: TARGET,
                "rewards (wrapped) before: {:?}, after: {:?}",
                rewards_state.total_rewards_wrapped,
                wrapped_staking_rewards_after,
            );
            // NOTE: check lt due to rounding errors
            ensure!(
                wrapped_staking_rewards_after < rewards_state.total_rewards_wrapped,
                "Previous rewards should be in staking"
            );

            // check that totalUserVaultCollateral matches sum of stakes in staking pallet (plus liquidated collateral)
            for (currency_pair, expected_collateral) in crate::TotalUserVaultCollateral::<Runtime>::iter() {
                let amount_from_nominator_stakes = staking::Stake::<Runtime>::iter()
                    .filter_map(|(_nonce, (vault, nominator), _value)| {
                        if vault.collateral_currency() == currency_pair.collateral {
                            let value = ext::staking::compute_stake::<Runtime>(&vault, &nominator).unwrap();
                            Some(value)
                        } else {
                            None
                        }
                    })
                    .reduce(|a, b| a.saturating_add(b))
                    .unwrap_or_default();
                let amount_from_vault_liquidated_collateral = crate::Vaults::<Runtime>::iter()
                    .filter(|(key, _value)| key.currencies.collateral == currency_pair.collateral)
                    .map(|(_key, vault)| vault.liquidated_collateral)
                    .reduce(|a, b| a.saturating_add(b))
                    .unwrap_or_default();
                let amount_from_liquidation_vault = crate::LiquidationVault::<Runtime>::get(currency_pair)
                    .map(|x| x.collateral)
                    .unwrap_or_default();

                log::info!(
                    target: TARGET,
                    "TotalUserVaultCollateral: {:?}, sum(stakes): {:?} liquidated_collateral: {:?}, liquidation_vault: {:?}",
                    expected_collateral,
                    amount_from_nominator_stakes,
                    amount_from_vault_liquidated_collateral,
                    amount_from_liquidation_vault,
                );

                let actual = amount_from_nominator_stakes
                    + amount_from_vault_liquidated_collateral
                    + amount_from_liquidation_vault;
                let diff = if expected_collateral > actual {
                    expected_collateral - actual
                } else {
                    actual - expected_collateral
                };

                // allow it to be off by 100 planck
                assert!(diff <= 100u32.into());
            }

            // check that TotalCurrentStake matches sum of stakes in staking pallet
            for (_nonce, vault_id, total) in staking::TotalCurrentStake::<Runtime>::iter() {
                log::info!(target: TARGET, "total = {:?}", total);

                let expected =
                    Amount::<Runtime>::from_signed_fixed_point(total, vault_id.collateral_currency()).unwrap();

                let actual_stake = staking::Stake::<Runtime>::iter()
                    .filter_map(|(_nonce, (vault, nominator), _)| {
                        if vault_id == vault {
                            let stake = ext::staking::compute_stake::<Runtime>(&vault, &nominator).unwrap();
                            Some(stake)
                        } else {
                            None
                        }
                    })
                    .reduce(|a, b| a.saturating_add(b))
                    .unwrap_or_default();

                let diff = if expected.amount() > actual_stake {
                    expected.amount() - actual_stake
                } else {
                    actual_stake - expected.amount()
                };
                log::info!(
                    target: TARGET,
                    "expected = {:?}, actual = {:?}",
                    expected.amount(),
                    actual_stake
                );

                assert!(diff <= 1u32.into());
            }

            // check that reward pool stake matches minting capacity
            for (_key, vault) in crate::Vaults::<Runtime>::iter() {
                let vault_id = &vault.id;
                let total_collateral = ext::staking::total_current_stake::<Runtime>(vault_id).unwrap();
                let secure_threshold = Pallet::<Runtime>::get_vault_secure_threshold(vault_id).unwrap();
                let expected_stake = total_collateral.checked_div(&secure_threshold).unwrap();
                let actual_stake =
                    reward::Stake::<Runtime, VaultRewardsInstance>::get((vault_id.collateral_currency(), vault_id));
                let actual_stake =
                    Amount::<Runtime>::from_signed_fixed_point(actual_stake, vault_id.collateral_currency()).unwrap();
                assert_eq!(expected_stake.amount(), actual_stake.amount());
            }

            // check that reward::TotalStake matches the total of the individual stakes
            for (currency, total) in reward::TotalStake::<Runtime, VaultRewardsInstance>::iter() {
                let total_individual_stakes = reward::Stake::<Runtime, VaultRewardsInstance>::iter()
                    .filter_map(
                        |((pool_id, _), stake)| {
                            if pool_id == currency {
                                Some(stake)
                            } else {
                                None
                            }
                        },
                    )
                    .reduce(|a, b| a.saturating_add(b))
                    .unwrap_or_default();

                assert_eq!(total, total_individual_stakes);
            }

            // check that vault capacity reward stakes match the vault rewards total stakes
            for (((), currency), capacity_stake) in reward::Stake::<Runtime, VaultCapacityInstance>::iter() {
                let wrapped_currency_id = <Runtime as currency::Config>::GetWrappedCurrencyId::get();
                let capacity_stake_amount =
                    Amount::<Runtime>::from_signed_fixed_point(capacity_stake, wrapped_currency_id).unwrap();

                let total_reward_stake = ext::reward::total_current_stake::<Runtime>(currency)?;
                let total_reward_stake_amount = total_reward_stake.convert_to(wrapped_currency_id)?;

                assert_eq!(capacity_stake_amount.amount(), total_reward_stake_amount.amount());
            }

            Ok(())
        }
    }
}

#[cfg(test)]
#[cfg(feature = "try-runtime")]
mod test {
    use super::*;
    use crate::mock::*;
    use frame_support::assert_ok;

    const DEFAULT_REWARDS_CURRENCY: mock::CurrencyId = DEFAULT_WRAPPED_CURRENCY;

    fn register_old_vault(vault_id: DefaultVaultId<Test>) {
        let vault = Vault::new(vault_id.clone());
        VaultRegistry::insert_vault(&vault_id, vault);
        assert_ok!(staking::Pallet::<Test>::deposit_stake(
            &vault_id,
            &vault_id.account_id,
            1000.into() // collateral
        ));
        assert_ok!(reward::migration::v0::deposit_stake::<Test, VaultRewardsInstance>(
            &vault_id,
            100.into() // issued tokens
        ));
    }

    #[test]
    #[allow(deprecated)]
    fn migration_works() {
        run_test(|| {
            // assume that we are at v0
            StorageVersion::new(0).put::<VaultRegistry>();

            register_old_vault(DEFAULT_ID);

            // distribute rewards to old vault
            assert_ok!(reward::migration::v0::distribute_reward::<Test, VaultRewardsInstance>(
                DEFAULT_REWARDS_CURRENCY,
                100.into()
            ));
            assert_ok!(
                reward::migration::v0::compute_reward::<Test, VaultRewardsInstance>(
                    &DEFAULT_ID,
                    DEFAULT_REWARDS_CURRENCY
                ),
                100.into()
            );

            // no staking rewards prior to migration
            assert_ok!(
                staking::Pallet::<Test>::compute_reward(DEFAULT_REWARDS_CURRENCY, &DEFAULT_ID, &DEFAULT_ID.account_id),
                0.into()
            );

            Oracle::_set_exchange_rate(DEFAULT_COLLATERAL_CURRENCY, mock::UnsignedFixedPoint::from_float(0.1)).unwrap();

            let state = vault_capacity::RewardsMigration::<Test, VaultRewardsInstance>::pre_upgrade().unwrap();
            let _w = vault_capacity::RewardsMigration::<Test, VaultRewardsInstance>::on_runtime_upgrade();
            assert_ok!(vault_capacity::RewardsMigration::<Test, VaultRewardsInstance>::post_upgrade(state));

            assert_eq!(
                reward::migration::v0::Stake::<Test, VaultRewardsInstance>::get(&DEFAULT_ID),
                0.into()
            );
            // check old rewards are zero
            assert_ok!(
                reward::migration::v0::compute_reward::<Test, VaultRewardsInstance>(
                    &DEFAULT_ID,
                    DEFAULT_REWARDS_CURRENCY
                ),
                0.into()
            );

            // Stake = Collateral / Threshold
            // 500 = 1000 / 2
            assert_eq!(
                reward::Stake::<Test, VaultRewardsInstance>::get((DEFAULT_COLLATERAL_CURRENCY, DEFAULT_ID)),
                500.into()
            );

            // Stake = SUM(Collateral / Threshold) / Price
            // 5000 = 500 / 0.1
            assert_eq!(
                reward::Stake::<Test, CapacityRewardsInstance>::get(((), DEFAULT_COLLATERAL_CURRENCY)),
                5000.into()
            );

            // migration distributes previous rewards
            assert_ok!(
                staking::Pallet::<Test>::compute_reward(DEFAULT_REWARDS_CURRENCY, &DEFAULT_ID, &DEFAULT_ID.account_id),
                100.into()
            );

            assert_eq!(StorageVersion::get::<VaultRegistry>(), 1);
        });
    }
}
