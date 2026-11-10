// SPDX-License-Identifier: Apache-2.0

//! Pairing codes that carry their server: `<code>@<host>`, such as `K7QM-2XPD@flarebox.alice.workers.dev`
//! (docs/design/cli-setup.md section 1).

/// A pairing code and the server it was issued by.
#[derive(Debug, PartialEq, Eq)]
pub struct Invitation {
    pub code: String,
    pub server: String,
}

/// Splits `text` on its last `@` and puts `https://` before the host. `server` replaces the host, and is required
/// for a bare code.
pub fn parse(text: &str, server: Option<&str>) -> Result<Invitation, String> {
    let text = text.trim();
    let (code, host) = match text.rsplit_once('@') {
        Some((code, host)) => (code, Some(host)),
        None => (text, None),
    };
    if code.is_empty() {
        return Err(format!("{text:?} has no pairing code before the @"));
    }
    let server = match (server, host) {
        (Some(server), _) => server.to_owned(),
        (None, Some(host)) if !host.is_empty() && !host.contains('/') => format!("https://{host}"),
        (None, Some(host)) => return Err(format!("{host:?} is not a server host name")),
        (None, None) => {
            return Err(format!(
                "{text:?} names no server; use the code shown as code@host, or add --server"
            ));
        }
    };
    Ok(Invitation {
        code: code.to_owned(),
        server,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_carries_its_server() {
        assert_eq!(
            parse(" K7QM-2XPD@flarebox.alice.workers.dev ", None),
            Ok(Invitation {
                code: "K7QM-2XPD".into(),
                server: "https://flarebox.alice.workers.dev".into(),
            })
        );
    }

    #[test]
    fn the_last_at_sign_separates_the_host() {
        assert_eq!(parse("a@b@files.example.com", None).unwrap().code, "a@b");
    }

    #[test]
    fn server_replaces_the_host_and_completes_a_bare_code() {
        let local = Some("http://127.0.0.1:8787");
        assert_eq!(
            parse("K7QM-2XPD", local).unwrap().server,
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            parse("K7QM-2XPD@files.example.com", local).unwrap().server,
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn refuses_a_code_without_a_server_or_a_bad_host() {
        assert!(parse("K7QM-2XPD", None).unwrap_err().contains("--server"));
        assert!(parse("@files.example.com", None).is_err());
        assert!(parse("K7QM-2XPD@", None).is_err());
        assert!(parse("K7QM-2XPD@https://files.example.com", None).is_err());
    }
}
