#![allow(dead_code)]
//! Login / OAuth flow.
//!
//! Mirrors Python: hermes login (OAuth login for supported providers)

use console::Style;
use std::time::Duration;

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn dim() -> Style { Style::new().dim() }

/// Known OAuth provider configurations.
struct ProviderConfig {
    id: &'static str,
    name: &'static str,
    flow: OAuthFlow,
}

enum OAuthFlow {
    /// RFC 8628 device code flow.
    DeviceCode {
        device_code_url: &'static str,
        token_url: &'static str,
        client_id: &'static str,
        scope: &'static str,
    },
    /// Authorization code flow with local callback server.
    AuthorizationCode {
        authorize_url: &'static str,
        token_url: &'static str,
        client_id: &'static str,
        scope: &'static str,
    },
}

static PROVIDERS: &[ProviderConfig] = &[
    ProviderConfig {
        id: "github",
        name: "GitHub",
        flow: OAuthFlow::DeviceCode {
            device_code_url: "https://github.com/login/device/code",
            token_url: "https://github.com/login/oauth/access_token",
            client_id: "Ov23li8tweQw6odWQebz", // Same as Copilot CLI
            scope: "read:user",
        },
    },
    ProviderConfig {
        id: "google",
        name: "Google",
        flow: OAuthFlow::AuthorizationCode {
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth?client_id={client_id}&redirect_uri={redirect_uri}&response_type=code&scope={scope}&state={state}&access_type=offline",
            token_url: "https://oauth2.googleapis.com/token",
            client_id: "", // Must be provided by user
            scope: "openid email profile https://www.googleapis.com/auth/generative-language",
        },
    },
    ProviderConfig {
        id: "discord",
        name: "Discord",
        flow: OAuthFlow::AuthorizationCode {
            authorize_url: "https://discord.com/oauth2/authorize?client_id={client_id}&redirect_uri={redirect_uri}&response_type=code&scope={scope}&state={state}",
            token_url: "https://discord.com/api/oauth2/token",
            client_id: "", // Must be provided by user
            scope: "identify email",
        },
    },
    ProviderConfig {
        id: "nous",
        name: "Nous Portal",
        flow: OAuthFlow::DeviceCode {
            device_code_url: "https://portal.nousresearch.com/oauth/device/code",
            token_url: "https://portal.nousresearch.com/oauth/token",
            client_id: "hermes-cli",
            scope: "inference:mint_agent_key",
        },
    },
];

/// Interactive login via OAuth.
pub fn cmd_login(
    provider: &str,
    client_id: Option<&str>,
    no_browser: bool,
    scopes: Option<&str>,
    _portal_url: Option<&str>,
    _inference_url: Option<&str>,
    timeout: Option<f64>,
    _ca_bundle: Option<&str>,
    _insecure: bool,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(cmd_login_async(provider, client_id, no_browser, scopes, _portal_url, _inference_url, timeout, _ca_bundle, _insecure))
}

