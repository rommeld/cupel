//! OAuth login flows for subscription providers.
//!
//! Providers like OpenAI's ChatGPT backend are not driven by a pasted API
//! key but by short-lived OAuth access tokens obtained through a browser
//! (or device-code) login. This module holds the PROTOCOL side of those
//! flows: PKCE, authorize URLs, token exchange and refresh, and the local
//! callback server the browser redirects to.

pub mod openai_codex;
pub mod pkce;
