use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{CosmosMsg, Empty};
use dao_pre_propose_base::msg::{
    ExecuteMsg as ExecuteBase, InstantiateMsg as InstantiateBase, MigrateMsg as MigrateBase,
    QueryMsg as QueryBase,
};
use dao_voting::{proposal::SingleChoiceProposeMsg as ProposeMsg, voting::SingleChoiceAutoVote};

#[cw_serde]
pub enum ApproverProposeMessage {
    Propose {
        title: String,
        description: String,
        approval_id: u64,
    },
}

#[cw_serde]
pub enum ProposeMessage {
    Propose {
        title: String,
        description: String,
        msgs: Vec<CosmosMsg<Empty>>,
        vote: Option<SingleChoiceAutoVote>,
    },
}

#[cw_serde]
pub struct InstantiateExt {
    pub approver: String,
}

#[cw_serde]
pub enum ExecuteExt {
    /// Approve a proposal, only callable by approver
    Approve { id: u64 },
    /// Reject a proposal, only callable by approver
    Reject { id: u64 },
    /// Updates the approver, can only be called the current approver
    UpdateApprover { address: String },
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryExt {
    /// List the approver address
    #[returns(cosmwasm_std::Addr)]
    Approver {},
    /// Return whether or not the proposal is pending
    #[returns(bool)]
    IsPending { id: u64 },
    /// A proposal, pending or completed.
    #[returns(crate::state::Proposal)]
    Proposal { id: u64 },
    /// A pending proposal
    #[returns(crate::state::Proposal)]
    PendingProposal { id: u64 },
    /// List of proposals awaiting approval
    #[returns(Vec<crate::state::Proposal>)]
    PendingProposals {
        start_after: Option<u64>,
        limit: Option<u32>,
    },
    #[returns(Vec<crate::state::Proposal>)]
    ReversePendingProposals {
        start_before: Option<u64>,
        limit: Option<u32>,
    },
    /// A completed proposal
    #[returns(crate::state::Proposal)]
    CompletedProposal { id: u64 },
    /// List of completed proposals
    #[returns(Vec<crate::state::Proposal>)]
    CompletedProposals {
        start_after: Option<u64>,
        limit: Option<u32>,
    },
    #[returns(Vec<crate::state::Proposal>)]
    ReverseCompletedProposals {
        start_before: Option<u64>,
        limit: Option<u32>,
    },
    /// The completed approval ID for a created proposal ID.
    #[returns(::std::option::Option<u64>)]
    CompletedProposalIdForCreatedProposalId { id: u64 },
}

pub type InstantiateMsg = InstantiateBase<InstantiateExt>;
pub type ExecuteMsg = ExecuteBase<ProposeMessage, ExecuteExt>;
pub type QueryMsg = QueryBase<QueryExt>;
pub type MigrateMsg = MigrateBase<Empty>;

/// Internal version of the propose message that includes the
/// `proposer` field. The module will fill this in based on the sender
/// of the external message.
#[cw_serde]
pub(crate) enum ProposeMessageInternal {
    Propose(ProposeMsg),
}
