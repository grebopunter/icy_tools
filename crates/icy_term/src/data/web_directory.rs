use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use super::{is_syncterm_list, parse_syncterm_list, Address, AddressBook, ImportReport, WebDirectorySource};

const MAX_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;

/// The outcome of loading one directory source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReport {
    pub name: String,
    /// None when the source could not be loaded at all (and had no cache).
    pub report: Option<ImportReport>,
}

fn cache_directory() -> Option<PathBuf> {
    let directory = directories::ProjectDirs::from("com", "GitHub", "icy_term")?.cache_dir().join("web_directories");
    fs::create_dir_all(&directory).ok()?;
    Some(directory)
}

fn cache_file(source: &WebDirectorySource) -> Option<PathBuf> {
    let name: String = source
        .name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') { ch } else { '_' })
        .take(80)
        .collect();
    if name.is_empty() {
        return None;
    }
    Some(cache_directory()?.join(format!("{name}.toml")))
}

fn parse_directory(input: &str, source_name: &str) -> Result<(Vec<Address>, ImportReport), toml::de::Error> {
    let (mut addresses, mut report) = if is_syncterm_list(input) {
        parse_syncterm_list(input)
    } else {
        let book: AddressBook = toml::from_str(input)?;
        (book.addresses, ImportReport::default())
    };
    let before = addresses.len();
    addresses.retain(|address| {
        matches!(
            address.protocol,
            icy_net::ConnectionType::Telnet
                | icy_net::ConnectionType::Raw
                | icy_net::ConnectionType::SSH
                | icy_net::ConnectionType::Websocket
                | icy_net::ConnectionType::SecureWebsocket
                | icy_net::ConnectionType::Rlogin
                | icy_net::ConnectionType::RloginSwapped
        )
    });
    report.unsupported += before - addresses.len();
    for address in &mut addresses {
        address.web_source = Some(source_name.to_string());
        address.user_name.clear();
        address.password.clear();
        address.auto_login.clear();
        address.proxy_command.clear();
        address.proxy = None;
        address.ssh_authentication = Default::default();
        address.ssh_private_key.clear();
        address.ssh_key_passphrase.clear();
        address.modem_id.clear();
        address.icon = None;
        if let Some(mut url) = url::Url::parse(&address.address).ok().filter(|url| url.has_host()) {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            address.address = url.to_string();
        } else if let Ok(mut info) = crate::ConnectionInformation::parse(&address.address) {
            if info.protocol.is_none() {
                info.protocol = Some(address.protocol);
            }
            address.address = info.endpoint();
        }
    }
    // Same host, port and protocol twice in one list: keep the first.
    let mut seen = HashSet::new();
    let before = addresses.len();
    addresses.retain(|address| seen.insert((address.address.to_ascii_lowercase(), format!("{:?}", address.protocol))));
    report.duplicates += before - addresses.len();
    report.imported = addresses.len();
    Ok((addresses, report))
}

/// A source is either an https:// URL or a local file (a path or file:// URL).
fn local_path(location: &str) -> Option<PathBuf> {
    let location = location.trim();
    if let Some(rest) = location.strip_prefix("file://") {
        return url::Url::parse(location)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .or_else(|| Some(PathBuf::from(rest)));
    }
    let path = Path::new(location);
    path.is_absolute().then(|| path.to_path_buf())
}

