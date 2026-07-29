//! The axum HTTP/WebSocket surface: dashboard, intent extraction, contract
//! confirmation, hold resolution, the live event bridge, and the demo terminal.

use crate::daemon::DaemonClient;
use crate::intent::{extract_intent, toggle_card, ContractSpec};
use crate::terminal::bridge_terminal;
use crate::{explain, ConsoleError};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Json, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const XTERM_JS: &str = include_str!("../assets/vendor/xterm.js");
const XTERM_FIT_JS: &str = include_str!("../assets/vendor/xterm-addon-fit.js");
const XTERM_CSS: &str = include_str!("../assets/vendor/xterm.css");

/// Shared, cheaply-cloneable handler state.
#[derive(Clone)]
pub struct AppState {
    http: reqwest::Client,
    daemon: DaemonClient,
    active_task: Arc<Mutex<String>>,
}

impl AppState {
    pub fn new(daemon: DaemonClient) -> Self {
        Self {
            http: reqwest::Client::new(),
            daemon,
            active_task: Arc::new(Mutex::new("the approved task".to_owned())),
        }
    }

    fn active_task(&self) -> String {
        self.active_task
            .lock()
            .map(|task| task.clone())
            .unwrap_or_else(|_| "the approved task".to_owned())
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/static/vendor/xterm.js", get(xterm_js))
        .route("/static/vendor/xterm-addon-fit.js", get(xterm_fit_js))
        .route("/static/vendor/xterm.css", get(xterm_css))
        .route("/task", post(task))
        .route("/contract/confirm", post(confirm_contract))
        .route("/resolve", post(resolve))
        .route("/events", get(events_ws))
        .route("/terminal", get(terminal_ws))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn xterm_js() -> Response {
    asset("application/javascript; charset=utf-8", XTERM_JS)
}

async fn xterm_fit_js() -> Response {
    asset("application/javascript; charset=utf-8", XTERM_FIT_JS)
}

async fn xterm_css() -> Response {
    asset("text/css; charset=utf-8", XTERM_CSS)
}

#[derive(Deserialize)]
struct TaskRequest {
    request: String,
}

async fn task(State(state): State<AppState>, Json(payload): Json<TaskRequest>) -> Response {
    match extract_intent(&state.http, &payload.request).await {
        Ok(contract) => Json(toggle_card(&contract)).into_response(),
        Err(ConsoleError::NoCredentials) | Err(ConsoleError::UpstreamStatus(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "OpenAI credentials are unavailable",
        )
            .into_response(),
        Err(_) => (StatusCode::BAD_GATEWAY, "could not create a contract").into_response(),
    }
}

async fn confirm_contract(State(state): State<AppState>, Json(payload): Json<Value>) -> Response {
    let raw = payload
        .get("contract")
        .cloned()
        .unwrap_or_else(|| payload.clone());
    let contract: ContractSpec = match serde_json::from_value(raw) {
        Ok(contract) => contract,
        Err(_) => return (StatusCode::UNPROCESSABLE_ENTITY, "invalid contract").into_response(),
    };
    match state.daemon.set_contract(contract.daemon_wire()).await {
        Ok(acknowledgement) => {
            if let Ok(mut active) = state.active_task.lock() {
                *active = contract.task.clone();
            }
            Json(json!({ "contract": contract, "daemon": acknowledgement })).into_response()
        }
        Err(_) => (StatusCode::BAD_GATEWAY, "daemon unreachable").into_response(),
    }
}

#[derive(Deserialize)]
struct ResolveRequest {
    id: String,
    decision: String,
}

async fn resolve(State(state): State<AppState>, Json(payload): Json<ResolveRequest>) -> Response {
    if payload.decision != "approve_once" && payload.decision != "reject" {
        return (StatusCode::UNPROCESSABLE_ENTITY, "invalid decision").into_response();
    }
    match state.daemon.resolve(&payload.id, &payload.decision).await {
        Ok(acknowledgement) => Json(acknowledgement).into_response(),
        Err(_) => (StatusCode::BAD_GATEWAY, "daemon unreachable").into_response(),
    }
}

async fn events_ws(upgrade: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    upgrade.on_upgrade(move |socket| bridge_events(socket, state))
}

async fn terminal_ws(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(bridge_terminal)
}

/// Forward daemon events to the browser; attach an advisory explanation to each
/// block without stalling the stream.
async fn bridge_events(socket: WebSocket, state: AppState) {
    let (mut sink, mut incoming) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(128);

    // Single writer owns the socket sink; every producer sends through `tx`.
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    // Detect the browser closing the socket.
    let closer = tokio::spawn(async move { while let Some(Ok(_)) = incoming.next().await {} });

    if let Ok(mut lines) = state.daemon.subscribe().await {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if tx.send(Message::Text(line.into())).await.is_err() {
                break;
            }
            if event.get("type").and_then(Value::as_str) == Some("blocked") {
                spawn_explanation(tx.clone(), state.http.clone(), state.active_task(), event);
            }
        }
    }

    drop(tx);
    let _ = writer.await;
    closer.abort();
}

fn spawn_explanation(
    tx: mpsc::Sender<Message>,
    http: reqwest::Client,
    active_task: String,
    event: Value,
) {
    tokio::spawn(async move {
        let task = event
            .get("task")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or(active_task);
        let command = event
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or("the command");
        let evidence = event
            .get("proofs")
            .or_else(|| event.get("twin_diff"))
            .or_else(|| event.get("reason"))
            .cloned()
            .unwrap_or(Value::Null);
        let sentence = explain::explain_divergence(&http, &task, command, &evidence).await;

        let mut enriched = event;
        enriched["explanation"] = json!(sentence);
        enriched["event_update"] = json!(true);
        if enriched.get("explainer_model").is_none() {
            let model = env::var("EXPLAIN_MODEL").unwrap_or_else(|_| "gpt-5.6-luna".to_owned());
            enriched["explainer_model"] = json!(model);
        }
        let _ = tx.send(Message::Text(enriched.to_string().into())).await;
    });
}
