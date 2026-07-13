use crate::anthropic::schema::MessagesRequest;
use crate::monitor::MonitorHandle;
use crate::traffic::TrafficCapture;
use anyhow::Result;
use async_trait::async_trait;
use axum::response::Response;
use clap::Subcommand;
use std::sync::Arc;

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Sign in using browser-based authentication
    Login,
    /// Sign in using a device code
    Device,
    /// Show the current authentication status
    Status,
    /// Delete stored authentication credentials
    Logout,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_models(&self) -> Vec<String>;
    fn cli(&self) -> &'static dyn CliHandlers;
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response;
    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response;
}

pub trait CliHandlers: Send + Sync {
    fn login(&self) -> Result<()>;
    fn device(&self) -> Result<()>;
    fn status(&self) -> Result<()>;
    fn logout(&self) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: String,
    pub traffic: Option<Arc<TrafficCapture>>,
    pub monitor: Option<MonitorHandle>,
    /// Raw request material for byte-passthrough providers (the Anthropic backend).
    /// Present on real HTTP requests; None in unit tests. Forwarding these verbatim
    /// keeps the prompt-cache prefix byte-identical.
    pub passthrough: Option<Passthrough>,
}

/// Untranslated request material needed to relay a request to an upstream verbatim.
#[derive(Debug, Clone)]
pub struct Passthrough {
    /// Original request body bytes, forwarded without reserialization.
    pub raw_body: axum::body::Bytes,
    /// Original client request headers (carry Authorization + anthropic-beta).
    pub headers: axum::http::HeaderMap,
    /// Original path and query, e.g. `/v1/messages?beta=true`.
    pub path_and_query: String,
}
