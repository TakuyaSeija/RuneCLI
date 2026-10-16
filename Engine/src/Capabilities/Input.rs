//! MCP user input uses the existing typed broker and real host responses.
use crate::perm::broker::{Answer, Broker, Response};
use crate::session::live::Bus;
use crate::{panic, proto::Event};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub action: String,
    #[serde(default)]
    pub content: Option<Value>,
}
impl Response for Reply {
    type Request = Value;
    fn valid(&self, request: &Value) -> bool {
        match self.action.as_str() {
            "decline" | "cancel" => self.content.is_none(),
            "accept" if request["mode"] == "url" => self.content.is_none(),
            "accept" => self.content.as_ref().is_some_and(|c| {
                c.to_string().len() <= 32768
                    && crate::tools::validate_value(&request["requestedSchema"], c).is_ok()
            }),
            _ => false,
        }
    }
}

pub struct Input {
    pub broker: Arc<Broker<Reply>>,
    pub events: Option<Bus>,
    pub session: i64,
    pub operation: Mutex<Option<(String, Arc<AtomicBool>)>>,
    pub changed: AtomicBool,
}
impl Input {
    pub fn new(session: i64, events: Option<Bus>) -> Arc<Self> {
        Arc::new(Self {
            broker: Arc::new(Broker::new()),
            events,
            session,
            operation: Mutex::new(None),
            changed: AtomicBool::new(false),
        })
    }
}

#[derive(Clone)]
pub struct Handler {
    pub input: Arc<Input>,
    pub server: String,
}
impl rmcp::ClientHandler for Handler {
    fn get_info(&self) -> rmcp::model::ClientInfo {
        serde_json::from_value(json!({"protocolVersion":"2026-07-28","clientInfo":{"name":"Rune","version":crate::VERSION},"capabilities":if self.input.events.is_some(){json!({"elicitation":{"form":{},"url":{}}})}else{json!({})}})).expect("static MCP client metadata")
    }
    async fn create_elicitation(
        &self,
        request: rmcp::model::ElicitRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleClient>,
    ) -> std::result::Result<rmcp::model::ElicitResult, rmcp::model::ErrorData> {
        let refused =
            || serde_json::from_value(json!({"action":"decline"})).expect("static decline result");
        let Some(events) = self.input.events.clone() else {
            return Ok(refused());
        };
        let payload = serde_json::to_value(request)
            .map_err(|_| rmcp::model::ErrorData::invalid_params("Invalid MCP form", None))?;
        if payload.to_string().len() > 32768 {
            return Ok(refused());
        }
        if payload["mode"] == "url" {
            let Some(url) = payload["url"]
                .as_str()
                .and_then(|s| reqwest::Url::parse(s).ok())
            else {
                return Ok(refused());
            };
            if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
                return Ok(refused());
            }
        }
        let operation = panic::lock(&self.input.operation).clone();
        let Some((call, cancel)) = operation else {
            return Ok(refused());
        };
        let req = self.input.broker.open(payload.clone());
        let _ = events.send(Event::McpInputRequest {
            session: self.input.session,
            id: call,
            req: req.clone(),
            server_id: self.server.clone(),
            request: payload,
        });
        let broker = self.input.broker.clone();
        let wait_req = req.clone();
        let answer = tokio::task::spawn_blocking(move || {
            broker.wait(&wait_req, &cancel, std::time::Duration::from_secs(300))
        })
        .await;
        let _ = events.send(Event::McpInputResolved {
            session: self.input.session,
            req,
        });
        let reply = match answer {
            Ok(Answer::Decided(reply)) => reply,
            _ => Reply {
                action: "cancel".into(),
                content: None,
            },
        };
        serde_json::from_value(serde_json::to_value(reply).expect("serializable reply"))
            .map_err(|_| rmcp::model::ErrorData::invalid_params("Invalid MCP input reply", None))
    }
    async fn on_tool_list_changed(&self, _: rmcp::service::NotificationContext<rmcp::RoleClient>) {
        self.input.changed.store(true, Ordering::Relaxed);
    }
    async fn on_resource_list_changed(
        &self,
        _: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        self.input.changed.store(true, Ordering::Relaxed);
    }
    async fn on_prompt_list_changed(
        &self,
        _: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        self.input.changed.store(true, Ordering::Relaxed);
    }
}
