#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_json, to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Empty, Env,
    MessageInfo, Response, StdError, StdResult, Uint128, Uint256, WasmMsg,
};
use cw2::set_contract_version;
use cw20::Denom::Cw20;
use cw20::{Cw20ReceiveMsg, Denom};
use cw4::MemberChangedHookMsg;
use dao_hooks::{nft_stake::NftStakeChangedHookMsg, stake::StakeChangedHookMsg};
use dao_interface::voting::{
    Query as VotingQueryMsg, TotalPowerAtHeightResponse, VotingPowerAtHeightResponse,
};
use std::cmp::min;
use std::convert::TryInto;

use crate::msg::{
    ExecuteMsg, InfoResponse, InstantiateMsg, PendingRewardsResponse, QueryMsg, ReceiveMsg,
};
use crate::state::{
    Config, RewardConfig, CONFIG, LAST_UPDATE_BLOCK, PENDING_REWARDS, REWARD_CONFIG,
    REWARD_PER_TOKEN, USER_REWARD_PER_TOKEN,
};
use crate::ContractError;
use crate::ContractError::{
    InvalidCw20, InvalidFunds, NoRewardsClaimable, RewardPeriodNotFinished,
};

const CONTRACT_NAME: &str = env!("CARGO_PKG_NAME");
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response<Empty>, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    cw_ownable::initialize_owner(deps.storage, deps.api, msg.owner.as_deref())?;

    let reward_token = match msg.reward_token {
        Denom::Native(denom) => Denom::Native(denom),
        Cw20(addr) => Cw20(deps.api.addr_validate(addr.as_ref())?),
    };

    // Verify contract provided is a voting module contract
    let _: TotalPowerAtHeightResponse = deps.querier.query_wasm_smart(
        &msg.vp_contract,
        &VotingQueryMsg::TotalPowerAtHeight { height: None },
    )?;

    let hook_caller: Option<Addr>;
    match msg.hook_caller {
        Some(addr) => {
            hook_caller = Some(deps.api.addr_validate(&addr)?);
        }
        None => {
            hook_caller = None;
        }
    }
    let config = Config {
        vp_contract: deps.api.addr_validate(&msg.vp_contract)?,
        hook_caller,
        reward_token,
    };
    CONFIG.save(deps.storage, &config)?;

    if msg.reward_duration == 0 {
        return Err(ContractError::ZeroRewardDuration {});
    }

    let reward_config = RewardConfig {
        period_finish: 0,
        reward_rate: Uint128::zero(),
        reward_duration: msg.reward_duration,
    };
    REWARD_CONFIG.save(deps.storage, &reward_config)?;

    Ok(Response::new()
        .add_attribute("owner", msg.owner.unwrap_or_else(|| "None".to_string()))
        .add_attribute("vp_contract", config.vp_contract)
        .add_attribute(
            "reward_token",
            match config.reward_token {
                Denom::Native(denom) => denom,
                Cw20(addr) => addr.into_string(),
            },
        )
        .add_attribute("reward_rate", reward_config.reward_rate)
        .add_attribute("period_finish", reward_config.period_finish.to_string())
        .add_attribute("reward_duration", reward_config.reward_duration.to_string()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<Empty>, ContractError> {
    match msg {
        ExecuteMsg::StakeChangeHook(msg) => execute_stake_changed(deps, env, info, msg),
        ExecuteMsg::NftStakeChangeHook(msg) => execute_nft_stake_changed(deps, env, info, msg),
        ExecuteMsg::MemberChangedHook(msg) => execute_membership_changed(deps, env, info, msg),
        ExecuteMsg::Claim {} => execute_claim(deps, env, info),
        ExecuteMsg::Fund {} => execute_fund_native(deps, env, info),
        ExecuteMsg::Receive(msg) => execute_receive(deps, env, info, msg),
        ExecuteMsg::UpdateRewardDuration { new_duration } => {
            execute_update_reward_duration(deps, env, info, new_duration)
        }
        ExecuteMsg::UpdateOwnership(action) => execute_update_owner(deps, info, env, action),
    }
}

pub fn execute_receive(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    wrapper: Cw20ReceiveMsg,
) -> Result<Response<Empty>, ContractError> {
    let msg: ReceiveMsg = from_json(&wrapper.msg)?;
    let config = CONFIG.load(deps.storage)?;
    let sender = deps.api.addr_validate(&wrapper.sender)?;
    if config.reward_token != Denom::Cw20(info.sender) {
        return Err(InvalidCw20 {});
    };
    match msg {
        ReceiveMsg::Fund {} => execute_fund(deps, env, sender, wrapper.amount),
    }
}

pub fn execute_fund_native(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response<Empty>, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    match config.reward_token {
        Denom::Native(denom) => {
            let amount = cw_utils::must_pay(&info, &denom).map_err(|_| InvalidFunds {})?;
            execute_fund(deps, env, info.sender, amount)
        }
        Cw20(_) => Err(InvalidFunds {}),
    }
}

pub fn execute_fund(
    mut deps: DepsMut,
    env: Env,
    sender: Addr,
    amount: Uint128,
) -> Result<Response<Empty>, ContractError> {
    cw_ownable::assert_owner(deps.storage, &sender)?;

    update_rewards(&mut deps, &env, &sender)?;
    let reward_config = REWARD_CONFIG.load(deps.storage)?;
    if reward_config.period_finish > env.block.height {
        return Err(RewardPeriodNotFinished {});
    }
    let new_reward_config = RewardConfig {
        period_finish: env.block.height + reward_config.reward_duration,
        reward_rate: amount
            .checked_div(Uint128::from(reward_config.reward_duration))
            .map_err(StdError::divide_by_zero)?,
        // As we're not changing the value and changing the value
        // validates that the duration is non-zero we don't need to
        // check here.
        reward_duration: reward_config.reward_duration,
    };

    if new_reward_config.reward_rate == Uint128::zero() {
        return Err(ContractError::RewardRateLessThenOnePerBlock {});
    };

    REWARD_CONFIG.save(deps.storage, &new_reward_config)?;
    LAST_UPDATE_BLOCK.save(deps.storage, &env.block.height)?;

    Ok(Response::new()
        .add_attribute("action", "fund")
        .add_attribute("amount", amount)
        .add_attribute("new_reward_rate", new_reward_config.reward_rate.to_string()))
}

pub fn execute_stake_changed(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: StakeChangedHookMsg,
) -> Result<Response<Empty>, ContractError> {
    // Check that the sender is the vp_contract (or the hook_caller if configured).
    check_hook_caller(deps.as_ref(), info)?;

    match msg {
        StakeChangedHookMsg::Stake { addr, .. } => execute_stake(deps, env, addr),
        StakeChangedHookMsg::Unstake { addr, .. } => execute_unstake(deps, env, addr),
    }
}

pub fn execute_membership_changed(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: MemberChangedHookMsg,
) -> Result<Response<Empty>, ContractError> {
    // Check that the sender is the vp_contract (or the hook_caller if configured).
    check_hook_caller(deps.as_ref(), info)?;

    // Get the addresses of members whose voting power has changed.
    let _ = msg.diffs.iter().map(|member| -> Result<(), StdError> {
        let addr = deps.api.addr_validate(&member.key)?;
        update_rewards(&mut deps, &env, &addr)
    });

    Ok(Response::new().add_attribute("action", "membership_changed"))
}

pub fn execute_nft_stake_changed(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: NftStakeChangedHookMsg,
) -> Result<Response<Empty>, ContractError> {
    // Check that the sender is the vp_contract (or the hook_caller if configured).
    check_hook_caller(deps.as_ref(), info)?;

    match msg {
        NftStakeChangedHookMsg::Stake { addr, .. } => execute_stake(deps, env, addr),
        NftStakeChangedHookMsg::Unstake { addr, .. } => execute_unstake(deps, env, addr),
    }
}

// TODO better attributes?
pub fn execute_stake(
    mut deps: DepsMut,
    env: Env,
    addr: Addr,
) -> Result<Response<Empty>, ContractError> {
    update_rewards(&mut deps, &env, &addr)?;
    Ok(Response::new().add_attribute("action", "stake"))
}

// TODO better attributes?
pub fn execute_unstake(
    mut deps: DepsMut,
    env: Env,
    addr: Addr,
) -> Result<Response<Empty>, ContractError> {
    update_rewards(&mut deps, &env, &addr)?;
    Ok(Response::new().add_attribute("action", "unstake"))
}

pub fn execute_claim(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response<Empty>, ContractError> {
    update_rewards(&mut deps, &env, &info.sender)?;
    let rewards = PENDING_REWARDS
        .load(deps.storage, info.sender.clone())
        .map_err(|_| NoRewardsClaimable {})?;
    if rewards == Uint128::zero() {
        return Err(ContractError::NoRewardsClaimable {});
    }
    PENDING_REWARDS.save(deps.storage, info.sender.clone(), &Uint128::zero())?;
    let config = CONFIG.load(deps.storage)?;
    let transfer_msg = get_transfer_msg(info.sender, rewards, config.reward_token)?;
    Ok(Response::new()
        .add_message(transfer_msg)
        .add_attribute("action", "claim")
        .add_attribute("amount", rewards))
}

pub fn execute_update_owner(
    deps: DepsMut,
    info: MessageInfo,
    env: Env,
    action: cw_ownable::Action,
) -> Result<Response, ContractError> {
    let ownership = cw_ownable::update_ownership(deps, &env.block, &info.sender, action)?;
    Ok(Response::default().add_attributes(ownership.into_attributes()))
}

pub fn check_hook_caller(deps: Deps, info: MessageInfo) -> Result<(), ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // Only the voting power contract or the designated hook_caller contract (if configured)
    // can call this hook.
    match config.hook_caller {
        Some(hook_caller) => {
            if info.sender != hook_caller {
                return Err(ContractError::InvalidHookSender {});
            }
        }
        None => {
            if info.sender != config.vp_contract {
                return Err(ContractError::InvalidHookSender {});
            };
        }
    }
    Ok(())
}

pub fn get_transfer_msg(recipient: Addr, amount: Uint128, denom: Denom) -> StdResult<CosmosMsg> {
    match denom {
        Denom::Native(denom) => Ok(BankMsg::Send {
            to_address: recipient.into_string(),
            amount: vec![Coin { denom, amount }],
        }
        .into()),
        Denom::Cw20(addr) => {
            let cw20_msg = to_json_binary(&cw20::Cw20ExecuteMsg::Transfer {
                recipient: recipient.into_string(),
                amount,
            })?;
            Ok(WasmMsg::Execute {
                contract_addr: addr.into_string(),
                msg: cw20_msg,
                funds: vec![],
            }
            .into())
        }
    }
}

pub fn update_rewards(deps: &mut DepsMut, env: &Env, addr: &Addr) -> StdResult<()> {
    let config = CONFIG.load(deps.storage)?;
    let reward_per_token = get_reward_per_token(deps.as_ref(), env, &config.vp_contract)?;
    REWARD_PER_TOKEN.save(deps.storage, &reward_per_token)?;

    let earned_rewards = get_rewards_earned(
        deps.as_ref(),
        env,
        addr,
        reward_per_token,
        &config.vp_contract,
    )?;
    PENDING_REWARDS.update::<_, StdError>(deps.storage, addr.clone(), |r| {
        Ok(r.unwrap_or_default() + earned_rewards)
    })?;

    USER_REWARD_PER_TOKEN.save(deps.storage, addr.clone(), &reward_per_token)?;
    let last_time_reward_applicable = get_last_time_reward_applicable(deps.as_ref(), env)?;
    LAST_UPDATE_BLOCK.save(deps.storage, &last_time_reward_applicable)?;
    Ok(())
}

pub fn get_reward_per_token(deps: Deps, env: &Env, vp_contract: &Addr) -> StdResult<Uint256> {
    let reward_config = REWARD_CONFIG.load(deps.storage)?;
    let total_staked = get_total_voting_power(deps, vp_contract)?;
    let last_time_reward_applicable = get_last_time_reward_applicable(deps, env)?;
    let last_update_block = LAST_UPDATE_BLOCK.load(deps.storage).unwrap_or_default();
    let prev_reward_per_token = REWARD_PER_TOKEN.load(deps.storage).unwrap_or_default();
    let additional_reward_per_token = if total_staked == Uint128::zero() {
        Uint256::zero()
    } else {
        // It is impossible for this to overflow as total rewards can never exceed max value of
        // Uint128 as total tokens in existence cannot exceed Uint128
        let numerator = reward_config
            .reward_rate
            .full_mul(Uint128::from(
                last_time_reward_applicable - last_update_block,
            ))
            .checked_mul(scale_factor())?;
        let denominator = Uint256::from(total_staked);
        numerator.checked_div(denominator)?
    };

    Ok(prev_reward_per_token + additional_reward_per_token)
}

pub fn get_rewards_earned(
    deps: Deps,
    _env: &Env,
    addr: &Addr,
    reward_per_token: Uint256,
    vp_contract: &Addr,
) -> StdResult<Uint128> {
    let _config = CONFIG.load(deps.storage)?;
    let staked_balance = Uint256::from(get_voting_power(deps, vp_contract, addr)?);
    let user_reward_per_token = USER_REWARD_PER_TOKEN
        .load(deps.storage, addr.clone())
        .unwrap_or_default();
    let reward_factor = reward_per_token.checked_sub(user_reward_per_token)?;
    Ok(staked_balance
        .checked_mul(reward_factor)?
        .checked_div(scale_factor())?
        .try_into()?)
}

fn get_last_time_reward_applicable(deps: Deps, env: &Env) -> StdResult<u64> {
    let reward_config = REWARD_CONFIG.load(deps.storage)?;
    Ok(min(env.block.height, reward_config.period_finish))
}

fn get_total_voting_power(deps: Deps, contract_addr: &Addr) -> StdResult<Uint128> {
    let msg = VotingQueryMsg::TotalPowerAtHeight { height: None };
    let resp: TotalPowerAtHeightResponse = deps.querier.query_wasm_smart(contract_addr, &msg)?;
    Ok(resp.power)
}

fn get_voting_power(deps: Deps, contract_addr: &Addr, addr: &Addr) -> StdResult<Uint128> {
    let msg = VotingQueryMsg::VotingPowerAtHeight {
        address: addr.into(),
        height: None,
    };
    let resp: VotingPowerAtHeightResponse = deps.querier.query_wasm_smart(contract_addr, &msg)?;
    Ok(resp.power)
}

pub fn execute_update_reward_duration(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_duration: u64,
) -> Result<Response<Empty>, ContractError> {
    cw_ownable::assert_owner(deps.storage, &info.sender)?;

    let mut reward_config = REWARD_CONFIG.load(deps.storage)?;
    if reward_config.period_finish > env.block.height {
        return Err(ContractError::RewardPeriodNotFinished {});
    };

    if new_duration == 0 {
        return Err(ContractError::ZeroRewardDuration {});
    }

    let old_duration = reward_config.reward_duration;
    reward_config.reward_duration = new_duration;
    REWARD_CONFIG.save(deps.storage, &reward_config)?;

    Ok(Response::new()
        .add_attribute("action", "update_reward_duration")
        .add_attribute("new_duration", new_duration.to_string())
        .add_attribute("old_duration", old_duration.to_string()))
}

fn scale_factor() -> Uint256 {
    Uint256::from(10u8).pow(39)
}
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Info {} => Ok(to_json_binary(&query_info(deps, env)?)?),
        QueryMsg::GetPendingRewards { address } => {
            Ok(to_json_binary(&query_pending_rewards(deps, env, address)?)?)
        }
        QueryMsg::Ownership {} => to_json_binary(&cw_ownable::get_ownership(deps.storage)?),
    }
}

