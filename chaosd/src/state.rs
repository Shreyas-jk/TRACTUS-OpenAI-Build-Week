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
    pub agent_session: String,
    pub commands: Vec<Vec<String>>,
}

#[derive(Default)]
pub struct DaemonState {
    pub active_contract: Option<Arc<Contract>>,
    pub histories: HashMap<String, History>,
    pub pending_holds: HashMap<String, oneshot::Sender<HoldDecision>>,
    pub pending_reports: HashMap<String, PendingReport>,
}

pub type SharedState = Arc<Mutex<DaemonState>>;

pub fn shared_state() -> SharedState {
    Arc::new(Mutex::new(DaemonState::default()))
}
