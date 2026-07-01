//! Ad-hoc: verify player discovery receives + parses live announcements.
//!   cargo run -p unterm --example discover_test
use std::time::{Duration, Instant};

fn main() {
    let sock = match unterm::sdb::discovery_socket() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("discovery_socket failed: {e}");
            return;
        }
    };
    eprintln!("bound + joined; listening 10s…");
    let mut buf = [0u8; 2048];
    let mut seen = std::collections::HashSet::new();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(p) = unterm::sdb::parse_player(&buf[..n]) {
                    if seen.insert(p.guid) {
                        eprintln!(
                            "PLAYER from {from}: guid={} ip={} port={} project={:?} label={:?}",
                            p.guid, p.ip, p.port, p.project, p.label
                        );
                    }
                } else {
                    eprintln!("(non-debuggable or unparsable {n}B from {from})");
                }
            }
            Err(_) => {}
        }
    }
    eprintln!("done: {} unique player(s)", seen.len());
}
