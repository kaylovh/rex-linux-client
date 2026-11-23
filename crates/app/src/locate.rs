//! Which file system a place lives on: this computer's, an ssh machine's over SFTP, or a
//! machine Rex reaches (read-only). Never a guess: when it isn't clear, Rex or nothing.

use crate::i18n::t;
use crate::machines::MachineSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Local,
    Sftp { address: String },
    /// Read through a terminal of that machine's session.
    Rex { machine: String, session: String, block: String },
    Unavailable(String),
}

/// An ssh machine this computer reaches directly (SFTP works there).
fn direct_ssh(machine: &MachineSpec) -> bool {
    machine.address.starts_with("ssh://") && !machine.address.contains('#')
}

/// Where a terminal block's files are: ours when it runs where we are; the ssh machine's
/// when it runs there; else read through Rex. Unknown hosts are never treated as ours.
pub fn source_for_block(
    block_host: Option<&str>,
    local_host: Option<&str>,
    machine: &MachineSpec,
    machine_host: Option<&str>,
    session: &str,
    block: &str,
) -> Source {
    match (block_host, local_host, machine_host) {
        (Some(b), Some(l), _) if b == l => Source::Local,
        (Some(b), _, Some(m)) if b == m && direct_ssh(machine) => Source::Sftp { address: machine.address.clone() },
        _ => Source::Rex { machine: machine.key.clone(), session: session.into(), block: block.into() },
    }
}

/// Where a machine's files are when picked by hand (no terminal to read through).
pub fn source_for_machine(machine: &MachineSpec, machine_host: Option<&str>, local_host: Option<&str>) -> Source {
    match (machine_host, local_host) {
        (Some(m), Some(l)) if m == l => Source::Local,
        _ if direct_ssh(machine) => Source::Sftp { address: machine.address.clone() },
        _ => Source::Unavailable(t("Open a terminal on this machine to browse its files.", "在这台机器上打开一个终端,才能浏览它的文件。").into()),
    }
}

/// One shell word: single quotes, with ' written as '\''.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn cd_command(path: &str) -> String {
    format!("cd -- {}", shell_quote(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rex_proto::transport::Endpoint;

    fn m(key: &str, address: &str, local: bool) -> MachineSpec {
        MachineSpec {
            key: key.into(),
            name: key.into(),
            endpoint: Endpoint::parse(if address.is_empty() { "/tmp/x.sock" } else { address }).unwrap(),
            local,
            address: address.into(),
            parent: None,
        }
    }

    fn rex(machine: &str) -> Source {
        Source::Rex { machine: machine.into(), session: "s".into(), block: "b".into() }
    }

    #[test]
    fn block_rules() {
        let local = m("local", "", true);
        let dev = m("m:dev", "ssh://me@dev", false);
        let hop = m("h:m:dev/studio", "ssh://me@dev#studio", false);
        // On this computer's files.
        assert_eq!(source_for_block(Some("L"), Some("L"), &local, Some("L"), "s", "b"), Source::Local);
        assert_eq!(source_for_block(Some("L"), Some("L"), &dev, Some("D"), "s", "b"), Source::Local);
        // On the ssh machine itself.
        assert_eq!(source_for_block(Some("D"), Some("L"), &dev, Some("D"), "s", "b"), Source::Sftp { address: "ssh://me@dev".into() });
        // Somewhere else that machine's Rex reaches; a hop; nothing known.
        assert_eq!(source_for_block(Some("X"), Some("L"), &dev, Some("D"), "s", "b"), rex("m:dev"));
        assert_eq!(source_for_block(Some("S"), Some("L"), &hop, Some("S"), "s", "b"), rex("h:m:dev/studio"));
        assert_eq!(source_for_block(None, Some("L"), &local, Some("L"), "s", "b"), rex("local"));
        assert_eq!(source_for_block(Some("L"), None, &local, None, "s", "b"), rex("local"));
    }

    #[test]
    fn machine_rules() {
        assert_eq!(source_for_machine(&m("local", "", true), Some("L"), Some("L")), Source::Local);
        assert_eq!(source_for_machine(&m("m:dev", "ssh://me@dev", false), Some("D"), Some("L")), Source::Sftp { address: "ssh://me@dev".into() });
        // A socket on this computer whose server shares our files.
        assert_eq!(source_for_machine(&m("cli", "unix:///run/t.sock", false), Some("L"), Some("L")), Source::Local);
        assert!(matches!(source_for_machine(&m("h:local/studio", "studio.ts.net", false), Some("S"), Some("L")), Source::Unavailable(_)));
        assert!(matches!(source_for_machine(&m("cli", "unix:///run/t.sock", false), None, Some("L")), Source::Unavailable(_)));
    }

    #[test]
    fn osc7_and_quoting() {
        assert_eq!(shell_quote("it's dir"), r"'it'\''s dir'");
        assert_eq!(cd_command("-x"), "cd -- '-x'");
    }
}
