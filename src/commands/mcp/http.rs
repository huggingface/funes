//! Socket ownership and endpoint policy for the SDK's Streamable HTTP service.

use super::Funes;
use anyhow::{ensure, Context, Result};
use axum::extract::{Request, State};
use axum::http::uri::{Authority, Uri};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use std::future::IntoFuture;
use std::net::{IpAddr, SocketAddr};
use tokio_util::sync::CancellationToken;

/// Run the same read tools on `/mcp`; memory selection and model ownership stay in Funes.
pub async fn run(
    memory: Option<String>,
    bind: SocketAddr,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
) -> Result<()> {
    validate_authorities(&allowed_hosts, &allowed_origins)?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding MCP HTTP listener at {bind}"))?;
    // Use the actual port for Origin checks, including when the caller requested port zero.
    let address = listener.local_addr().context("reading MCP listen address")?;
    let cancellation = CancellationToken::new();
    let config =
        endpoint_config(address, allowed_hosts, allowed_origins).with_cancellation_token(cancellation.child_token());
    let service = StreamableHttpService::new(
        move || Ok(Funes::new(memory.clone())),
        LocalSessionManager::default().into(),
        config,
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(from_fn_with_state(cancellation.clone(), cancel_on_shutdown));
    eprintln!("MCP listening at http://{address}/mcp");
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(cancellation.clone().cancelled_owned())
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.context("serving MCP HTTP"),
        signal = shutdown_signal() => {
            cancellation.cancel();
            signal?;
            server.await.context("shutting down MCP HTTP")
        }
    }
}

async fn cancel_on_shutdown(State(cancellation): State<CancellationToken>, request: Request, next: Next) -> Response {
    // Include body reads so an unfinished upload cannot hold up graceful shutdown.
    tokio::select! {
        response = next.run(request) => response,
        _ = cancellation.cancelled() => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn validate_authorities(hosts: &[String], origins: &[String]) -> Result<()> {
    for host in hosts {
        if host.parse::<IpAddr>().is_ok() {
            continue;
        }
        let error = format!("invalid --allowed-host {host:?}: expected a hostname or host:port authority");
        let authority = host.parse::<Authority>().with_context(|| error.clone())?;
        let valid = valid_authority(&authority) && !host.contains(['@', '*']) && host.trim() == host;
        ensure!(valid, "{error}");
    }
    for origin in origins {
        let error = format!("invalid --allowed-origin {origin:?}: expected an http:// or https:// origin");
        let uri = origin.parse::<Uri>().with_context(|| error.clone())?;
        let http_scheme = matches!(uri.scheme_str(), Some("http" | "https"));
        let authority = uri.authority().is_some_and(valid_authority) && !origin.contains(['@', '*']);
        let no_path_or_query = matches!(uri.path(), "" | "/") && uri.query().is_none();
        ensure!(http_scheme && authority && no_path_or_query, "{error}");
    }
    Ok(())
}

fn valid_authority(authority: &Authority) -> bool {
    // An Authority can parse even when its explicit port is not a valid socket port.
    let remainder = authority.as_str().strip_prefix(authority.host()).unwrap_or("");
    !authority.host().is_empty()
        && (remainder.is_empty()
            || remainder
                .strip_prefix(':')
                .is_some_and(|port| port.parse::<u16>().is_ok()))
}

fn endpoint_config(address: SocketAddr, hosts: Vec<String>, origins: Vec<String>) -> StreamableHttpServerConfig {
    // The SDK routes modern requests statelessly and manages sessions for older clients.
    let mut config = StreamableHttpServerConfig::default().with_json_response(true);
    config.allowed_origins = ["localhost", "127.0.0.1", "[::1]"]
        .map(|host| format!("http://{host}:{}", address.port()))
        .into();
    if !address.ip().is_unspecified() {
        config.allowed_hosts.push(address.ip().to_string());
        config.allowed_origins.push(format!("http://{address}"));
    }
    config.allowed_hosts.extend(hosts);
    config.allowed_origins.extend(origins);
    config
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).context("registering SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("waiting for Ctrl-C"),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.context("waiting for Ctrl-C")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_bind_keeps_explicit_authorities_and_concrete_bind_adds_its_address() {
        let config = endpoint_config("0.0.0.0:8000".parse().unwrap(), vec![], vec![]);
        assert_eq!(config.allowed_hosts, ["localhost", "127.0.0.1", "::1"]);
        assert!(!config.allowed_origins.iter().any(|s| s.contains("0.0.0.0")));
        let config = endpoint_config("[2001:db8::1]:8123".parse().unwrap(), vec![], vec![]);
        assert!(config.allowed_hosts.contains(&"2001:db8::1".to_string()));
        assert!(config
            .allowed_origins
            .contains(&"http://[2001:db8::1]:8123".to_string()));
    }

    #[test]
    fn reject_misconfigured_authority_lists() {
        for host in [
            "",
            "*",
            "http://example.com",
            "user@example.com",
            "example.com/path",
            "example.com:abc",
            "example.com:99999",
            "example.com:",
        ] {
            assert!(validate_authorities(&[host.into()], &[]).is_err(), "{host}");
        }
        for origin in [
            "",
            "*",
            "example.com",
            "https://example.com/path",
            "https://example.com?query",
            "https://example.com:abc",
            "https://example.com:99999",
        ] {
            assert!(validate_authorities(&[], &[origin.into()]).is_err(), "{origin}");
        }
        assert!(validate_authorities(
            &["example.com:8000".into(), "[::1]:8000".into()],
            &["https://example.com".into()]
        )
        .is_ok());
    }
}
