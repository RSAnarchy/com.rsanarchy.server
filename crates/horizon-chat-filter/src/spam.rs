//! The advert screen: a text that gives a web address, a server address or an advertising phrase.
//! Only addresses and phrases are looked at, never single words, so ordinary chat (and swears) are
//! not stopped here; the word list has its own filter.

/// Top-level domains that make a name into an address (`example.com`, `play.server.net`).
const TLDS: &[&str] = &[
    "com", "net", "org", "gg", "io", "xyz", "cc", "ru", "tk", "pw", "info", "dev", "live", "club",
    "server", "games", "uk", "de", "fr", "app", "eu",
];

/// Advertising phrases, matched as whole words.
const PHRASES: &[&str] = &[
    "join my server",
    "join our server",
    "join the server",
    "server ip",
    "my server ip",
    "best server",
    "free op",
    "free rank",
    "free ranks",
];

/// Whether `text` gives a web address, a server address or an advertising phrase.
pub fn advertises(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower.split_whitespace().any(is_address) {
        return true;
    }
    let words: Vec<String> = lower
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_owned())
        .filter(|w| !w.is_empty())
        .collect();
    let spaced = format!(" {} ", words.join(" "));
    PHRASES
        .iter()
        .any(|phrase| spaced.contains(&format!(" {phrase} ")))
}

/// Whether one whitespace-separated token is an address: a scheme or `www.`, an IPv4 address (with
/// or without a port), or a name with a known top-level domain, or `play.` in front.
fn is_address(token: &str) -> bool {
    if token.contains("://") || token.starts_with("www.") {
        return true;
    }
    let host = token.split(['/', ':', '?']).next().unwrap_or("");
    if is_ipv4(host) {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2
        || labels
            .iter()
            .any(|l| l.is_empty() || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    {
        return false;
    }
    let (first, last) = (labels[0], labels[labels.len() - 1]);
    first == "play" || (first.len() >= 2 && TLDS.contains(&last))
}

fn is_ipv4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}