fn decode(bytes: Vec<u8>) -> String {
    // SyncTERM lists are plain bytes and may hold CP437 text; never reject a list over it.
    String::from_utf8(bytes).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

fn read_local(path: &Path, name: &str) -> Option<String> {
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take(MAX_DIRECTORY_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_DIRECTORY_BYTES {
        log::warn!("Web directory '{name}' exceeds the size limit");
        return None;
    }
    Some(decode(bytes))
}

fn download(source: &WebDirectorySource) -> Option<String> {
    if !source.url.starts_with("https://") {
        log::warn!("Ignoring web directory that is neither HTTPS nor a local file: {}", source.url);
        return None;
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("icy_term/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let response = client.get(&source.url).send().ok()?.error_for_status().ok()?;
    if response.content_length().is_some_and(|length| length > MAX_DIRECTORY_BYTES) {
        log::warn!("Web directory '{}' exceeds the size limit", source.name);
        return None;
    }
    let mut bytes = Vec::new();
    response.take(MAX_DIRECTORY_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_DIRECTORY_BYTES {
        log::warn!("Web directory '{}' exceeds the size limit", source.name);
        return None;
    }
    Some(decode(bytes))
}

pub fn merge_web_directories(book: &mut AddressBook, sources: &[WebDirectorySource]) {
    merge_web_directories_with_reports(book, sources);
}

/// Load every enabled source into the book and say what each one contributed.
pub fn merge_web_directories_with_reports(book: &mut AddressBook, sources: &[WebDirectorySource]) -> Vec<SourceReport> {
    let mut reports = Vec::new();
    for source in sources.iter().filter(|source| source.enabled) {
        // A local file is read directly every time; only downloads are cached.
        if let Some(path) = local_path(&source.url) {
            match read_local(&path, &source.name)
                .as_deref()
                .and_then(|input| parse_directory(input, &source.name).ok())
            {
                Some((addresses, report)) => {
                    log::info!("Loaded '{}' from {}: {report:?}", source.name, path.display());
                    book.addresses.extend(addresses);
                    reports.push(SourceReport {
                        name: source.name.clone(),
                        report: Some(report),
                    });
                }
                None => {
                    log::warn!("Unable to load directory file '{}' ({})", source.name, path.display());
                    reports.push(SourceReport {
                        name: source.name.clone(),
                        report: None,
                    });
                }
            }
            continue;
        }
        let cache = cache_file(source);
        let downloaded = download(source);
        let content = downloaded
            .as_deref()
            .and_then(|input| parse_directory(input, &source.name).ok().map(|parsed| (input.to_string(), parsed)))
            .or_else(|| {
                let input = fs::read_to_string(cache.as_ref()?).ok()?;
                let parsed = parse_directory(&input, &source.name).ok()?;
                Some((input, parsed))
            });

        let Some((input, (addresses, report))) = content else {
            log::warn!("Unable to load web directory '{}'", source.name);
            reports.push(SourceReport {
                name: source.name.clone(),
                report: None,
            });
            continue;
        };
        log::info!("Loaded '{}': {report:?}", source.name);
        reports.push(SourceReport {
            name: source.name.clone(),
            report: Some(report),
        });
        if downloaded.is_some() {
            if let Some(cache) = &cache {
                if let Err(error) = fs::write(cache, input) {
                    log::warn!("Unable to cache web directory '{}': {error}", source.name);
                }
            }
        }
        book.addresses.extend(addresses);
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::parse_directory;

    #[test]
    fn remote_entries_are_read_only_and_strip_credentials() {
        let input = r#"
version = "1.1.0"

[[addresses]]
system_name = "Remote BBS"
address = "bbs.example.org:23"
user_name = "should-not-import"
password = "secret"
auto_login = "login script"
proxy_command = "untrusted command"
ssh_private_key = "/private/key"
icon = "/home/user/.ssh/id_rsa"
"#;

        let (addresses, _) = parse_directory(input, "Community").unwrap();
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].web_source.as_deref(), Some("Community"));
        assert!(addresses[0].user_name.is_empty());
        assert!(addresses[0].password.is_empty());
        assert!(addresses[0].auto_login.is_empty());
        assert!(addresses[0].proxy_command.is_empty());
        assert!(addresses[0].ssh_private_key.is_empty());
        assert!(addresses[0].icon.is_none());
        let input = input.replace("bbs.example.org:23", "telnet://remote:secret@bbs.example.org:23");
        let (addresses, _) = parse_directory(&input, "Community").unwrap();
        let info = crate::ConnectionInformation::parse(&addresses[0].address).unwrap();
        assert!(info.user_name().is_none() && info.password().is_none());
        let input = input.replace("telnet://remote:secret@bbs.example.org:23", "remote:secret@bbs.example.org") + "\nprotocol = \"SSH\"\n";
        let (addresses, _) = parse_directory(&input, "Community").unwrap();
        assert_eq!(addresses[0].address, "bbs.example.org:22");
    }
}
