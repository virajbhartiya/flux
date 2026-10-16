//! How `run:` scripts are executed: the `shell:` keyword (or the platform
//! default) becomes a program, arguments and a script file extension.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

impl Os {
    pub fn current() -> Os {
        match std::env::consts::OS {
            "windows" => Os::Windows,
            "macos" => Os::MacOs,
            _ => Os::Linux,
        }
    }

    /// The runner label (`linux`, `macos`, `windows`).
    pub fn label(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::MacOs => "macos",
            Os::Windows => "windows",
        }
    }

    /// `runner.os` / `RUNNER_OS`.
    pub fn runner_os(self) -> &'static str {
        match self {
            Os::Linux => "Linux",
            Os::MacOs => "macOS",
            Os::Windows => "Windows",
        }
    }
}

/// The script interpreter family, which decides how the script is wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Posix,
    PowerShell,
    Cmd,
    Python,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    pub program: String,
    pub args: Vec<String>,
    pub kind: Kind,
}

impl ShellCommand {
    /// File extension for the script file.
    pub fn extension(&self) -> &'static str {
        match self.kind {
            Kind::Posix => ".sh",
            Kind::PowerShell => ".ps1",
            Kind::Cmd => ".cmd",
            Kind::Python => ".py",
            Kind::Custom => "",
        }
    }

    /// The script as written to disk (PowerShell stops on errors and
    /// propagates native exit codes, like GitHub's runner).
    pub fn wrap(&self, script: &str) -> String {
        match self.kind {
            Kind::PowerShell => format!(
                "$ErrorActionPreference = 'stop'\n{script}\nif ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}\n"
            ),
            Kind::Cmd => script.replace('\n', "\r\n"),
            _ => script.to_string(),
        }
    }
}

/// Split a command line into words, honouring single and double quotes.
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err(format!("unterminated quote in shell {s:?}"));
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

fn template(program: &str, args: &[&str], kind: Kind, script: &str) -> ShellCommand {
    ShellCommand { program: program.to_string(), args: args.iter().map(|a| a.replace("{0}", script)).collect(), kind }
}

/// Resolve `shell:` for a script at `script`. `has` reports whether a
/// program is available (on the runner, or in the job container).
pub fn shell_command(shell: Option<&str>, os: Os, has: &dyn Fn(&str) -> bool, script: &str) -> Result<ShellCommand, String> {
    const BASH: [&str; 5] = ["--noprofile", "--norc", "-eo", "pipefail", "{0}"];
    let ps_script = script.replace('\'', "''");
    let pwsh = |program: &str| ShellCommand {
        program: program.to_string(),
        args: vec!["-command".into(), format!(". '{ps_script}'")],
        kind: Kind::PowerShell,
    };
    let shell = shell.map(str::trim).filter(|s| !s.is_empty());
    Ok(match shell {
        None if os == Os::Windows => pwsh(if has("pwsh") { "pwsh" } else { "powershell" }),
        None if has("bash") => template("bash", &BASH, Kind::Posix, script),
        None => template("sh", &["-e", "{0}"], Kind::Posix, script),
        Some("bash") => template("bash", &BASH, Kind::Posix, script),
        Some("sh") => template("sh", &["-e", "{0}"], Kind::Posix, script),
        Some("pwsh") => pwsh("pwsh"),
        Some("powershell") => pwsh("powershell"),
        Some("cmd") => ShellCommand {
            program: "cmd".into(),
            args: ["/D", "/E:ON", "/V:OFF", "/S", "/C"]
                .iter()
                .map(|s| s.to_string())
                .chain([format!("\"CALL \"{script}\"\"")])
                .collect(),
            kind: Kind::Cmd,
        },
        Some("python") => {
            let program = if os != Os::Windows && !has("python") && has("python3") { "python3" } else { "python" };
            template(program, &["{0}"], Kind::Python, script)
        }
        Some(custom) if custom.contains("{0}") => {
            let words = split_words(custom)?;
            let (program, args) = words.split_first().ok_or_else(|| format!("invalid shell {custom:?}"))?;
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            template(program, &args, Kind::Custom, script)
        }
        Some(other) => {
            return Err(format!(
                "unsupported shell {other:?}: use bash, sh, python, pwsh, powershell, cmd, or a command template containing {{0}}"
            ));
        }
    })
}