pub fn query_info(deps: Deps, _env: Env) -> StdResult<InfoResponse> {
    let config = CONFIG.load(deps.storage)?;
    let reward = REWARD_CONFIG.load(deps.storage)?;
    Ok(InfoResponse { config, reward })
}

pub fn query_pending_rewards(
    deps: Deps,
    env: Env,
    addr: String,
) -> StdResult<PendingRewardsResponse> {
    let addr = deps.api.addr_validate(&addr)?;
    let config = CONFIG.load(deps.storage)?;
    let reward_per_token = get_reward_per_token(deps, &env, &config.vp_contract)?;
    let earned_rewards =
        get_rewards_earned(deps, &env, &addr, reward_per_token, &config.vp_contract)?;

    let existing_rewards = PENDING_REWARDS
        .load(deps.storage, addr.clone())
        .unwrap_or_default();
    let pending_rewards = earned_rewards + existing_rewards;
    Ok(PendingRewardsResponse {
        address: addr.to_string(),
        pending_rewards,
        denom: config.reward_token,
        last_update_block: LAST_UPDATE_BLOCK.load(deps.storage).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use std::borrow::BorrowMut;

    use crate::ContractError;

    use cosmwasm_std::{coin, to_json_binary, Addr, Empty, Uint128};
    use cw20::{Cw20Coin, Cw20ExecuteMsg, Denom};
    use cw_ownable::{Action, Ownership, OwnershipError};
    use cw_utils::Duration;

    use cw_multi_test::{next_block, App, BankSudo, Contract, ContractWrapper, Executor, SudoMsg};

    use crate::msg::{ExecuteMsg, InfoResponse, PendingRewardsResponse, QueryMsg, ReceiveMsg};

    const OWNER: &str = "owner";
    const ADDR1: &str = "addr0001";
    const ADDR2: &str = "addr0002";
    const ADDR3: &str = "addr0003";

    pub fn contract_rewards() -> Box<dyn Contract<Empty>> {
        let contract = ContractWrapper::new(
            crate::contract::execute,
            crate::contract::instantiate,
            crate::contract::query,
        );
        Box::new(contract)
    }

    // TODO import these from dao-testing
    pub fn contract_staking() -> Box<dyn Contract<Empty>> {
        let contract = ContractWrapper::new(
            cw20_stake::contract::execute,
            cw20_stake::contract::instantiate,
            cw20_stake::contract::query,
        );
        Box::new(contract)
    }

    pub fn contract_cw20() -> Box<dyn Contract<Empty>> {
        let contract = ContractWrapper::new(
            cw20_base::contract::execute,
            cw20_base::contract::instantiate,
            cw20_base::contract::query,
        );
        Box::new(contract)
    }

    pub fn contract_voting_power() -> Box<dyn Contract<Empty>> {
        let contract = ContractWrapper::new(
            dao_voting_cw20_staked::contract::execute,
            dao_voting_cw20_staked::contract::instantiate,
            dao_voting_cw20_staked::contract::query,
        );
        Box::new(contract)
    }

    fn mock_app() -> App {
        App::default()
    }

    fn instantiate_cw20(app: &mut App, initial_balances: Vec<Cw20Coin>) -> Addr {
        let cw20_id = app.store_code(contract_cw20());
        let msg = cw20_base::msg::InstantiateMsg {
            name: String::from("Test"),
            symbol: String::from("TEST"),
            decimals: 6,
            initial_balances,
            mint: None,
            marketing: None,
        };

        app.instantiate_contract(cw20_id, Addr::unchecked(ADDR1), &msg, &[], "cw20", None)
            .unwrap()
    }

    fn instantiate_staking(
        app: &mut App,
        cw20: Addr,
        unstaking_duration: Option<Duration>,
    ) -> Addr {
        let staking_code_id = app.store_code(contract_staking());
        let msg = cw20_stake::msg::InstantiateMsg {
            owner: Some(OWNER.to_string()),
            token_address: cw20.to_string(),
            unstaking_duration,
        };
        app.instantiate_contract(
            staking_code_id,
            Addr::unchecked(ADDR1),
            &msg,
            &[],
            "staking",
            None,
        )
        .unwrap()
    }

    fn instantiate_vp_contract(app: &mut App, cw20: Addr, staking_contract: Addr) -> Addr {
        let vp_code_id = app.store_code(contract_voting_power());
        let msg = dao_voting_cw20_staked::msg::InstantiateMsg {
            token_info: dao_voting_cw20_staked::msg::TokenInfo::Existing {
                address: cw20.to_string(),
                staking_contract: dao_voting_cw20_staked::msg::StakingInfo::Existing {
                    staking_contract_address: staking_contract.to_string(),
                },
            },
            active_threshold: None,
        };
        app.instantiate_contract(vp_code_id, Addr::unchecked(ADDR1), &msg, &[], "vp", None)
            .unwrap()
    }

    fn stake_tokens<T: Into<String>>(
        app: &mut App,
        staking_addr: &Addr,
        cw20_addr: &Addr,
        sender: T,
        amount: u128,
    ) {
        let msg = cw20::Cw20ExecuteMsg::Send {
            contract: staking_addr.to_string(),
            amount: Uint128::new(amount),
            msg: to_json_binary(&cw20_stake::msg::ReceiveMsg::Stake {}).unwrap(),
        };
        app.execute_contract(Addr::unchecked(sender), cw20_addr.clone(), &msg, &[])
            .unwrap();
    }

    fn unstake_tokens(app: &mut App, staking_addr: &Addr, address: &str, amount: u128) {
        let msg = cw20_stake::msg::ExecuteMsg::Unstake {
            amount: Uint128::new(amount),
        };
        app.execute_contract(Addr::unchecked(address), staking_addr.clone(), &msg, &[])
            .unwrap();
    }

    fn setup(app: &mut App, initial_balances: Vec<Cw20Coin>) -> (Addr, Addr, Addr) {
        // Instantiate cw20 contract
        let cw20_addr = instantiate_cw20(app, initial_balances.clone());
        app.update_block(next_block);

        // Instantiate staking contract
        let staking_addr = instantiate_staking(app, cw20_addr.clone(), None);
        app.update_block(next_block);

        // Instantiate vp contract
        let vp_addr = instantiate_vp_contract(app, cw20_addr.clone(), staking_addr.clone());

        for coin in initial_balances {
            stake_tokens(
                app,
                &staking_addr,
                &cw20_addr,
                coin.address,
                coin.amount.u128(),
            );
        }
        (staking_addr, cw20_addr, vp_addr)
    }

    fn setup_reward_contract(
        app: &mut App,
        vp_contract: Addr,
        stake_contract: Addr,
        reward_token: Denom,
        owner: Addr,
    ) -> Addr {
        let reward_code_id = app.store_code(contract_rewards());
        let msg = crate::msg::InstantiateMsg {
            owner: Some(owner.clone().into_string()),
            vp_contract: vp_contract.clone().into_string(),
            hook_caller: Some(stake_contract.clone().into_string()),
            reward_token,
            reward_duration: 100000,
        };
        let reward_addr = app
            .instantiate_contract(reward_code_id, owner, &msg, &[], "reward", None)
            .unwrap();
        let msg = cw20_stake::msg::ExecuteMsg::AddHook {
            addr: reward_addr.to_string(),
        };
        let _result = app
            .execute_contract(Addr::unchecked(OWNER), stake_contract, &msg, &[])
            .unwrap();
        reward_addr
    }

    fn get_balance_cw20<T: Into<String>, U: Into<String>>(
        app: &App,
        contract_addr: T,
        address: U,
    ) -> Uint128 {
        let msg = cw20::Cw20QueryMsg::Balance {
            address: address.into(),
        };
        let result: cw20::BalanceResponse =
            app.wrap().query_wasm_smart(contract_addr, &msg).unwrap();
        result.balance
    }

    fn get_balance_native<T: Into<String>, U: Into<String>>(
        app: &App,
        address: T,
        denom: U,
    ) -> Uint128 {
        app.wrap().query_balance(address, denom).unwrap().amount
    }

    fn get_ownership<T: Into<String>>(app: &App, address: T) -> Ownership<Addr> {
        app.wrap()
            .query_wasm_smart(address, &QueryMsg::Ownership {})
            .unwrap()
    }

    fn assert_pending_rewards(app: &mut App, reward_addr: &Addr, address: &str, expected: u128) {
        let res: PendingRewardsResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(
                reward_addr,
                &QueryMsg::GetPendingRewards {
                    address: address.to_string(),
                },
            )
            .unwrap();
        assert_eq!(res.pending_rewards, Uint128::new(expected));
    }

    fn claim_rewards(app: &mut App, reward_addr: Addr, address: &str) {
        let msg = ExecuteMsg::Claim {};
        app.borrow_mut()
            .execute_contract(Addr::unchecked(address), reward_addr, &msg, &[])
            .unwrap();
    }

    fn fund_rewards_cw20(
        app: &mut App,
        admin: &Addr,
        reward_token: Addr,
        reward_addr: &Addr,
        amount: u128,
    ) {
        let fund_sub_msg = to_json_binary(&ReceiveMsg::Fund {}).unwrap();
        let fund_msg = Cw20ExecuteMsg::Send {
            contract: reward_addr.clone().into_string(),
            amount: Uint128::new(amount),
            msg: fund_sub_msg,
        };
        let _res = app
            .borrow_mut()
            .execute_contract(admin.clone(), reward_token, &fund_msg, &[])
            .unwrap();
    }

    #[test]
    fn test_zero_rewards_duration() {
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let denom = "utest".to_string();
        let (staking_addr, _, vp_addr) = setup(&mut app, vec![]);
        let reward_funding = vec![coin(100000000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding,
            }
        }))
        .unwrap();

        let reward_token = Denom::Native(denom);
        let owner = admin;
        let reward_code_id = app.store_code(contract_rewards());
        let msg = crate::msg::InstantiateMsg {
            owner: Some(owner.clone().into_string()),
            vp_contract: vp_addr.to_string(),
            hook_caller: Some(staking_addr.to_string()),
            reward_token,
            reward_duration: 0,
        };
        let err: ContractError = app
            .instantiate_contract(reward_code_id, owner, &msg, &[], "reward", None)
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::ZeroRewardDuration {})
    }

    #[test]
    fn test_native_rewards() {
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        let (staking_addr, cw20_addr, vp_addr) = setup(&mut app, initial_balances);
        let reward_funding = vec![coin(100000000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr.clone(),
            staking_addr.clone(),
            Denom::Native(denom.clone()),
            admin.clone(),
        );

        app.borrow_mut().update_block(|b| b.height = 1000);

        let fund_msg = ExecuteMsg::Fund {};

        let _res = app
            .borrow_mut()
            .execute_contract(
                admin.clone(),
                reward_addr.clone(),
                &fund_msg,
                &reward_funding,
            )
            .unwrap();

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(1000));
        assert_eq!(res.reward.period_finish, 101000);
        assert_eq!(res.reward.reward_duration, 100000);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 250);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 250);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 1000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 500);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 1500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 750);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 750);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 2000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 1000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 1000);

        assert_eq!(get_balance_native(&app, ADDR1, &denom), Uint128::zero());
        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        assert_eq!(get_balance_native(&app, ADDR1, &denom), Uint128::new(2000));
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 0);

        app.borrow_mut().update_block(|b| b.height += 10);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 5000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 3500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 3500);

        unstake_tokens(&mut app, &staking_addr, ADDR2, 50);
        unstake_tokens(&mut app, &staking_addr, ADDR3, 50);

        app.borrow_mut().update_block(|b| b.height += 10);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 15000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 3500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 3500);

        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        assert_eq!(get_balance_native(&app, ADDR1, &denom), Uint128::new(17000));

        claim_rewards(&mut app, reward_addr.clone(), ADDR2);
        assert_eq!(get_balance_native(&app, ADDR2, &denom), Uint128::new(3500));

        stake_tokens(&mut app, &staking_addr, &cw20_addr, ADDR2, 50);
        stake_tokens(&mut app, &staking_addr, &cw20_addr, ADDR3, 50);

        app.borrow_mut().update_block(|b| b.height += 10);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 5000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 2500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 6000);

        // Current height is 1034. ADDR1 is receiving 500 tokens/block
        // and ADDR2 / ADDR3 are receiving 250.
        //
        // At height 101000 99966 additional blocks have passed. So we
        // expect:
        //
        // ADDR1: 5000 + 99966 * 500 = 49,998,000
        // ADDR2: 2500 + 99966 * 250 = 24,994,000
        // ADDR3: 6000 + 99966 * 250 = 24,997,500
        app.borrow_mut().update_block(|b| b.height = 101000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 49988000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 24994000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 24997500);

        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        claim_rewards(&mut app, reward_addr.clone(), ADDR2);
        assert_eq!(
            get_balance_native(&app, ADDR1, &denom),
            Uint128::new(50005000)
        );
        assert_eq!(
            get_balance_native(&app, ADDR2, &denom),
            Uint128::new(24997500)
        );
        assert_eq!(get_balance_native(&app, ADDR3, &denom), Uint128::new(0));
        assert_eq!(
            get_balance_native(&app, &reward_addr, &denom),
            Uint128::new(24997500)
        );

        app.borrow_mut().update_block(|b| b.height = 200000);
        let fund_msg = ExecuteMsg::Fund {};

        // Add more rewards
        let reward_funding = vec![coin(200000000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();

        let _res = app
            .borrow_mut()
            .execute_contract(
                admin.clone(),
                reward_addr.clone(),
                &fund_msg,
                &reward_funding,
            )
            .unwrap();

        app.borrow_mut().update_block(|b| b.height = 300000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 100000000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 50000000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 74997500);

        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        claim_rewards(&mut app, reward_addr.clone(), ADDR2);
        assert_eq!(
            get_balance_native(&app, ADDR1, &denom),
            Uint128::new(150005000)
        );
        assert_eq!(
            get_balance_native(&app, ADDR2, &denom),
            Uint128::new(74997500)
        );
        assert_eq!(get_balance_native(&app, ADDR3, &denom), Uint128::zero());
        assert_eq!(
            get_balance_native(&app, &reward_addr, &denom),
            Uint128::new(74997500)
        );

        // Add more rewards
        let reward_funding = vec![coin(200000000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();

        let _res = app
            .borrow_mut()
            .execute_contract(admin, reward_addr.clone(), &fund_msg, &reward_funding)
            .unwrap();

        app.borrow_mut().update_block(|b| b.height = 400000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 100000000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 50000000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 124997500);

        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        claim_rewards(&mut app, reward_addr.clone(), ADDR2);
        claim_rewards(&mut app, reward_addr.clone(), ADDR3);
        assert_eq!(
            get_balance_native(&app, ADDR1, &denom),
            Uint128::new(250005000)
        );
        assert_eq!(
            get_balance_native(&app, ADDR2, &denom),
            Uint128::new(124997500)
        );
        assert_eq!(
            get_balance_native(&app, ADDR3, &denom),
            Uint128::new(124997500)
        );
        assert_eq!(
            get_balance_native(&app, &reward_addr, &denom),
            Uint128::zero()
        );

        app.borrow_mut().update_block(|b| b.height = 500000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 0);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 0);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 0);

        app.borrow_mut().update_block(|b| b.height = 1000000);
        unstake_tokens(&mut app, &staking_addr, ADDR3, 1);
        stake_tokens(&mut app, &staking_addr, &cw20_addr, ADDR3, 1);
    }

    #[test]
    fn test_cw20_rewards() {
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        let (staking_addr, cw20_addr, vp_addr) = setup(&mut app, initial_balances);
        let reward_token = instantiate_cw20(
            &mut app,
            vec![Cw20Coin {
                address: OWNER.to_string(),
                amount: Uint128::new(500000000),
            }],
        );
        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr.clone(),
            staking_addr.clone(),
            Denom::Cw20(reward_token.clone()),
            admin.clone(),
        );

        app.borrow_mut().update_block(|b| b.height = 1000);

        fund_rewards_cw20(
            &mut app,
            &admin,
            reward_token.clone(),
            &reward_addr,
            100000000,
        );

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(1000));
        assert_eq!(res.reward.period_finish, 101000);
        assert_eq!(res.reward.reward_duration, 100000);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 250);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 250);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 1000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 500);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 1500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 750);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 750);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 2000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 1000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 1000);

        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR1),
            Uint128::zero()
        );
        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR1),
            Uint128::new(2000)
        );
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 0);

        app.borrow_mut().update_block(|b| b.height += 10);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 5000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 3500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 3500);

        unstake_tokens(&mut app, &staking_addr, ADDR2, 50);
        unstake_tokens(&mut app, &staking_addr, ADDR3, 50);

        app.borrow_mut().update_block(|b| b.height += 10);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 15000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 3500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 3500);

        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR1),
            Uint128::new(17000)
        );

        claim_rewards(&mut app, reward_addr.clone(), ADDR2);
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR2),
            Uint128::new(3500)
        );

        stake_tokens(&mut app, &staking_addr, &cw20_addr, ADDR2, 50);
        stake_tokens(&mut app, &staking_addr, &cw20_addr, ADDR3, 50);

        app.borrow_mut().update_block(|b| b.height += 10);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 5000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 2500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 6000);

        app.borrow_mut().update_block(|b| b.height = 101000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 49988000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 24994000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 24997500);

        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        claim_rewards(&mut app, reward_addr.clone(), ADDR2);
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR1),
            Uint128::new(50005000)
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR2),
            Uint128::new(24997500)
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR3),
            Uint128::new(0)
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, &reward_addr),
            Uint128::new(24997500)
        );

        app.borrow_mut().update_block(|b| b.height = 200000);

        let reward_funding = vec![coin(200000000, denom)];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding,
            }
        }))
        .unwrap();

        fund_rewards_cw20(
            &mut app,
            &admin,
            reward_token.clone(),
            &reward_addr,
            200000000,
        );

        app.borrow_mut().update_block(|b| b.height = 300000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 100000000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 50000000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 74997500);

        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        claim_rewards(&mut app, reward_addr.clone(), ADDR2);
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR1),
            Uint128::new(150005000)
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR2),
            Uint128::new(74997500)
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR3),
            Uint128::zero()
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, &reward_addr),
            Uint128::new(74997500)
        );

        // Add more rewards
        fund_rewards_cw20(
            &mut app,
            &admin,
            reward_token.clone(),
            &reward_addr,
            200000000,
        );

        app.borrow_mut().update_block(|b| b.height = 400000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 100000000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 50000000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 124997500);

        claim_rewards(&mut app, reward_addr.clone(), ADDR1);
        claim_rewards(&mut app, reward_addr.clone(), ADDR2);
        claim_rewards(&mut app, reward_addr.clone(), ADDR3);
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR1),
            Uint128::new(250005000)
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR2),
            Uint128::new(124997500)
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, ADDR3),
            Uint128::new(124997500)
        );
        assert_eq!(
            get_balance_cw20(&app, &reward_token, &reward_addr),
            Uint128::zero()
        );

        app.borrow_mut().update_block(|b| b.height = 500000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 0);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 0);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 0);

        app.borrow_mut().update_block(|b| b.height = 1000000);
        unstake_tokens(&mut app, &staking_addr, ADDR3, 1);
        stake_tokens(&mut app, &staking_addr, &cw20_addr, ADDR3, 1);
    }

    #[test]
    fn update_rewards() {
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        let (staking_addr, _cw20_addr, vp_addr) = setup(&mut app, initial_balances);
        let reward_funding = vec![coin(200000000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        // Add funding to Addr1 to make sure it can't update staking contract
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: ADDR1.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr,
            staking_addr.clone(),
            Denom::Native(denom.clone()),
            admin.clone(),
        );

        app.borrow_mut().update_block(|b| b.height = 1000);

        let fund_msg = ExecuteMsg::Fund {};

        // None admin cannot update rewards
        let err: ContractError = app
            .borrow_mut()
            .execute_contract(
                Addr::unchecked(ADDR1),
                reward_addr.clone(),
                &fund_msg,
                &reward_funding,
            )
            .unwrap_err()
            .downcast()
            .unwrap();

        assert_eq!(err, ContractError::Ownable(OwnershipError::NotOwner));

        let _res = app
            .borrow_mut()
            .execute_contract(
                admin.clone(),
                reward_addr.clone(),
                &fund_msg,
                &reward_funding,
            )
            .unwrap();

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(2000));
        assert_eq!(res.reward.period_finish, 101000);
        assert_eq!(res.reward.reward_duration, 100000);

        // Create new period after old period
        app.borrow_mut().update_block(|b| b.height = 101000);

        let reward_funding = vec![coin(100000000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        let _res = app
            .borrow_mut()
            .execute_contract(
                admin.clone(),
                reward_addr.clone(),
                &fund_msg,
                &reward_funding,
            )
            .unwrap();

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(1000));
        assert_eq!(res.reward.period_finish, 201000);
        assert_eq!(res.reward.reward_duration, 100000);

        // Add funds in middle of period returns an error
        app.borrow_mut().update_block(|b| b.height = 151000);

        let reward_funding = vec![coin(200000000, denom)];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        let err = app
            .borrow_mut()
            .execute_contract(admin, reward_addr.clone(), &fund_msg, &reward_funding)
            .unwrap_err();
        assert_eq!(
            ContractError::RewardPeriodNotFinished {},
            err.downcast().unwrap()
        );

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(1000));
        assert_eq!(res.reward.period_finish, 201000);
        assert_eq!(res.reward.reward_duration, 100000);
    }

    #[test]
    fn update_reward_duration() {
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        let (staking_addr, _cw20_addr, vp_addr) = setup(&mut app, initial_balances);

        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr,
            staking_addr.clone(),
            Denom::Native(denom.clone()),
            admin.clone(),
        );

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(0));
        assert_eq!(res.reward.period_finish, 0);
        assert_eq!(res.reward.reward_duration, 100000);

        // Zero rewards durations are not allowed.
        let msg = ExecuteMsg::UpdateRewardDuration { new_duration: 0 };
        let err: ContractError = app
            .borrow_mut()
            .execute_contract(admin.clone(), reward_addr.clone(), &msg, &[])
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::ZeroRewardDuration {});

        let msg = ExecuteMsg::UpdateRewardDuration { new_duration: 10 };
        let _resp = app
            .borrow_mut()
            .execute_contract(admin.clone(), reward_addr.clone(), &msg, &[])
            .unwrap();

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(0));
        assert_eq!(res.reward.period_finish, 0);
        assert_eq!(res.reward.reward_duration, 10);

        // Non-admin cannot update rewards
        let msg = ExecuteMsg::UpdateRewardDuration { new_duration: 100 };
        let err: ContractError = app
            .borrow_mut()
            .execute_contract(Addr::unchecked("non-admin"), reward_addr.clone(), &msg, &[])
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::Ownable(OwnershipError::NotOwner));

        let reward_funding = vec![coin(1000, denom)];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        // Add funding to Addr1 to make sure it can't update staking contract
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: ADDR1.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();

        app.borrow_mut().update_block(|b| b.height = 1000);

        let fund_msg = ExecuteMsg::Fund {};

        let _res = app
            .borrow_mut()
            .execute_contract(
                admin.clone(),
                reward_addr.clone(),
                &fund_msg,
                &reward_funding,
            )
            .unwrap();

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(100));
        assert_eq!(res.reward.period_finish, 1010);
        assert_eq!(res.reward.reward_duration, 10);

        // Cannot update reward period before it finishes
        let msg = ExecuteMsg::UpdateRewardDuration { new_duration: 10 };
        let err: ContractError = app
            .borrow_mut()
            .execute_contract(admin.clone(), reward_addr.clone(), &msg, &[])
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::RewardPeriodNotFinished {});

        // Update reward period once rewards are finished
        app.borrow_mut().update_block(|b| b.height = 1010);

        let msg = ExecuteMsg::UpdateRewardDuration { new_duration: 100 };
        let _resp = app
            .borrow_mut()
            .execute_contract(admin, reward_addr.clone(), &msg, &[])
            .unwrap();

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(100));
        assert_eq!(res.reward.period_finish, 1010);
        assert_eq!(res.reward.reward_duration, 100);
    }

    #[test]
    fn test_update_owner() {
        let mut app = mock_app();
        let addr_owner = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        let (staking_addr, _cw20_addr, vp_addr) = setup(&mut app, initial_balances);

        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr,
            staking_addr.clone(),
            Denom::Native(denom),
            addr_owner.clone(),
        );

        let owner = get_ownership(&app, &reward_addr).owner;
        assert_eq!(owner, Some(addr_owner.clone()));

        // random addr cannot update owner
        let msg = ExecuteMsg::UpdateOwnership(Action::TransferOwnership {
            new_owner: ADDR1.to_string(),
            expiry: None,
        });
        let err: ContractError = app
            .borrow_mut()
            .execute_contract(Addr::unchecked(ADDR1), reward_addr.clone(), &msg, &[])
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::Ownable(OwnershipError::NotOwner));

        // owner nominates a new onwer.
        app.borrow_mut()
            .execute_contract(addr_owner.clone(), reward_addr.clone(), &msg, &[])
            .unwrap();

        let ownership = get_ownership(&app, &reward_addr);
        assert_eq!(
            ownership,
            Ownership::<Addr> {
                owner: Some(addr_owner),
                pending_owner: Some(Addr::unchecked(ADDR1)),
                pending_expiry: None,
            }
        );

        // new owner accepts the nomination.
        app.execute_contract(
            Addr::unchecked(ADDR1),
            reward_addr.clone(),
            &ExecuteMsg::UpdateOwnership(Action::AcceptOwnership),
            &[],
        )
        .unwrap();

        let ownership = get_ownership(&app, &reward_addr);
        assert_eq!(
            ownership,
            Ownership::<Addr> {
                owner: Some(Addr::unchecked(ADDR1)),
                pending_owner: None,
                pending_expiry: None,
            }
        );

        // new owner renounces ownership.
        app.execute_contract(
            Addr::unchecked(ADDR1),
            reward_addr.clone(),
            &ExecuteMsg::UpdateOwnership(Action::RenounceOwnership),
            &[],
        )
        .unwrap();

        let ownership = get_ownership(&app, &reward_addr);
        assert_eq!(
            ownership,
            Ownership::<Addr> {
                owner: None,
                pending_owner: None,
                pending_expiry: None,
            }
        );
    }

    #[test]
    fn test_cannot_fund_with_wrong_coin_native() {
        let mut app = mock_app();
        let owner = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        let (staking_addr, _cw20_addr, vp_addr) = setup(&mut app, initial_balances);

        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr,
            staking_addr.clone(),
            Denom::Native(denom.clone()),
            owner.clone(),
        );

        app.borrow_mut().update_block(|b| b.height = 1000);

        // No funding
        let fund_msg = ExecuteMsg::Fund {};

        let err: ContractError = app
            .borrow_mut()
            .execute_contract(owner.clone(), reward_addr.clone(), &fund_msg, &[])
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::InvalidFunds {});

        // Invalid funding
        let invalid_funding = vec![coin(100, "invalid")];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: owner.to_string(),
                amount: invalid_funding.clone(),
            }
        }))
        .unwrap();

        let fund_msg = ExecuteMsg::Fund {};

        let err: ContractError = app
            .borrow_mut()
            .execute_contract(
                owner.clone(),
                reward_addr.clone(),
                &fund_msg,
                &invalid_funding,
            )
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::InvalidFunds {});

        // Extra funding
        let extra_funding = vec![coin(100, denom), coin(100, "extra")];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: owner.to_string(),
                amount: extra_funding.clone(),
            }
        }))
        .unwrap();

        let fund_msg = ExecuteMsg::Fund {};

        let err: ContractError = app
            .borrow_mut()
            .execute_contract(
                owner.clone(),
                reward_addr.clone(),
                &fund_msg,
                &extra_funding,
            )
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::InvalidFunds {});

        // Cw20 funding fails
        let cw20_token = instantiate_cw20(
            &mut app,
            vec![Cw20Coin {
                address: OWNER.to_string(),
                amount: Uint128::new(500000000),
            }],
        );
        let fund_sub_msg = to_json_binary(&ReceiveMsg::Fund {}).unwrap();
        let fund_msg = Cw20ExecuteMsg::Send {
            contract: reward_addr.into_string(),
            amount: Uint128::new(100),
            msg: fund_sub_msg,
        };
        let err: ContractError = app
            .borrow_mut()
            .execute_contract(owner, cw20_token, &fund_msg, &[])
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::InvalidCw20 {});
    }

    #[test]
    fn test_cannot_fund_with_wrong_coin_cw20() {
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let _denom = "utest".to_string();
        let (staking_addr, _cw20_addr, vp_addr) = setup(&mut app, initial_balances);
        let reward_token = instantiate_cw20(
            &mut app,
            vec![Cw20Coin {
                address: OWNER.to_string(),
                amount: Uint128::new(500000000),
            }],
        );
        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr,
            staking_addr.clone(),
            Denom::Cw20(Addr::unchecked("dummy_cw20")),
            admin.clone(),
        );

        app.borrow_mut().update_block(|b| b.height = 1000);

        // Test with invalid token
        let fund_sub_msg = to_json_binary(&ReceiveMsg::Fund {}).unwrap();
        let fund_msg = Cw20ExecuteMsg::Send {
            contract: reward_addr.clone().into_string(),
            amount: Uint128::new(100),
            msg: fund_sub_msg,
        };
        let err: ContractError = app
            .borrow_mut()
            .execute_contract(admin.clone(), reward_token, &fund_msg, &[])
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::InvalidCw20 {});

        // Test does not work when funded with native
        let invalid_funding = vec![coin(100, "invalid")];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: invalid_funding.clone(),
            }
        }))
        .unwrap();

        let fund_msg = ExecuteMsg::Fund {};

        let err: ContractError = app
            .borrow_mut()
            .execute_contract(admin, reward_addr, &fund_msg, &invalid_funding)
            .unwrap_err()
            .downcast()
            .unwrap();
        assert_eq!(err, ContractError::InvalidFunds {})
    }

    #[test]
    fn test_rewards_with_zero_staked() {
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        // Instantiate cw20 contract
        let cw20_addr = instantiate_cw20(&mut app, initial_balances.clone());
        app.update_block(next_block);
        // Instantiate staking contract
        let staking_addr = instantiate_staking(&mut app, cw20_addr.clone(), None);
        app.update_block(next_block);
        // Instantiate vote power contract
        let vp_addr = instantiate_vp_contract(&mut app, cw20_addr.clone(), staking_addr.clone());

        let reward_funding = vec![coin(100000000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr.clone(),
            staking_addr.clone(),
            Denom::Native(denom),
            admin.clone(),
        );

        app.borrow_mut().update_block(|b| b.height = 1000);

        let fund_msg = ExecuteMsg::Fund {};

        let _res = app
            .borrow_mut()
            .execute_contract(admin, reward_addr.clone(), &fund_msg, &reward_funding)
            .unwrap();

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(1000));
        assert_eq!(res.reward.period_finish, 101000);
        assert_eq!(res.reward.reward_duration, 100000);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 0);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 0);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 0);

        for coin in initial_balances {
            stake_tokens(
                &mut app,
                &staking_addr,
                &cw20_addr,
                coin.address,
                coin.amount.u128(),
            );
        }

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 250);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 250);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 1000);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 500);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 500);
    }

    #[test]
    fn test_small_rewards() {
        // This test was added due to a bug in the contract not properly paying out small reward
        // amounts due to floor division
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        let (staking_addr, _, vp_addr) = setup(&mut app, initial_balances);
        let reward_funding = vec![coin(1000000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr,
            staking_addr.clone(),
            Denom::Native(denom),
            admin.clone(),
        );

        app.borrow_mut().update_block(|b| b.height = 1000);

        let fund_msg = ExecuteMsg::Fund {};

        let _res = app
            .borrow_mut()
            .execute_contract(admin, reward_addr.clone(), &fund_msg, &reward_funding)
            .unwrap();

        let res: InfoResponse = app
            .borrow_mut()
            .wrap()
            .query_wasm_smart(&reward_addr, &QueryMsg::Info {})
            .unwrap();

        assert_eq!(res.reward.reward_rate, Uint128::new(10));
        assert_eq!(res.reward.period_finish, 101000);
        assert_eq!(res.reward.reward_duration, 100000);

        app.borrow_mut().update_block(next_block);
        assert_pending_rewards(&mut app, &reward_addr, ADDR1, 5);
        assert_pending_rewards(&mut app, &reward_addr, ADDR2, 2);
        assert_pending_rewards(&mut app, &reward_addr, ADDR3, 2);
    }

    #[test]
    fn test_zero_reward_rate_failed() {
        // This test is due to a bug when funder provides rewards config that results in less then 1
        // reward per block which rounds down to zer0
        let mut app = mock_app();
        let admin = Addr::unchecked(OWNER);
        app.borrow_mut().update_block(|b| b.height = 0);
        let initial_balances = vec![
            Cw20Coin {
                address: ADDR1.to_string(),
                amount: Uint128::new(100),
            },
            Cw20Coin {
                address: ADDR2.to_string(),
                amount: Uint128::new(50),
            },
            Cw20Coin {
                address: ADDR3.to_string(),
                amount: Uint128::new(50),
            },
        ];
        let denom = "utest".to_string();
        let (staking_addr, _, vp_addr) = setup(&mut app, initial_balances);
        let reward_funding = vec![coin(10000, denom.clone())];
        app.sudo(SudoMsg::Bank({
            BankSudo::Mint {
                to_address: admin.to_string(),
                amount: reward_funding.clone(),
            }
        }))
        .unwrap();
        let reward_addr = setup_reward_contract(
            &mut app,
            vp_addr,
            staking_addr.clone(),
            Denom::Native(denom),
            admin.clone(),
        );

        app.borrow_mut().update_block(|b| b.height = 1000);

        let fund_msg = ExecuteMsg::Fund {};

        let _res = app
            .borrow_mut()
            .execute_contract(admin, reward_addr, &fund_msg, &reward_funding)
            .unwrap_err();
    }
}