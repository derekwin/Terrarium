//! Outbound network whitelist. `--net-allow host[:port]` — host may be an
//! IP, a hostname (resolved at sandbox start) or `*`; port is optional.
//! An empty policy denies all outbound connections.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

#[derive(Debug, Clone)]
pub(crate) struct Rule {
    ips: Vec<IpAddr>,
    port: Option<u16>,
}

impl Rule {
    fn matches(&self, ip: IpAddr, port: u16) -> bool {
        self.port.is_none_or(|p| p == port) && (self.ips.is_empty() || self.ips.contains(&ip))
    }
}

/// Parse specs like "api.openai.com:443", "1.1.1.1", "*", "*:80".
/// Hostnames are resolved once at startup (the sandbox itself may not be
/// allowed DNS); resolution failure is a hard error (fail closed).
pub(crate) fn parse(specs: &[String]) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    for s in specs {
        let (host, port) = match s.rsplit_once(':') {
            Some((h, p)) => {
                let port = p
                    .parse::<u16>()
                    .map_err(|_| format!("bad port in --net-allow {s:?}"))?;
                (h.to_string(), Some(port))
            }
            None => (s.clone(), None),
        };
        let ips = if host == "*" {
            Vec::new()
        } else if let Ok(ip) = host.parse::<IpAddr>() {
            vec![ip]
        } else {
            let addrs: Vec<IpAddr> = (host.as_str(), port.unwrap_or(0))
                .to_socket_addrs()
                .map_err(|e| format!("resolve --net-allow {host:?}: {e}"))?
                .map(|a: SocketAddr| a.ip())
                .collect();
            if addrs.is_empty() {
                return Err(format!("--net-allow {host:?} resolved to no addresses"));
            }
            addrs
        };
        rules.push(Rule { ips, port });
    }
    Ok(rules)
}

pub(crate) fn allows(rules: &[Rule], ip: IpAddr, port: u16) -> bool {
    rules.iter().any(|r| r.matches(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_and_port_match() {
        let rules = parse(&["1.1.1.1:443".into()]).unwrap();
        assert!(allows(&rules, "1.1.1.1".parse().unwrap(), 443));
        assert!(!allows(&rules, "1.1.1.1".parse().unwrap(), 80));
        assert!(!allows(&rules, "8.8.8.8".parse().unwrap(), 443));
    }

    #[test]
    fn wildcard_ip_any_port() {
        let rules = parse(&["*".into()]).unwrap();
        assert!(allows(&rules, "10.0.0.1".parse().unwrap(), 22));
        assert!(allows(&rules, "2001:db8::1".parse().unwrap(), 443));
    }

    #[test]
    fn empty_policy_denies_everything() {
        let rules = parse(&[]).unwrap();
        assert!(!allows(&rules, "1.1.1.1".parse().unwrap(), 80));
    }

    #[test]
    fn bad_port_rejected() {
        assert!(parse(&["1.1.1.1:99999".into()]).is_err());
    }
}
