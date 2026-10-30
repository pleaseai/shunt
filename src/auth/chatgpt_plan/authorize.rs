//! The pure pieces of the sign-in: the authorize URL, binding the callback's
//! client id to the saved registration, and reading a pasted callback URL.

use anyhow::{bail, Context};

use crate::auth::callback::CallbackParams;

/// The placeholder client id of a first sign-in. The authorization issues the
/// real `oaiapp_...` id and returns it on the callback.
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
pub const AGENT_NAME_HINT: &str = "shunt";
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// Scopes requested at sign-in.
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The scope without which the access token cannot call the public API.
pub const PLAN_USAGE_SCOPE: &str = "chatgpt.tokens.use.direct";

/// Everything that varies per attempt.
pub struct AuthorizeRequest<'a> {
    pub authorize_url: &'a str,
    /// `Some` on a re-sign-in; `None` registers a new client.
    pub saved_client_id: Option<&'a str>,
    pub host_id: &'a str,
    pub redirect_uri: &'a str,
    pub state: &'a str,
    pub nonce: &'a str,
    pub challenge: &'a str,
}

pub fn build_url(request: &AuthorizeRequest<'_>) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(request.authorize_url)?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair(
            "client_id",
            request.saved_client_id.unwrap_or(DYNAMIC_CLIENT_ID),
        );
        // Only a first sign-in names the agent; a re-sign-in must not rename
        // the registration.
        if request.saved_client_id.is_none() {
            query.append_pair("agent_name_hint", AGENT_NAME_HINT);
        }
        query
            .append_pair("ext_agent_host_id", request.host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", request.redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("resource", RESOURCE)
            .append_pair("state", request.state)
            .append_pair("nonce", request.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", request.challenge);
    }
    Ok(url)
}

/// The client id to exchange the code with.
///
/// A saved registration is binding: the callback may omit the id or repeat it,
/// never name another. Without one the callback must carry the issued id.
pub fn bind_client_id(saved: Option<&str>, callback: Option<&str>) -> anyhow::Result<String> {
    match (saved, callback) {
        (Some(saved), None) => Ok(saved.to_string()),
        (Some(saved), Some(returned)) if returned == saved => Ok(saved.to_string()),
        (Some(_), Some(_)) => bail!(
            "the authorization returned a different client id than this account's saved \
             registration; the account file was not changed. Sign in with a new --name, or \
             remove the account file to register again"
        ),
        (None, Some(returned)) if !returned.is_empty() && returned != DYNAMIC_CLIENT_ID => {
            Ok(returned.to_string())
        }
        (None, _) => bail!(
            "registration incomplete: the authorization did not issue a client id; nothing was \
             stored. Try again"
        ),
    }
}

/// Read a pasted callback URL. Surrounding whitespace and quotes (a shell
/// copy often adds them) are dropped. The state is checked here, so a pasted
/// URL from another attempt never reaches the token endpoint.
pub(crate) fn parse_pasted(input: &str, expected_state: &str) -> anyhow::Result<CallbackParams> {
    let cleaned = input.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    let url = reqwest::Url::parse(cleaned)
        .context("the pasted text is not a callback URL (expected http://127.0.0.1:.../...)")?;
    let (mut code, mut state, mut error) = (None, None, None);
    let (mut client_id, mut scope) = (None, None);
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "error" => &mut error,
            "client_id" => &mut client_id,
            "scope" => &mut scope,
            _ => continue,
        };
        *slot = Some(value.into_owned()).filter(|value| !value.is_empty());
    }
    if state.as_deref() != Some(expected_state) {
        bail!("the pasted callback belongs to a different sign-in attempt (state mismatch)");
    }
    if let Some(error) = error {
        bail!("ChatGPT authorization was denied ({error})");
    }
    let code = code.context("the pasted callback URL has no authorization code")?;
    Ok(CallbackParams {
        code,
        client_id,
        scope,
    })
}
