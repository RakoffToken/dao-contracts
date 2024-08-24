use cw20::Cw20ExecuteMsg;
use cw_storage_plus::Map;
use cosmwasm_std::{to_json_binary, Addr, BankMsg, Coin, CosmosMsg, Decimal, Order, StdError, Storage, Uint128, WasmMsg};
use cw_denom::CheckedDenom;

pub struct MassDistribute<'a> {
    pub weights: Map<'a, String, Decimal>,
}

impl<'a> MassDistribute<'a> {
    pub const fn new(storage_key: &'a str) -> Self {
        Self {
            weights: Map::new(storage_key),
        }
    }

    pub fn set_weights(&self, store: &mut dyn Storage, input: &Vec<(String, Decimal)>) -> Result<(), StdError> {
        // weights must add up to exactly 1
        let total_weight: Decimal = input.iter().map(|(_, weight)| weight).sum();
        if total_weight != Decimal::one() {
            return Err(StdError::generic_err(String::from("weights must add up to exactly 1")));
        }
        for (address, weight) in input {
            self.weights.save(store, address.clone(), weight).unwrap();
        }
        return Ok(());
    }

    pub fn get_share(&self, store: &dyn Storage, address: String, amount: Uint128) -> Uint128 {
        let weight = self.weights.load(store, address).unwrap_or(Decimal::zero());
        return amount * weight;
    }

    pub fn distribute_cw20(&self, store: &dyn Storage, amount: Uint128, token: Addr) -> Vec<CosmosMsg>{
        self.weights.keys(store, None, None, Order::Ascending).
            into_iter().
            map(|address| -> CosmosMsg {
                let recipient = address.unwrap();
                let share = self.get_share(store, recipient.clone(), amount);
                let transfer_msg = to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: recipient.clone(),
                    amount: share,
                }).unwrap();
                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: token.to_string().clone(),
                    msg: transfer_msg,
                    funds: vec![],
                })
            }).
            collect::<Vec<_>>()
    }

    pub fn distribute_native(&self, store: &dyn Storage, amount: Uint128, denom: String) -> Vec<CosmosMsg> {
        self.weights.keys(store, None, None, Order::Ascending).
            into_iter().
            map(|address| -> CosmosMsg {
                let recipient = address.unwrap();
                let share = self.get_share(store, recipient.clone(), amount);
                CosmosMsg::Bank(BankMsg::Send {
                    to_address: recipient.clone(),
                    amount: vec![ Coin{
                        amount: share,
                        denom: denom.clone()
                    }],
                })
            }).
            collect::<Vec<_>>()
    }

    pub fn distribute(&self, store: &dyn Storage, amount: Uint128, denom: CheckedDenom) -> Vec<CosmosMsg> {
        match denom {
            CheckedDenom::Cw20(token) => self.distribute_cw20(store, amount, token),
            CheckedDenom::Native(denom) => self.distribute_native(store, amount, denom),
        }
    }
}

#[cfg(test)]
mod test {
    use cosmwasm_std::{testing::MockStorage, to_json_binary, Addr, CosmosMsg, Decimal, WasmMsg};
    use cw20::Cw20ExecuteMsg;

    use crate::mass_distribute::MassDistribute;

    #[test]
    fn successful_set_weights() {
        let mut store = MockStorage::new();
        let mass_distribute = MassDistribute::new("weights");
        let input = vec![
            (String::from("addr1"), Decimal::percent(50)),
            (String::from("addr2"), Decimal::percent(50)),
        ];
        let result = mass_distribute.set_weights(&mut store, &input);
        assert!(result.is_ok());
        assert_eq!(Decimal::percent(50), mass_distribute.weights.load(&store, String::from("addr1")).unwrap());
        assert_eq!(Decimal::percent(50), mass_distribute.weights.load(&store, String::from("addr2")).unwrap());
    }

    #[test]
    fn invalid_set_weights() {
        let mut store = MockStorage::new();
        let mass_distribute = MassDistribute::new("weights");
        let input = vec![
            (String::from("addr1"), Decimal::percent(50)),
            (String::from("addr2"), Decimal::percent(60)),
        ];
        let result = mass_distribute.set_weights(&mut store, &input);
        assert!(result.is_err());
    }

    #[test]
    fn correct_distribution_amounts_native() {
        let mut store = MockStorage::new();
        let mass_distribute = MassDistribute::new("weights");
        let input = vec![
            (String::from("addr1"), Decimal::percent(60)),
            (String::from("addr2"), Decimal::percent(30)),
            (String::from("addr3"), Decimal::percent(10)),
        ];
        let expected = vec![
            CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                to_address: String::from("addr1"),
                amount: vec![cosmwasm_std::Coin {
                    denom: String::from("uusd"),
                    amount: 60u128.into(),
                }],
            }),
            CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                to_address: String::from("addr2"),
                amount: vec![cosmwasm_std::Coin {
                    denom: String::from("uusd"),
                    amount: 30u128.into(),
                }],
            }),
            CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                to_address: String::from("addr3"),
                amount: vec![cosmwasm_std::Coin {
                    denom: String::from("uusd"),
                    amount: 10u128.into(),
                }],
            }),
        ];
        mass_distribute.set_weights(&mut store, &input).unwrap();
        let result = mass_distribute.distribute(&store, 100u128.into(), cw_denom::CheckedDenom::Native(String::from("uusd")));
        assert_eq!(3, result.len());
        assert_eq!(result, expected)
    }

    #[test]
    fn correct_distribution_amounts_cw20() {
        let mut store = MockStorage::new();
        let mass_distribute = MassDistribute::new("weights");
        let input = vec![
            (String::from("addr1"), Decimal::percent(60)),
            (String::from("addr2"), Decimal::percent(30)),
            (String::from("addr3"), Decimal::percent(10)),
        ];
        let expected = vec![
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: String::from("token"),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: String::from("addr1"),
                    amount: 60u128.into(),
                }).unwrap(),
                funds: vec![],
            }),
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: String::from("token"),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: String::from("addr2"),
                    amount: 30u128.into(),
                }).unwrap(),
                funds: vec![],
            }),
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: String::from("token"),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: String::from("addr3"),
                    amount: 10u128.into(),
                }).unwrap(),
                funds: vec![],
            }),
        ];
        mass_distribute.set_weights(&mut store, &input).unwrap();
        let result = mass_distribute.distribute(&store, 100u128.into(), cw_denom::CheckedDenom::Cw20(Addr::unchecked("token")));
        assert_eq!(3, result.len());
        assert_eq!(result, expected)
    }

}