//! Step commands: the `GITHUB_OUTPUT` / `GITHUB_ENV` / `GITHUB_PATH` files
//! and `::name args::message` workflow commands printed to stdout.

/// Parse a `GITHUB_OUTPUT` / `GITHUB_ENV` file: `name=value` lines and
/// `name<<DELIMITER` heredocs.
pub fn parse_kv(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut lines = content.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let eq = line.find('=');
        let heredoc = line.find("<<");
        match (eq, heredoc) {
            (Some(e), h) if h.is_none_or(|h| e < h) => {
                let name = &line[..e];
                if name.is_empty() {
                    return Err(format!("invalid line {line:?}: missing name"));
                }
                out.push((name.to_string(), line[e + 1..].to_string()));
            }
            (_, Some(h)) => {
                let name = &line[..h];
                let delim = &line[h + 2..];
                if name.is_empty() || delim.is_empty() {
                    return Err(format!("invalid line {line:?}"));
                }
                let mut value: Vec<&str> = Vec::new();
                loop {
                    match lines.next() {
                        Some(l) if l == delim => break,
                        Some(l) => value.push(l),
                        None => return Err(format!("{name}: matching delimiter {delim:?} not found")),
                    }
                }
                out.push((name.to_string(), value.join("\n")));
            }
            _ => return Err(format!("invalid line {line:?}: expected name=value or name<<DELIMITER")),
        }
    }
    Ok(out)
}

/// Parse a `GITHUB_PATH` file: one directory per line.
pub fn parse_path(content: &str) -> Vec<String> {
    content.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// A `::command key=value,…::message` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub props: Vec<(String, String)>,
    pub message: String,
}

fn unescape_data(s: &str) -> String {
    s.replace("%0D", "\r").replace("%0A", "\n").replace("%25", "%")
}

fn unescape_prop(s: &str) -> String {
    s.replace("%0D", "\r").replace("%0A", "\n").replace("%3A", ":").replace("%2C", ",").replace("%25", "%")
}

/// Recognize a workflow command line (without its newline).
pub fn parse_command(line: &str) -> Option<Command> {
    let rest = line.trim_start().strip_prefix("::")?;
    let end = rest.find("::")?;
    let (head, message) = (&rest[..end], &rest[end + 2..]);
    let (name, props) = match head.split_once(' ') {
        Some((n, p)) => (n, p),
        None => (head, ""),
    };
    if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return None;
    }
    let props = props
        .split(',')
        .filter(|p| !p.trim().is_empty())
        .filter_map(|p| p.split_once('=').map(|(k, v)| (k.trim().to_string(), unescape_prop(v))))
        .collect();
    Some(Command { name: name.to_string(), props, message: unescape_data(message) })
}

impl Command {
    pub fn prop(&self, key: &str) -> Option<&str> {
        self.props.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(s: &str) -> Vec<(String, String)> {
        parse_kv(s).unwrap()
    }

    #[test]
    fn output_file() {
        assert_eq!(kv("a=1\nb=x=y\n\nc=\n"), vec![("a".into(), "1".into()), ("b".into(), "x=y".into()), ("c".into(), "".into())]);
        assert_eq!(kv("a=1\r\nb=2\r\n"), vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
        assert_eq!(
            kv("json<<EOF\n{\n  \"a\": 1\n}\nEOF\nnext=ok\n"),
            vec![("json".into(), "{\n  \"a\": 1\n}".into()), ("next".into(), "ok".into())]
        );
        assert_eq!(kv("empty<<END\nEND\n"), vec![("empty".into(), "".into())]);
        // `=` before `<<` is a plain value.
        assert_eq!(kv("cmd=a<<b\n"), vec![("cmd".into(), "a<<b".into())]);
        assert_eq!(kv("x<<ghadelim_1\nline=1\nghadelim_1\r\n"), vec![("x".into(), "line=1".into())]);
        assert!(parse_kv("nodelim<<EOF\nvalue\n").is_err());
        assert!(parse_kv("justtext\n").is_err());
        assert!(parse_kv("=value\n").is_err());
    }

    #[test]
    fn path_file() {
        assert_eq!(parse_path("/opt/a/bin\n\n  /opt/b \n"), vec!["/opt/a/bin", "/opt/b"]);
    }

    #[test]
    fn workflow_commands() {
        let c = parse_command("::add-mask::hunter2").unwrap();
        assert_eq!((c.name.as_str(), c.message.as_str()), ("add-mask", "hunter2"));
        let c = parse_command("::set-output name=out%2C1::a%0Ab%25").unwrap();
        assert_eq!(c.prop("name"), Some("out,1"));
        assert_eq!(c.message, "a\nb%");
        let c = parse_command("::error file=a.rs,line=3::boom").unwrap();
        assert_eq!(c.prop("file"), Some("a.rs"));
        assert_eq!(c.prop("line"), Some("3"));
        assert_eq!(parse_command("::endgroup::").unwrap().name, "endgroup");
        assert!(parse_command("plain text").is_none());
        assert!(parse_command(":: not a command").is_none());
        assert!(parse_command("::nope").is_none());
    }
}
