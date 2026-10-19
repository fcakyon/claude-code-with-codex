//! Optional token on the proxy's own routes, for a bind address other machines can reach.
//!
//! Claude Code sends its subscription login in `Authorization` and the Anthropic
//! passthrough forwards it, so the proxy token travels in its own header, set through
//! `ANTHROPIC_CUSTOM_HEADERS`. A bearer token is accepted too, for OpenAI-compatible
//! clients that have nowhere else to put a key.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};

use crate::{anthropic::json_error, auth::constant_time_eq};

pub const PROXY_KEY_HEADER: &str = "x-claude-codex-key";

pub async fn require_token(
    State(token): State<Arc<str>>,
    mut request: Request,
    next: Next,
) -> Response {
    // Liveness probes, and the monitor, which already answers only loopback peers.
    if matches!(request.uri().path(), "/healthz" | "/monitor") {
        return next.run(request).await;
    }
    let key_ok = request
        .headers()
        .get(PROXY_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| constant_time_eq(value.trim(), &token));
    let bearer_ok = bearer(request.headers()).is_some_and(|value| constant_time_eq(value, &token));
    if !key_ok && !bearer_ok {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            format!(
                "missing or invalid proxy token; send `{PROXY_KEY_HEADER}: <token>` or `Authorization: Bearer <token>`"
            ),
        );
    }
    // The proxy token is for this hop only. A different value is the Claude login and stays.
    if bearer_ok {
        request.headers_mut().remove(AUTHORIZATION);
    }
    next.run(request).await
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim()
        .split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::get};
    use tower::util::ServiceExt;

    /// Answers with the `Authorization` header the routed handler received.
    async fn call(path: &str, headers: &[(&str, &str)]) -> (StatusCode, String) {
        let app = Router::new()
            .route(
                "/{*path}",
                get(|headers: HeaderMap| async move {
                    headers
                        .get(AUTHORIZATION)
                        .map(|value| value.to_str().unwrap().to_string())
                        .unwrap_or_default()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                Arc::<str>::from("s3cret"),
                require_token,
            ));
        let mut request = axum::http::Request::get(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn token_gates_routes_and_keeps_the_claude_login() {
        assert_eq!(call("/v1/models", &[]).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            call("/v1/models", &[(PROXY_KEY_HEADER, "wrong")]).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(call("/healthz", &[]).await.0, StatusCode::OK);
        assert_eq!(
            call(
                "/v1/messages",
                &[
                    (PROXY_KEY_HEADER, "s3cret"),
                    ("authorization", "Bearer claude-login")
                ]
            )
            .await,
            (StatusCode::OK, "Bearer claude-login".to_string())
        );
        assert_eq!(
            call(
                "/v1/chat/completions",
                &[("authorization", "bearer s3cret")]
            )
            .await,
            (StatusCode::OK, String::new())
        );
    }
}
