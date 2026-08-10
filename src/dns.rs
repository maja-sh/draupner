//! Wildcard DNS responder for ws's per-worktree hostnames. Answers A queries
//! for `*.<zone>` with a configured IPv4; NXDOMAIN for anything else.
//!
//! This is the dnsmasq replacement: binds to a non-privileged port so it runs
//! as the invoking user, and the OS resolver hint (macOS `/etc/resolver/*`,
//! systemd-resolved drop-in on Linux) is what routes `<zone>` queries here.

use std::net::{Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use hickory_proto::op::{Header, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{RData, Record, RecordType};
use hickory_server::authority::MessageResponseBuilder;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo, ServerFuture};
use tokio::net::UdpSocket;

/// Serves one wildcard zone; every subdomain of `zone` (and `zone` itself)
/// resolves to `ip`.
pub struct Wildcard {
    pub zone: String,
    pub ip: Ipv4Addr,
    pub ttl: u32,
}

impl Wildcard {
    /// True when `qname` is `zone` itself or a subdomain of it. Case-insensitive
    /// and trailing-dot-tolerant, matching normal DNS name comparison.
    fn matches(&self, qname: &str) -> bool {
        let z = self.zone.trim_end_matches('.').to_ascii_lowercase();
        let q = qname.trim_end_matches('.').to_ascii_lowercase();
        q == z || q.ends_with(&format!(".{z}"))
    }
}

#[async_trait::async_trait]
impl RequestHandler for Wildcard {
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        mut responder: R,
    ) -> ResponseInfo {
        let query = request.query();
        let qname = query.name().to_string();
        let qtype = query.query_type();

        let builder = MessageResponseBuilder::from_message_request(request);
        let mut header = Header::response_from_request(request.header());

        // Not our zone → REFUSED. NXDOMAIN would falsely claim the name
        // doesn't exist anywhere in DNS; REFUSED just says "not my zone."
        if !self.matches(&qname) {
            header.set_response_code(ResponseCode::Refused);
            let response = builder.build(header, [].iter(), &[], &[], &[]);
            return responder.send_response(response).await.unwrap_or(header.into());
        }

        // Our zone + A/ANY → answer. Everything else → NOERROR with empty
        // answer (the name exists, this type doesn't). NXDOMAIN here would
        // trigger IPv6-first resolvers on macOS to retry endlessly on AAAA.
        if matches!(qtype, RecordType::A | RecordType::ANY) {
            header.set_response_code(ResponseCode::NoError);
            let record = Record::from_rdata(query.name().into(), self.ttl, RData::A(A(self.ip)));
            let records = [record];
            let response = builder.build(header, records.iter(), &[], &[], &[]);
            responder.send_response(response).await.unwrap_or(header.into())
        } else {
            header.set_response_code(ResponseCode::NoError);
            let response = builder.build(header, [].iter(), &[], &[], &[]);
            responder.send_response(response).await.unwrap_or(header.into())
        }
    }
}

/// Bind on `addr` (UDP) and serve `wildcard` until cancelled.
pub async fn serve(addr: SocketAddr, wildcard: Wildcard) -> Result<()> {
    let sock = UdpSocket::bind(addr)
        .await
        .with_context(|| format!("bind udp {addr}"))?;
    let mut server = ServerFuture::new(wildcard);
    server.register_socket(sock);
    server
        .block_until_done()
        .await
        .context("dns server exited")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wc(zone: &str) -> Wildcard {
        Wildcard { zone: zone.to_string(), ip: Ipv4Addr::LOCALHOST, ttl: 60 }
    }

    #[test]
    fn matches_subdomain() {
        let w = wc("myapp.test");
        assert!(w.matches("feat-x.api.myapp.test"));
        assert!(w.matches("feat-x.api.myapp.test.")); // trailing dot
        assert!(w.matches("MYAPP.test")); // case
    }

    #[test]
    fn matches_zone_itself() {
        assert!(wc("myapp.test").matches("myapp.test"));
    }

    #[test]
    fn rejects_sibling_and_unrelated() {
        let w = wc("myapp.test");
        assert!(!w.matches("example.com"));
        assert!(!w.matches("notmyapp.test")); // must be under, not adjacent
        assert!(!w.matches("myapp.test.evil.com"));
    }

    /// End-to-end test: spawn a server on an ephemeral port, query it via
    /// hickory-resolver, verify the full request/response loop across the four
    /// meaningful cases (A hit, AAAA-in-zone empty, non-A-type-in-zone empty,
    /// out-of-zone refused).
    #[tokio::test]
    async fn end_to_end_responses() {
        use hickory_proto::rr::RecordType;
        use hickory_resolver::config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts};
        use hickory_resolver::TokioAsyncResolver;
        use std::time::Duration;

        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let mut server = ServerFuture::new(Wildcard {
            zone: "myapp.test".to_string(),
            ip: Ipv4Addr::LOCALHOST,
            ttl: 60,
        });
        server.register_socket(sock);
        let handle = tokio::spawn(async move {
            let _ = server.block_until_done().await;
        });

        let mut cfg = ResolverConfig::new();
        cfg.add_name_server(NameServerConfig {
            socket_addr: addr,
            protocol: Protocol::Udp,
            tls_dns_name: None,
            trust_negative_responses: true,
            bind_addr: None,
        });
        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_secs(2);
        opts.attempts = 1;
        let resolver = TokioAsyncResolver::tokio(cfg, opts);

        // A query in zone → the pinned IP.
        let ips: Vec<_> = resolver
            .lookup("feat-x.api.myapp.test", RecordType::A)
            .await
            .unwrap()
            .iter()
            .filter_map(|r| r.as_a().map(|a| a.0))
            .collect();
        assert_eq!(ips, vec![Ipv4Addr::LOCALHOST], "A wildcard lookup failed");

        // hickory-resolver surfaces NOERROR-with-no-records AND REFUSED as
        // Err(NoRecordsFound{..}) with different `response_code` fields.
        // Peek at that code to distinguish the two shapes we care about.
        use hickory_resolver::error::ResolveErrorKind;

        let assert_response_code = |res: Result<_, hickory_resolver::error::ResolveError>,
                                    expected: ResponseCode,
                                    ctx: &str| {
            let err = res.err().unwrap_or_else(|| panic!("{ctx}: expected error, got Ok"));
            match err.kind() {
                ResolveErrorKind::NoRecordsFound { response_code, .. } => {
                    assert_eq!(*response_code, expected, "{ctx}: wrong response code");
                }
                other => panic!("{ctx}: expected NoRecordsFound, got {other:?}"),
            }
        };

        // AAAA in zone: NOERROR + empty (NOT NXDOMAIN — that would trigger
        // IPv6-first resolvers on macOS to retry endlessly).
        assert_response_code(
            resolver.lookup("feat-x.api.myapp.test", RecordType::AAAA).await,
            ResponseCode::NoError,
            "AAAA in zone",
        );

        // MX in zone: same shape — the name exists, this type doesn't.
        assert_response_code(
            resolver.lookup("myapp.test", RecordType::MX).await,
            ResponseCode::NoError,
            "MX in zone",
        );

        // Out of zone: REFUSED. We're not authoritative for example.com.
        assert_response_code(
            resolver.lookup("example.com", RecordType::A).await,
            ResponseCode::Refused,
            "out of zone",
        );

        handle.abort();
    }
}
