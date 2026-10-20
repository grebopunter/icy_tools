//! Reader for SyncTERM dialing directories (`syncterm.lst`).
//!
//! A SyncTERM list is an INI file: one `[BBS Name]` section per board with
//! `Key=Value` lines (case-insensitive keys, `;` comments). The key names and
//! connection-type spellings follow SyncTERM's own `bbslist.c` / `conn.c`.
//!
//! Only what IcyTerm can dial is imported. Credentials and every other
//! per-user setting are ignored, so nothing private crosses over.

use std::collections::HashSet;

use icy_net::ConnectionType;

use super::Address;

/// What happened to the entries of one list.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Entries added.
    pub imported: usize,
    /// Entries whose connection type IcyTerm can't dial (TelnetS, Modem, Serial, Shell, ...).
    pub unsupported: usize,
    /// Entries without an address, or sections that aren't boards at all.
    pub missing_address: usize,
    /// Entries with no usable port where the type has no standard one (Raw).
    pub missing_port: usize,
    /// Repeats of a board already in the same list (same host, port and type).
    pub duplicates: usize,
}

impl ImportReport {
    pub fn skipped(&self) -> usize {
        self.unsupported + self.missing_address + self.missing_port + self.duplicates
    }
}

/// True when the text is a SyncTERM list rather than an IcyTerm TOML phone book.
pub fn is_syncterm_list(input: &str) -> bool {
    let mut saw_section = false;
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with("[[") {
            return false; // TOML array of tables, e.g. [[addresses]]
        }
        if line.starts_with('[') && line.ends_with(']') {
            saw_section = true;
            continue;
        }
        if !saw_section {
            // A key before any section: TOML (`version = "1.1.0"`), not SyncTERM.
            return false;
        }
    }
    saw_section
}

/// SyncTERM connection types, as written in the file (`conn_types_enum`), in
/// SyncTERM's order so a numeric value maps too.
const CONN_TYPES: [&str; 14] = [
    "unknown",
    "rlogin",
    "rloginreversed",
    "telnet",
    "raw",
    "ssh",
    "sshna",
    "modem",
    "serial",
    "norts",
    "shell",
    "mbbsghost",
    "telnets",
    "mqtt",
];

/// The type and its standard port, or None when IcyTerm can't dial it.
fn connection_type(value: Option<&str>) -> Option<(ConnectionType, Option<u16>)> {
    // SyncTERM's default for a missing type is SSH.
    let raw = value.map(str::trim).filter(|v| !v.is_empty()).unwrap_or("ssh");
    let key: String = match raw.parse::<usize>() {
        Ok(index) => CONN_TYPES.get(index)?.to_string(),
        // Accept the display names too ("RLogin Reversed", "SSH (no auth)").
        Err(_) => raw
            .to_ascii_lowercase()
            .replace("(no auth)", "na")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect(),
    };
    match key.as_str() {
        "telnet" => Some((ConnectionType::Telnet, Some(23))),
        "ssh" | "sshna" => Some((ConnectionType::SSH, Some(22))),
        "rlogin" => Some((ConnectionType::Rlogin, Some(513))),
        "rloginreversed" => Some((ConnectionType::RloginSwapped, Some(513))),
        "raw" => Some((ConnectionType::Raw, None)),
        _ => None,
    }
}

/// Host part of SyncTERM's Address value: any `user@` or `scheme://` is dropped.
fn clean_host(address: &str) -> String {
    let mut host = address.trim();
    if let Some((_, rest)) = host.split_once("://") {
        host = rest;
    }
    if let Some((_, rest)) = host.rsplit_once('@') {
        host = rest;
    }
    host.trim_end_matches('/').trim().to_string()
}

fn endpoint(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}") // bare IPv6 address
    } else {
        format!("{host}:{port}")
    }
}

struct Section {
    name: String,
    address: Option<String>,
    connection_type: Option<String>,
    port: Option<String>,
    comment: Option<String>,
}

fn sections(input: &str) -> Vec<Section> {
    let mut result: Vec<Section> = Vec::new();
    for line in input.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            result.push(Section {
                name: line[1..line.len() - 1].trim().to_string(),
                address: None,
                connection_type: None,
                port: None,
                comment: None,
            });
            continue;
        }
        let (Some(section), Some((key, value))) = (result.last_mut(), line.split_once('=')) else {
            continue;
        };
        let value = Some(value.trim().to_string());
        match key.trim().to_ascii_lowercase().as_str() {
            "address" => section.address = value,
            "connectiontype" => section.connection_type = value,
            "port" => section.port = value,
            "comment" => section.comment = value,
            _ => {} // credentials, paths, fonts, etc. are never imported
        }
    }
    result
}

