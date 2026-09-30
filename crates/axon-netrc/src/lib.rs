//! Minimal `.netrc` parser: `machine`, `default`, `login`, `password`,
//! `account` and `macdef` tokens, plus `#` comments.

#[derive(Debug, PartialEq)]
pub struct Credentials {
    pub login: String,
    pub password: Option<String>,
}

#[derive(Default)]
struct Entry<'a> {
    /// `None` for the `default` entry.
    machine: Option<&'a str>,
    login: Option<&'a str>,
    password: Option<&'a str>,
}

/// Returns the credentials for `host`, falling back to the `default` entry.
pub fn lookup(contents: &str, host: &str) -> Option<Credentials> {
    let entries = parse(contents);
    let entry = entries
        .iter()
        .find(|e| e.machine.is_some_and(|m| m.eq_ignore_ascii_case(host)))
        .or_else(|| entries.iter().find(|e| e.machine.is_none()))?;
    Some(Credentials {
        login: entry.login?.to_string(),
        password: entry.password.map(str::to_string),
    })
}

fn parse(contents: &str) -> Vec<Entry<'_>> {
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    let mut tokens = tokenize(contents).into_iter();

    while let Some(token) = tokens.next() {
        match token {
            "machine" | "default" => {
                entries.extend(current.take());
                let machine = if token == "machine" { tokens.next() } else { None };
                current = Some(Entry { machine, ..Default::default() });
            }
            "login" | "password" | "account" => {
                let value = tokens.next();
                if let Some(entry) = current.as_mut() {
                    match token {
                        "login" => entry.login = value,
                        "password" => entry.password = value,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    entries.extend(current);
    entries
}

fn tokenize(contents: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut lines = contents.lines();
    while let Some(line) = lines.next() {
        for word in line.split_whitespace() {
            if word.starts_with('#') {
                break;
            }
            if word == "macdef" {
                // A macro body runs until the next blank line.
                for body in lines.by_ref() {
                    if body.trim().is_empty() {
                        break;
                    }
                }
                break;
            }
            tokens.push(word);
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(login: &str, password: Option<&str>) -> Option<Credentials> {
        Some(Credentials { login: login.into(), password: password.map(Into::into) })
    }

    #[test]
    fn matches_machine() {
        let netrc = "machine a.com login alice password one\nmachine b.com login bob password two";
        assert_eq!(lookup(netrc, "b.com"), creds("bob", Some("two")));
    }

    #[test]
    fn host_match_is_case_insensitive() {
        assert_eq!(lookup("machine Example.COM login u password p", "example.com"), creds("u", Some("p")));
    }

    #[test]
    fn falls_back_to_default() {
        let netrc = "machine a.com login alice password one\ndefault login anon password guest";
        assert_eq!(lookup(netrc, "other.com"), creds("anon", Some("guest")));
    }

    #[test]
    fn prefers_machine_over_earlier_default() {
        let netrc = "default login anon\nmachine a.com login alice";
        assert_eq!(lookup(netrc, "a.com"), creds("alice", None));
    }

    #[test]
    fn no_match() {
        assert_eq!(lookup("machine a.com login alice password one", "b.com"), None);
    }

    #[test]
    fn entry_without_login_is_ignored() {
        assert_eq!(lookup("machine a.com password one", "a.com"), None);
    }

    #[test]
    fn multiline_with_comments_account_and_macdef() {
        let netrc = "\
# comment line
machine a.com
    login alice   # trailing comment
    account acct
    password one

macdef init
machine evil.com login mallory

machine b.com login bob password two
";
        assert_eq!(lookup(netrc, "a.com"), creds("alice", Some("one")));
        assert_eq!(lookup(netrc, "evil.com"), None);
        assert_eq!(lookup(netrc, "b.com"), creds("bob", Some("two")));
    }
}