async fn cmd_login_async(
    provider: &str,
    client_id: Option<&str>,
    no_browser: bool,
    scopes: Option<&str>,
    _portal_url: Option<&str>,
    _inference_url: Option<&str>,
    timeout: Option<f64>,
    _ca_bundle: Option<&str>,
    _insecure: bool,
) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ OAuth Login"));
    println!();

    let provider_lc = provider.to_lowercase();
    let config = PROVIDERS.iter().find(|p| p.id == provider_lc);

    let Some(config) = config else {
        println!("  {} Provider '{provider}' does not support OAuth login.", yellow().apply_to("⚠"));
        println!("  Supported providers: {}", PROVIDERS.iter().map(|p| p.id).collect::<Vec<_>>().join(", "));
        println!();
        println!("  {}", dim().apply_to("Use `hermes auth add <provider> --key <api_key>` for API key auth."));
        println!();
        return Ok(());
    };

    println!("  Provider: {}", config.name);
    if let Some(cid) = client_id {
        println!("  Client ID: {cid}");
    }
    if no_browser {
        println!("  Browser: disabled (manual URL open required)");
    }
    if let Some(s) = scopes {
        println!("  Scopes: {s}");
    }
    println!();

    let timeout = timeout.map(Duration::from_secs_f64).unwrap_or(Duration::from_secs(600));

    match &config.flow {
        OAuthFlow::DeviceCode {
            device_code_url,
            token_url,
            client_id: default_client_id,
            scope: default_scope,
        } => {
            let cid = client_id.unwrap_or(default_client_id);
            let scope = scopes.unwrap_or(default_scope);

            crate::oauth_flow::device_code_login(
                config.id,
                device_code_url,
                token_url,
                cid,
                scope,
                timeout,
                no_browser,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        OAuthFlow::AuthorizationCode {
            authorize_url,
            token_url,
            client_id: default_client_id,
            scope: default_scope,
        } => {
            let cid = client_id
                .or({
                    if default_client_id.is_empty() {
                        None
                    } else {
                        Some(*default_client_id)
                    }
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("Provider '{}' requires --client-id", config.id)
                })?;
            let scope = scopes.unwrap_or(default_scope);

            crate::oauth_flow::authorization_code_login(
                config.id,
                authorize_url,
                token_url,
                cid,
                None, // No client secret for public clients
                scope,
                timeout,
                no_browser,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }

    println!();
    Ok(())
}

/// Logout from an OAuth provider.
pub fn cmd_oauth_logout(provider: &str) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ OAuth Logout"));
    println!();

    match crate::oauth_store::delete_credential(provider) {
        Ok(()) => {
            println!("  {} Logged out from '{}'.", green().apply_to("✓"), provider);
        }
        Err(crate::oauth_store::SecureStoreError::NotFound) => {
            println!("  {} No OAuth credentials found for '{}'.", yellow().apply_to("→"), provider);
        }
        Err(e) => {
            println!("  {} Failed to remove credentials: {e}", yellow().apply_to("⚠"));
        }
    }

    // Also clear legacy auth.json state if present
    let mut auth_store = crate::auth_cmd::load_auth_store()?;
    if auth_store.providers.remove(provider).is_some() {
        crate::auth_cmd::save_auth_store(&mut auth_store)?;
    }

    println!();
    Ok(())
}

/// Refresh OAuth tokens for a provider.
pub async fn cmd_oauth_refresh(
    provider: &str,
    client_id: Option<&str>,
) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ OAuth Token Refresh"));
    println!();

    let provider_lc = provider.to_lowercase();
    let config = PROVIDERS.iter().find(|p| p.id == provider_lc);

    let Some(config) = config else {
        println!("  {} Unknown provider '{provider}'.", yellow().apply_to("⚠"));
        return Ok(());
    };

    let token_url = match &config.flow {
        OAuthFlow::DeviceCode { token_url, .. } => token_url,
        OAuthFlow::AuthorizationCode { token_url, .. } => token_url,
    };

    let cid = client_id.unwrap_or(match &config.flow {
        OAuthFlow::DeviceCode { client_id, .. } => client_id,
        OAuthFlow::AuthorizationCode { client_id, .. } => client_id,
    });

    match crate::oauth_flow::refresh_access_token(config.id, token_url, cid, None).await {
        Ok(_) => {
            println!("  {} Token refreshed for '{}'.", green().apply_to("✓"), config.name);
        }
        Err(crate::oauth_flow::OAuthError::MissingConfig(msg)) => {
            println!("  {} {msg}", yellow().apply_to("⚠"));
        }
        Err(e) => {
            println!("  {} Refresh failed: {e}", yellow().apply_to("⚠"));
        }
    }

    println!();
    Ok(())
}

/// List OAuth logins.
pub fn cmd_oauth_list() -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ OAuth Accounts"));
    println!();

    let providers = crate::oauth_store::list_stored_providers();
    if providers.is_empty() {
        println!("  {}", dim().apply_to("No OAuth accounts configured."));
        println!();
        println!("  {}", dim().apply_to("Run `hermes login <provider>` to authenticate."));
    } else {
        for p in &providers {
            match crate::oauth_store::retrieve_credential(p) {
                Ok(cred) => {
                    let status = if crate::oauth_flow::needs_refresh(&cred) {
                        yellow().apply_to("needs refresh").to_string()
                    } else {
                        green().apply_to("active").to_string()
                    };
                    println!("  {} {} [{}]", green().apply_to("✓"), p, status);
                }
                Err(_) => {
                    println!("  {} {} [error reading]", yellow().apply_to("⚠"), p);
                }
            }
        }
    }
    println!();
    Ok(())
}