/// Parse a SyncTERM list into IcyTerm entries (not yet tagged with a source).
pub fn parse_syncterm_list(input: &str) -> (Vec<Address>, ImportReport) {
    let mut report = ImportReport::default();
    let mut seen = HashSet::new();
    let mut addresses = Vec::new();

    for section in sections(input) {
        let host = section.address.as_deref().map(clean_host).unwrap_or_default();
        if host.is_empty() {
            report.missing_address += 1;
            continue;
        }
        let Some((protocol, default_port)) = connection_type(section.connection_type.as_deref()) else {
            report.unsupported += 1;
            continue;
        };
        let port = section
            .port
            .as_deref()
            .and_then(|p| p.trim().parse::<u16>().ok())
            .filter(|p| *p != 0)
            .or(default_port);
        let Some(port) = port else {
            report.missing_port += 1;
            continue;
        };
        if !seen.insert((host.to_ascii_lowercase(), port, format!("{protocol:?}"))) {
            report.duplicates += 1;
            continue;
        }

        let mut entry = Address::new(if section.name.is_empty() { host.clone() } else { section.name });
        entry.address = endpoint(&host, port);
        entry.protocol = protocol;
        entry.comment = section.comment.unwrap_or_default();
        addresses.push(entry);
        report.imported += 1;
    }
    (addresses, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "; Exported from Synchronix\r\n\r\n[The Pharcyde]\r\n\tConnectionType=telnet\r\n\tAddress=bbs.pharcyde.org\r\n\tPort=23\r\n\tComment=Awesome ANSI stomping ground.\r\n\r\n[War Ensemble BBS]\r\n\tConnectionType=SSH\r\n\tAddress=warensemble.com\r\n";

    #[test]
    fn detects_syncterm_and_not_toml() {
        assert!(is_syncterm_list(SAMPLE));
        assert!(!is_syncterm_list("version = \"1.1.0\"\n\n[[addresses]]\nsystem_name = \"x\"\n"));
        assert!(!is_syncterm_list("[[addresses]]\nsystem_name = \"x\"\n"));
        assert!(!is_syncterm_list(""));
        assert!(!is_syncterm_list("; only a comment\n"));
    }

    #[test]
    fn reads_entries_with_defaults() {
        let (entries, report) = parse_syncterm_list(SAMPLE);
        assert_eq!(
            report,
            ImportReport {
                imported: 2,
                ..Default::default()
            }
        );
        assert_eq!(entries[0].system_name, "The Pharcyde");
        assert_eq!(entries[0].address, "bbs.pharcyde.org:23");
        assert_eq!(entries[0].protocol, ConnectionType::Telnet);
        assert_eq!(entries[0].comment, "Awesome ANSI stomping ground.");
        // missing Port falls back to the type's standard port
        assert_eq!(entries[1].address, "warensemble.com:22");
        assert_eq!(entries[1].protocol, ConnectionType::SSH);
    }

    #[test]
    fn maps_every_type() {
        for (value, expected) in [
            ("Telnet", Some(ConnectionType::Telnet)),
            ("ssh", Some(ConnectionType::SSH)),
            ("SSHNA", Some(ConnectionType::SSH)),
            ("SSH (no auth)", Some(ConnectionType::SSH)),
            ("RLogin", Some(ConnectionType::Rlogin)),
            ("RLoginReversed", Some(ConnectionType::RloginSwapped)),
            ("RLogin Reversed", Some(ConnectionType::RloginSwapped)),
            ("Raw", Some(ConnectionType::Raw)),
            ("3", Some(ConnectionType::Telnet)),
            ("TelnetS", None),
            ("Modem", None),
            ("Serial", None),
            ("NoRTS", None),
            ("Shell", None),
            ("MBBSGhost", None),
            ("MQTT", None),
            ("Unknown", None),
            ("99", None),
        ] {
            assert_eq!(connection_type(Some(value)).map(|t| t.0), expected, "{value}");
        }
        assert_eq!(connection_type(None).map(|t| t.0), Some(ConnectionType::SSH));
    }

    #[test]
    fn skips_and_counts() {
        let input = "[Secure]\nConnectionType=TelnetS\nAddress=a.org\n\
                     [Dialup]\nConnectionType=Modem\nAddress=555-1234\n\
                     [NoAddr]\nConnectionType=telnet\n\
                     [SortProfiles]\nSortOrder=1\n\
                     [RawNoPort]\nConnectionType=raw\nAddress=b.org\n\
                     [Same]\nConnectionType=telnet\nAddress=C.org\nPort=23\n\
                     [Same Again]\nConnectionType=telnet\nAddress=c.org\n\
                     [Other Port]\nConnectionType=telnet\nAddress=c.org\nPort=2323\n\
                     [Other Type]\nConnectionType=ssh\nAddress=c.org\nPort=23\n";
        let (entries, report) = parse_syncterm_list(input);
        assert_eq!(
            report,
            ImportReport {
                imported: 3,
                unsupported: 2,
                missing_address: 2,
                missing_port: 1,
                duplicates: 1
            }
        );
        assert_eq!(report.skipped(), 6);
        let names: Vec<_> = entries.iter().map(|e| e.system_name.as_str()).collect();
        assert_eq!(names, ["Same", "Other Port", "Other Type"]);
    }

    #[test]
    fn never_imports_credentials() {
        let input = "[Board]\nConnectionType=telnet\nAddress=user:pw@telnet://d.org\nUserName=me\nPassword=secret\nSystemPassword=sys\nTLSPSK=key\n";
        let (entries, _) = parse_syncterm_list(input);
        let e = &entries[0];
        assert_eq!(e.address, "d.org:23");
        assert!(e.user_name.is_empty() && e.password.is_empty() && e.auto_login.is_empty());
    }

    /// A 59-board list of edge cases (generated, every entry checked by hand).
    #[test]
    fn synthetic_edge_case_list() {
        let input = include_str!("testdata/syncterm_synthetic.lst");
        let (entries, report) = parse_syncterm_list(input);
        assert_eq!(
            report,
            ImportReport {
                imported: 42,
                unsupported: 11,
                missing_address: 3,
                missing_port: 1,
                duplicates: 2
            }
        );
        assert_eq!(report.imported + report.skipped(), input.lines().filter(|l| l.trim().starts_with('[')).count());
        let find = |name: &str| entries.iter().find(|e| e.system_name == name).unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(find("Mailbox Junction").address, "192.0.2.44:513");
        assert_eq!(find("Dead Letter Office").protocol, ConnectionType::RloginSwapped);
        assert_eq!(find("Numeric Two").protocol, ConnectionType::RloginSwapped);
        assert_eq!(find("Port Too Big").address, "192.0.2.91:23");
        assert_eq!(find("Port With Spaces").address, "192.0.2.93:23");
        assert_eq!(find("IPv6 Six Board").address, "[2001:db8::6]:23");
        assert_eq!(find("User At Host").address, "ssh.example.org:22");
        assert_eq!(find("Telnet Scheme").address, "scheme.example.net:23");
        assert_eq!(find("Accented Comment").comment, "café über board é ü");
        assert_eq!(find("192.0.2.111").comment, "empty section name with valid address");
        assert_eq!(find("Heron Same Port Different Type").protocol, ConnectionType::Telnet);
        for entry in &entries {
            assert!(entry.user_name.is_empty() && entry.password.is_empty(), "{}", entry.system_name);
            let port = entry.address.rsplit(':').next().unwrap().parse::<u16>().unwrap();
            assert_ne!(port, 0, "{}", entry.system_name);
        }
    }

    /// Point SYNCTERM_LIST at a real list file to check it parses (skipped otherwise).
    #[test]
    fn real_list_from_env() {
        let Ok(path) = std::env::var("SYNCTERM_LIST") else {
            return;
        };
        let bytes = std::fs::read(&path).expect("SYNCTERM_LIST unreadable");
        let input = String::from_utf8_lossy(&bytes);
        assert!(is_syncterm_list(&input));
        let (entries, report) = parse_syncterm_list(&input);
        println!("{path}: {report:?}");
        assert_eq!(report.imported, entries.len());
        assert_eq!(report.imported + report.skipped(), input.lines().filter(|l| l.trim().starts_with('[')).count());
        assert!(report.imported > 0);
    }

    #[test]
    fn odd_values() {
        let input = "\u{feff}[  Spaced Name  ]\n  connectiontype = TELNET \n  ADDRESS = e.org \n port = 0 \n\
                     [v6]\nConnectionType=telnet\nAddress=2001:db8::1\nPort=2323\n\
                     [BadPort]\nConnectionType=ssh\nAddress=f.org\nPort=99999\n\
                     [Equals]\nConnectionType=telnet\nAddress=g.org\nComment=a=b=c\n\
                     junk line without equals\n\
                     []\nConnectionType=telnet\nAddress=h.org\n";
        let (entries, report) = parse_syncterm_list(input);
        assert_eq!(report.imported, 5);
        assert_eq!(entries[0].system_name, "Spaced Name");
        assert_eq!(entries[0].address, "e.org:23"); // port 0 -> default
        assert_eq!(entries[1].address, "[2001:db8::1]:2323");
        assert_eq!(entries[2].address, "f.org:22"); // unparsable port -> default
        assert_eq!(entries[3].comment, "a=b=c");
        assert_eq!(entries[4].system_name, "h.org"); // empty name -> host
    }
}
