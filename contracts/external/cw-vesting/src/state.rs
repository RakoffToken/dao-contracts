use cw_storage_plus::Item;

use crate::vesting::Payment;
use crate::mass_distribute::MassDistribute;

pub const PAYMENT: Payment = Payment::new("vesting", "staked", "validator", "cardinality");
pub const UNBONDING_DURATION_SECONDS: Item<u64> = Item::new("ubs");
pub const MASS_DISTRIBUTE: MassDistribute = MassDistribute::new("weights");
