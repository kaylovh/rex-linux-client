//! A server's own Rex hosts (`hosts.list`) as machines: which to show, and how this
//! computer reaches each one (directly, or through the ssh machine whose server lists it).

use rex_proto::transport::Endpoint;
use serde_json::Value;

use crate::config::ShownHosts;
use crate::machines::MachineSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    pub label: String,
    pub endpoints: Vec<String>,
    pub host_id: Option<String>,
}

/// The hosts in a `hosts.list` result; entries without a label are dropped.
pub fn parse_hosts(v: &Value) -> Vec<HostEntry> {
    v["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| {
            Some(HostEntry {
                label: h["label"].as_str()?.to_string(),
                endpoints: h["endpoints"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect(),
                host_id: h["host_id"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// Whether a server's hosts show when none were chosen: yes for a server on this computer.
pub fn shown_by_default(parent: &MachineSpec) -> bool {
    here(parent)
}

/// Whether the parent's server runs on this computer (its host addresses work from here).
fn here(parent: &MachineSpec) -> bool {
    parent.local || parent.address.starts_with("unix://") || parent.address.starts_with('/')
}

/// How this computer reaches a host that `parent`'s server lists at `endpoint`: as is when
/// that server is here; through the parent's ssh connection otherwise; not at all through
/// anything else (a host of a host, a command, a label).
pub fn host_address(parent: &MachineSpec, endpoint: &str) -> Option<String> {
    if parent.parent.is_some() {
        return None;
    }
    if here(parent) {
        return Some(endpoint.to_string());
    }
    if parent.address.starts_with("ssh://") && !parent.address.contains('#') {
        return Some(format!("{}#{endpoint}", parent.address));
    }
    None
}

/// The hosts of `parent` to show as machines: the ones chosen (`shown`), or by default all
/// for a server here and none for a remote one; minus those that are a machine already
/// (`known_host_ids`) and those that can't be reached.
pub fn host_specs(parent: &MachineSpec, hosts: &[HostEntry], shown: Option<&[String]>, known_host_ids: &mut Vec<String>) -> Vec<MachineSpec> {
    let mut out = Vec::new();
    for h in hosts.iter().filter(|h| match shown {
        Some(labels) => labels.iter().any(|l| l == &h.label),
        None => here(parent),
    }) {
        if h.host_id.as_ref().is_some_and(|id| known_host_ids.contains(id)) {
            continue;
        }
        let Some(address) = h.endpoints.first().and_then(|e| host_address(parent, e)) else { continue };
        let Ok(endpoint) = Endpoint::parse(&address) else { continue };
        // Listed again by another server: shown once.
        if let Some(id) = &h.host_id {
            known_host_ids.push(id.clone());
        }
        out.push(MachineSpec {
            key: format!("h:{}/{}", parent.key, h.label),
            name: h.label.clone(),
            endpoint,
            local: false,
            address,
            parent: Some(parent.key.clone()),
        });
    }
    out
}

/// The labels chosen for `server`, if it was ever chosen.
pub fn shown_for<'a>(shown: &'a [ShownHosts], server: &str) -> Option<&'a [String]> {
    shown.iter().find(|s| s.server == server).map(|s| s.hosts.as_slice())
}

/// Shows or hides one host of `server`. The first change for a server starts from the hosts
/// visible now (`visible_now`), so the default stays as it was for the others.
pub fn toggle_shown(shown: &mut Vec<ShownHosts>, server: &str, label: &str, visible_now: &[String]) {
    let i = match shown.iter().position(|s| s.server == server) {
        Some(i) => i,
        None => {
            shown.push(ShownHosts { server: server.to_string(), hosts: visible_now.to_vec() });
            shown.len() - 1
        }
    };
    let hosts = &mut shown[i].hosts;
    match hosts.iter().position(|h| h == label) {
        Some(p) => {
            hosts.remove(p);
        }
        None => hosts.push(label.to_string()),
    }
}

/// `base` (machines that aren't hosts) with each one's shown hosts right after it, from the
/// hosts each server last listed (kept while it reconnects); a host already shown, or one
/// whose host_id is in `known` (the base machines' own), appears once.
pub fn machines_with_hosts(base: Vec<MachineSpec>, hosts_of: impl Fn(&str) -> Vec<HostEntry>, shown: &[ShownHosts], mut known: Vec<String>) -> Vec<MachineSpec> {
    let mut out = Vec::new();
    for spec in base {
        let hosts = host_specs(&spec, &hosts_of(&spec.key), shown_for(shown, &spec.key), &mut known);
        out.push(spec);
        out.extend(hosts);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(key: &str, address: &str, local: bool) -> MachineSpec {
        MachineSpec {
            key: key.into(),
            name: key.into(),
            endpoint: Endpoint::parse(if address.is_empty() { "/tmp/local.sock" } else { address }).unwrap(),
            local,
            address: address.into(),
            parent: None,
        }
    }

    fn host(label: &str, ep: &str, id: Option<&str>) -> HostEntry {
        HostEntry { label: label.into(), endpoints: vec![ep.into()], host_id: id.map(str::to_string) }
    }

    #[test]
    fn parse_hosts_reads_label_endpoints_id() {
        let v = json!({"hosts": [
            {"id": "host:1", "label": "studio", "endpoints": ["studio.tail.ts.net", "10.0.0.2:7300"], "host_id": "hid-studio"},
            {"label": "bare", "endpoints": null},
            {"endpoints": ["x"]}
        ], "hash": "h"});
        assert_eq!(
            parse_hosts(&v),
            vec![
                HostEntry { label: "studio".into(), endpoints: vec!["studio.tail.ts.net".into(), "10.0.0.2:7300".into()], host_id: Some("hid-studio".into()) },
                HostEntry { label: "bare".into(), endpoints: vec![], host_id: None },
            ]
        );
        assert!(parse_hosts(&json!({})).is_empty());
    }

    #[test]
    fn local_parent_shows_all_by_default() {
        let local = spec("local", "", true);
        let s = host_specs(&local, &[host("a", "a.ts.net", None), host("b", "unix:///tmp/b.sock", None)], None, &mut vec![]);
        assert_eq!(s.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(), ["h:local/a", "h:local/b"]);
        assert_eq!(s[1].address, "unix:///tmp/b.sock");
        assert_eq!(s[0].parent.as_deref(), Some("local"));
        assert!(!s[0].local);
        // Chosen: only those.
        let only = ["b".to_string()];
        assert_eq!(host_specs(&local, &[host("a", "a.ts.net", None), host("b", "unix:///tmp/b.sock", None)], Some(&only), &mut vec![]).len(), 1);
    }

    #[test]
    fn remote_parent_shows_none_until_chosen() {
        let dev = spec("m:dev", "ssh://me@dev", false);
        let hosts = [host("a", "a.ts.net", None)];
        assert!(host_specs(&dev, &hosts, None, &mut vec![]).is_empty());
        let chosen = ["a".to_string()];
        let s = host_specs(&dev, &hosts, Some(&chosen), &mut vec![]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].address, "ssh://me@dev#a.ts.net");
    }

    #[test]
    fn address_through_ssh_parent() {
        let dev = spec("m:dev", "ssh://me@dev", false);
        assert_eq!(host_address(&dev, "unix:///tmp/r.sock").as_deref(), Some("ssh://me@dev#unix:///tmp/r.sock"));
        let hop = spec("m:x", "ssh://me@dev#studio", false);
        assert_eq!(host_address(&hop, "y"), None);
        let cmd = spec("m:c", "cmd:nc host 1", false);
        assert_eq!(host_address(&cmd, "y"), None);
        let local = spec("local", "", true);
        assert_eq!(host_address(&local, "studio.ts.net").as_deref(), Some("studio.ts.net"));
        let pinned_sock = spec("cli", "unix:///run/user/1000/t.sock", false);
        assert_eq!(host_address(&pinned_sock, "unix:///tmp/b.sock").as_deref(), Some("unix:///tmp/b.sock"));
    }

    #[test]
    fn known_host_ids_are_skipped() {
        let local = spec("local", "", true);
        let s = host_specs(&local, &[host("mac", "mac.ts.net", Some("hid-mac")), host("pi", "pi.ts.net", Some("hid-pi"))], None, &mut vec!["hid-mac".into()]);
        assert_eq!(s.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["pi"]);
        // No usable address: skipped too.
        let none = HostEntry { label: "n".into(), endpoints: vec![], host_id: None };
        assert!(host_specs(&local, &[none], None, &mut vec![]).is_empty());
    }

    #[test]
    fn same_host_from_two_servers_once() {
        let local = spec("local", "", true);
        let dev = spec("m:dev", "ssh://me@dev", false);
        let mac = [host("mac", "mac.ts.net", Some("hid-mac"))];
        let chosen = ["mac".to_string()];
        let all = machines_with_hosts(
            vec![local, dev],
            |k| if k == "local" || k == "m:dev" { mac.to_vec() } else { vec![] },
            &[ShownHosts { server: "m:dev".into(), hosts: chosen.to_vec() }],
            vec![],
        );
        assert_eq!(all.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(), ["local", "h:local/mac", "m:dev"]);
    }

    #[test]
    fn toggle_shown_materialises() {
        let mut shown = Vec::new();
        // First change for this server: start from what is visible now.
        toggle_shown(&mut shown, "local", "a", &["a".into(), "b".into()]);
        assert_eq!(shown_for(&shown, "local"), Some(&["b".to_string()][..]));
        toggle_shown(&mut shown, "local", "a", &["b".into()]);
        assert_eq!(shown_for(&shown, "local").map(|s| s.len()), Some(2));
        assert_eq!(shown_for(&shown, "m:dev"), None);
    }
}
