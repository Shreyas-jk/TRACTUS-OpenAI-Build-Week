use chaos_core::contract::Contract;
use chaos_core::history::History;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldDecision {
    ApproveOnce,
    Reject,
}

#[derive(Clone, Debug)]
pub struct PendingReport {
    pub session_key: SessionKey,
    pub commands: Vec<Vec<String>>,
}

/// A loop-history namespace. A Codex session may legitimately move to a new
/// approved contract, so failures under an old contract must not contribute to
/// a loop verdict under the newly selected one.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionKey {
    pub agent_session: String,
    pub contract_id: Option<String>,
}

impl SessionKey {
    pub fn legacy(agent_session: impl Into<String>) -> Self {
        Self {
            agent_session: agent_session.into(),
            contract_id: None,
        }
    }

    pub fn named(agent_session: impl Into<String>, contract_id: impl Into<String>) -> Self {
        Self {
            agent_session: agent_session.into(),
            contract_id: Some(contract_id.into()),
        }
    }
}

#[derive(Default)]
pub struct DaemonState {
    /// Compatibility fallback for legacy clients that do not send a contract
    /// id (the existing control plane uses this path).
    pub active_contract: Option<Arc<Contract>>,
    /// Explicitly named documents installed by `tractus codex`.
    pub contracts: HashMap<String, Arc<Contract>>,
    /// Once a session presents a named contract, later legacy-looking calls
    /// from that same session stay bound to it rather than falling back.
    pub session_contracts: HashMap<String, String>,
    pub histories: HashMap<SessionKey, History>,
    pub pending_holds: HashMap<String, oneshot::Sender<HoldDecision>>,
    pub pending_reports: HashMap<String, PendingReport>,
}

impl DaemonState {
    /// Resolves the contract for one proposal without ever falling back from an
    /// explicitly requested but unknown id. That makes a missing launcher
    /// registration a hold, never an accidental allow under another contract.
    pub fn resolve_contract(
        &mut self,
        agent_session: &str,
        requested_contract_id: Option<&str>,
    ) -> Option<(Arc<Contract>, SessionKey)> {
        if let Some(contract_id) = requested_contract_id {
            let contract = self.contracts.get(contract_id)?.clone();
            self.session_contracts
                .insert(agent_session.to_owned(), contract_id.to_owned());
            return Some((
                contract,
                SessionKey::named(agent_session, contract_id.to_owned()),
            ));
        }

        if let Some(contract_id) = self.session_contracts.get(agent_session).cloned() {
            let contract = self.contracts.get(&contract_id)?.clone();
            return Some((contract, SessionKey::named(agent_session, contract_id)));
        }

        self.active_contract
            .clone()
            .map(|contract| (contract, SessionKey::legacy(agent_session)))
    }
}

pub type SharedState = Arc<Mutex<DaemonState>>;

pub fn shared_state() -> SharedState {
    Arc::new(Mutex::new(DaemonState::default()))
}
