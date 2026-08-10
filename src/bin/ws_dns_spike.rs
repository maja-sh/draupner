//! Spike driver for the wildcard DNS responder. Not shipped — proves the
//! responder answers correctly before wiring into ws proper.
//!
//! Usage:
//!   cargo run --bin ws-dns-spike -- [zone] [ip] [port]
//! Defaults: zone=myapp.test ip=127.0.0.1 port=5353
//!
//! Verify:
//!   dig @127.0.0.1 -p 5353 feat-x.api.myapp.test  # → 127.0.0.1
//!   dig @127.0.0.1 -p 5353 myapp.test             # → 127.0.0.1
//!   dig @127.0.0.1 -p 5353 example.com            # → NXDOMAIN

#[path = "../dns.rs"]
mod dns;

use std::net::{Ipv4Addr, SocketAddr};

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let zone = args.next().unwrap_or_else(|| "myapp.test".to_string());
    let ip: Ipv4Addr = args.next().unwrap_or_else(|| "127.0.0.1".to_string()).parse()?;
    let port: u16 = args.next().unwrap_or_else(|| "5353".to_string()).parse()?;

    println!("ws-dns-spike: serving *.{zone} → {ip} on 0.0.0.0:{port}");
    println!("test: dig @127.0.0.1 -p {port} anything.{zone}");
    println!("      dig @127.0.0.1 -p {port} example.com   # → NXDOMAIN");

    dns::serve(
        SocketAddr::new("0.0.0.0".parse()?, port),
        dns::Wildcard { zone, ip, ttl: 60 },
    )
    .await
}
