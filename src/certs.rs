//! Bring-your-own certificate for a routing domain. Found at a fixed path under the
//! global config dir so issuance stays the user's tool's job and `.wsx.json` names nothing.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::remote::Site;

const CERT_FILE: &str = "fullchain.pem";
const KEY_FILE: &str = "privkey.pem";

pub fn dir(domain: &str) -> Result<PathBuf> {
    // `domain` arrives with a possibly-cloned `.wsx.json` and becomes a path segment.
    if domain.is_empty()
        || domain.starts_with('.')
        || !domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        bail!("`{domain}` isn't a plain domain name, so it can't name a certificate directory");
    }
    Ok(crate::config::global_dir()?.join("certs").join(domain))
}

pub fn cert_path(domain: &str) -> Result<PathBuf> {
    Ok(dir(domain)?.join(CERT_FILE))
}

/// The files are the source of truth: present ⇒ pushed to Caddy, absent ⇒ unloaded, so deleting them reverts to the internal CA.
pub fn sync(site: &Site, domain: &str) -> Result<Option<PathBuf>> {
    let dir = dir(domain)?;
    let (cert, key) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
    match (cert.is_file(), key.is_file()) {
        (false, false) => {
            crate::caddy::unload_cert(site, domain);
            Ok(None)
        }
        (true, true) => {
            let cert_pem = read(&cert)?;
            let key_pem = read(&key)?;
            crate::caddy::load_cert(site, domain, &cert_pem, &key_pem)?;
            Ok(Some(dir))
        }
        (has_cert, _) => bail!(
            "{} has {} but no {} — both are needed",
            dir.display(),
            if has_cert { CERT_FILE } else { KEY_FILE },
            if has_cert { KEY_FILE } else { CERT_FILE },
        ),
    }
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// SAN DNS names, via `openssl` since nothing else here parses X.509; `None` when it can't tell.
pub fn names(cert: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("openssl")
        .args(["x509", "-noout", "-text", "-in"])
        .arg(cert)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| parse_dns_names(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_dns_names(text: &str) -> Vec<String> {
    text.split([',', '\n'])
        .filter_map(|part| part.trim().strip_prefix("DNS:"))
        .map(|n| n.trim().to_ascii_lowercase())
        .collect()
}

/// `Some(true)` when the cert expires within `secs` (or already has).
pub fn expires_within(cert: &Path, secs: u64) -> Option<bool> {
    let status = std::process::Command::new("openssl")
        .args(["x509", "-noout", "-checkend", &secs.to_string(), "-in"])
        .arg(cert)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    // `-checkend` exits 1 for "will expire" and also for unreadable input; the latter fails `names` first.
    Some(!status.success())
}

/// TLS wildcards cover exactly one label, unlike DNS ones.
pub fn covers(names: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    names.iter().any(|n| match n.strip_prefix("*.") {
        Some(parent) => host
            .split_once('.')
            .is_some_and(|(label, rest)| !label.is_empty() && rest == parent),
        None => *n == host,
    })
}

/// The SANs that would cover `hosts`: a wildcard per parent, so new branches need no reissue.
pub fn wanted_names(hosts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = hosts
        .iter()
        .filter_map(|h| h.split_once('.').map(|(_, parent)| format!("*.{parent}")))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_covers_one_label_only() {
        let names = vec!["*.app.dev.example.com".to_string(), "exact.example.com".to_string()];
        assert!(covers(&names, "feat-x.app.dev.example.com"));
        assert!(covers(&names, "FEAT-X.app.dev.example.com"));
        assert!(!covers(&names, "tenant.feat-x.app.dev.example.com"));
        assert!(!covers(&names, "app.dev.example.com"));
        assert!(covers(&names, "exact.example.com"));
        assert!(!covers(&names, "feat-x.api.dev.example.com"));
    }

    #[test]
    fn wanted_names_are_one_wildcard_per_parent() {
        let hosts = vec![
            "feat-x.app.dev.example.com".to_string(),
            "feat-x.api.dev.example.com".to_string(),
            "main.app.dev.example.com".to_string(),
        ];
        assert_eq!(wanted_names(&hosts), vec!["*.api.dev.example.com", "*.app.dev.example.com"]);
    }

    #[test]
    fn parses_san_dns_names_from_openssl_text() {
        let text = "        X509v3 Subject Alternative Name: \n                DNS:*.app.dev.example.com, DNS:*.API.dev.example.com, IP Address:10.0.0.1\n";
        assert_eq!(parse_dns_names(text), vec!["*.app.dev.example.com", "*.api.dev.example.com"]);
    }

    #[test]
    fn refuses_a_domain_that_would_escape_the_cert_dir() {
        for bad in ["../../.ssh", "a/b", "", ".hidden", "x y"] {
            assert!(dir(bad).is_err(), "{bad:?} must be refused");
        }
    }
}
