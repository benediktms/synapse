use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct MintResponse {
    token: String,
}

/// Mint a new platform API token named `name`, authenticated by an existing token.
/// The Turso platform allows any valid token to create siblings, so setup can turn
/// a pasted long-lived token into a machine-scoped one and store only the latter.
pub fn mint_token(existing: &str, name: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("https://api.turso.tech/v1/auth/api-tokens/{name}");
    let response = client
        .post(&url)
        .bearer_auth(existing)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        return Err(format!("minting token {name} failed ({status}): {body}"));
    }
    response
        .json::<MintResponse>()
        .map(|minted| minted.token)
        .map_err(|e| format!("minting token {name}: unreadable response: {e}"))
}

/// A default per-machine token name: `synapse-<hostname>`, folded to the
/// `[a-z0-9-]` shape token names accept.
pub fn machine_token_name() -> String {
    let host = hostname().unwrap_or_else(|| "machine".to_string());
    sanitize(&format!("synapse-{host}"))
}

#[cfg(unix)]
fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0)?;
    String::from_utf8(buf[..end].to_vec()).ok()
}

#[cfg(windows)]
fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
}

fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_folds_to_token_name_shape() {
        assert_eq!(
            sanitize("synapse-Bens-MacBook.local"),
            "synapse-bens-macbook-local"
        );
        assert_eq!(sanitize("synapse-héllo_box"), "synapse-h-llo-box");
        assert_eq!(sanitize("--edge--"), "edge");
    }

    #[test]
    fn machine_token_name_is_never_empty() {
        let name = machine_token_name();
        assert!(name.starts_with("synapse"));
        assert!(!name.ends_with('-'));
    }
}
