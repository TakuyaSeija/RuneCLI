//! Bounded HTTP bodies around the SDK transport, including JSON error responses.
use futures_util::{StreamExt, TryStreamExt};
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::common::client_side_sse::BoxedSseResponse;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, StreamableHttpClient, StreamableHttpError,
    StreamableHttpPostResponse,
};
use std::collections::HashMap;
use std::sync::Arc;
use ureq::http::{HeaderName, HeaderValue};

#[derive(Clone)]
pub struct Http {
    pub client: reqwest::Client,
    pub max: usize,
}

impl StreamableHttpClient for Http {
    type Error = reqwest::Error;
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.client
            .delete_session(uri, session, auth, headers)
            .await
    }
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
        self.client
            .get_stream_with_max_sse_event_size(uri, session, last, auth, headers, self.max)
            .await
    }
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut request = self
            .client
            .post(uri.as_ref())
            .header("Accept", "application/json, text/event-stream");
        if let Some(auth) = auth {
            request = request.bearer_auth(auth);
        }
        for (key, value) in headers {
            if [
                "authorization",
                "host",
                "content-length",
                "content-type",
                "cookie",
            ]
            .contains(&key.as_str())
            {
                return Err(StreamableHttpError::ReservedHeaderConflict(key.to_string()));
            }
            request = request.header(key, value);
        }
        if let Some(session) = &session {
            request = request.header("Mcp-Session-Id", session.as_ref());
        }
        let mut response = request
            .json(&message)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;
        let status = response.status();
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if status == 401 {
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                challenge,
            )));
        }
        if status == 403 {
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(challenge, None),
            ));
        }
        if status == 404 && session.is_some() {
            return Err(StreamableHttpError::SessionExpired);
        }
        if status == 202 || status == 204 {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if mime.starts_with("text/event-stream") && status.is_success() {
            let max = self.max;
            let bounded = response.bytes_stream().map_err(std::io::Error::other).scan(
                (0usize, false),
                move |(size, ended), chunk| {
                    if *ended {
                        return futures_util::future::ready(None);
                    }
                    let result = chunk.and_then(|bytes| {
                        *size += bytes.len();
                        if *size > max {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "MCP response exceeded its byte budget",
                            ))
                        } else {
                            Ok(bytes)
                        }
                    });
                    *ended = result.is_err();
                    futures_util::future::ready(Some(result))
                },
            );
            let stream = sse_stream::SseStream::from_bytes_stream(bounded).boxed();
            return Ok(StreamableHttpPostResponse::Sse(stream, session));
        }
        let mut body = Vec::new();
        while let Some(bytes) = response
            .chunk()
            .await
            .map_err(StreamableHttpError::Client)?
        {
            if body.len() + bytes.len() > self.max {
                return Err(StreamableHttpError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "MCP response exceeded its byte budget",
                )));
            }
            body.extend_from_slice(&bytes);
        }
        if let Ok(message) = serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
            return Ok(StreamableHttpPostResponse::Json(message, session));
        }
        let request = serde_json::to_value(&message)?;
        if request["method"] == "server/discover" && status.is_client_error() {
            let error = serde_json::from_value(
                serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":"Legacy MCP discovery endpoint"}}),
            )?;
            return Ok(StreamableHttpPostResponse::Json(error, None));
        }
        if status.is_success() && body.is_empty() && request.get("id").is_none() {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        Err(StreamableHttpError::UnexpectedServerResponse(
            "Invalid MCP HTTP response".into(),
        ))
    }
}