/// Find a program on `path` (a `PATH`-style list), like `which`.
pub fn find_program(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let exts: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(String::new()).chain(pathext.split(';').map(|e| e.to_ascii_lowercase())).collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(path).find_map(|dir| exts.iter().map(|e| dir.join(format!("{name}{e}"))).find(|p| is_executable(p)))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(shell: Option<&str>, os: Os, avail: &[&str]) -> Result<ShellCommand, String> {
        let avail: Vec<String> = avail.iter().map(|s| s.to_string()).collect();
        shell_command(shell, os, &|p| avail.iter().any(|a| a == p), "/tmp/s/step")
    }

    fn line(c: &ShellCommand) -> String {
        std::iter::once(c.program.clone()).chain(c.args.iter().cloned()).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn defaults() {
        let c = cmd(None, Os::Linux, &["bash", "sh"]).unwrap();
        assert_eq!(line(&c), "bash --noprofile --norc -eo pipefail /tmp/s/step");
        assert_eq!(c.extension(), ".sh");
        assert_eq!(line(&cmd(None, Os::MacOs, &["sh"]).unwrap()), "sh -e /tmp/s/step");
        let c = cmd(None, Os::Windows, &["pwsh"]).unwrap();
        assert_eq!(c.program, "pwsh");
        assert_eq!(c.args, vec!["-command", ". '/tmp/s/step'"]);
        assert_eq!(c.extension(), ".ps1");
        assert_eq!(cmd(None, Os::Windows, &[]).unwrap().program, "powershell");
    }

    #[test]
    fn named_shells() {
        assert_eq!(line(&cmd(Some("bash"), Os::Linux, &[]).unwrap()), "bash --noprofile --norc -eo pipefail /tmp/s/step");
        assert_eq!(line(&cmd(Some("sh"), Os::Linux, &["bash"]).unwrap()), "sh -e /tmp/s/step");
        let py = cmd(Some("python"), Os::Linux, &["python3"]).unwrap();
        assert_eq!((line(&py).as_str(), py.extension()), ("python3 /tmp/s/step", ".py"));
        assert_eq!(cmd(Some("python"), Os::Linux, &["python", "python3"]).unwrap().program, "python");
        assert_eq!(cmd(Some("powershell"), Os::Windows, &["pwsh"]).unwrap().program, "powershell");
        let c = cmd(Some("cmd"), Os::Windows, &[]).unwrap();
        assert_eq!(line(&c), "cmd /D /E:ON /V:OFF /S /C \"CALL \"/tmp/s/step\"\"");
        assert_eq!(c.wrap("a\nb"), "a\r\nb");
        assert!(cmd(Some("fish"), Os::Linux, &[]).is_err());
    }

    #[test]
    fn custom_templates() {
        let c = cmd(Some("perl -w {0}"), Os::Linux, &[]).unwrap();
        assert_eq!((c.program.as_str(), c.args.clone()), ("perl", vec!["-w".to_string(), "/tmp/s/step".into()]));
        assert_eq!(c.extension(), "");
        let c = cmd(Some("bash -c 'source {0}' x"), Os::Linux, &[]).unwrap();
        assert_eq!(c.args, vec!["-c", "source /tmp/s/step", "x"]);
        assert!(cmd(Some("bash 'oops {0}"), Os::Linux, &[]).is_err());
        assert_eq!(split_words("echo 'hello world' \"a b\" c''").unwrap(), vec!["echo", "hello world", "a b", "c"]);
        assert_eq!(split_words("x '' y").unwrap(), vec!["x", "", "y"]);
    }

    #[test]
    fn powershell_wrapping() {
        let c = shell_command(Some("pwsh"), Os::Linux, &|_| true, "/tmp/it's.ps1").unwrap();
        assert_eq!(c.args[1], ". '/tmp/it''s.ps1'");
        let w = c.wrap("Write-Host hi");
        assert!(w.starts_with("$ErrorActionPreference = 'stop'\nWrite-Host hi\n"));
        assert!(w.contains("exit $LASTEXITCODE"));
    }

    #[cfg(unix)]
    #[test]
    fn finds_programs() {
        let path = std::env::var_os("PATH").unwrap();
        assert!(find_program("sh", &path).is_some());
        assert!(find_program("definitely-not-a-program-xyz", &path).is_none());
    }
}
