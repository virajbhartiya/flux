//! A step's output on its way to the log: split into lines so workflow
//! commands (`::add-mask::`, `::group::`, `::set-output`, …) are applied,
//! then masked by the [`JobLog`].

use crate::commands::parse_command;
use crate::exec::Output;
use crate::log::JobLog;

/// Lines longer than this are passed through without command parsing.
const MAX_LINE: usize = 16 * 1024;

pub struct StepOutput {
    log: JobLog,
    line: Vec<u8>,
    /// The buffered bytes continue a line already (partly) written.
    mid_line: bool,
    stop_token: Option<String>,
    /// `::set-output` values.
    pub outputs: Vec<(String, String)>,
}

impl StepOutput {
    pub fn new(log: JobLog) -> Self {
        StepOutput { log, line: Vec::new(), mid_line: false, stop_token: None, outputs: Vec::new() }
    }

    fn handle_line(&mut self, raw: Vec<u8>) {
        if std::mem::take(&mut self.mid_line) {
            self.log.write(&raw);
            return;
        }
        let text = String::from_utf8_lossy(&raw);
        let line = text.trim_end_matches(['\n', '\r']);
        if let Some(token) = &self.stop_token {
            if line.trim() == format!("::{token}::") {
                self.stop_token = None;
            }
            self.log.write(&raw);
            return;
        }
        let Some(cmd) = parse_command(line) else {
            self.log.write(&raw);
            return;
        };
        let location =
            ["file", "line", "col"].iter().filter_map(|k| cmd.prop(k).map(|v| format!("{k}={v}"))).collect::<Vec<_>>().join(",");
        let location = if location.is_empty() { String::new() } else { format!("{location}: ") };
        match cmd.name.as_str() {
            "add-mask" => {
                self.log.add_mask(&cmd.message);
                self.log.line("::add-mask::***");
            }
            "set-output" => match cmd.prop("name") {
                Some(name) => self.outputs.push((name.to_string(), cmd.message.clone())),
                None => self.log.line("##[warning]::set-output without a name"),
            },
            "group" => self.log.line(format!("##[group]{}", cmd.message)),
            "endgroup" => self.log.line("##[endgroup]"),
            "debug" => self.log.line(format!("##[debug]{}", cmd.message)),
            "notice" | "warning" | "error" => self.log.line(format!("##[{}]{location}{}", cmd.name, cmd.message)),
            "stop-commands" => {
                self.stop_token = Some(cmd.message.clone()).filter(|t| !t.is_empty());
                self.log.write(&raw);
            }
            _ => self.log.write(&raw),
        }
    }
}

impl Output for StepOutput {
    fn data(&mut self, chunk: &[u8]) {
        self.line.extend_from_slice(chunk);
        while let Some(pos) = self.line.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.line.drain(..=pos).collect();
            self.handle_line(line);
        }
        if self.line.len() > MAX_LINE {
            let part = std::mem::take(&mut self.line);
            self.log.write(&part);
            self.mid_line = true;
        }
    }

    /// Partial lines (progress output, prompts) are shown without waiting
    /// for their newline, unless they might be a command.
    fn tick(&mut self) {
        if !self.line.is_empty() && (self.mid_line || !self.line.starts_with(b"::")) {
            let part = std::mem::take(&mut self.line);
            self.log.write(&part);
            self.mid_line = true;
        }
    }

    fn finish(&mut self) {
        if !self.line.is_empty() {
            let mut rest = std::mem::take(&mut self.line);
            rest.push(b'\n');
            self.handle_line(rest);
        }
        self.mid_line = false;
        self.log.sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(chunks: &[&str]) -> (String, Vec<(String, String)>) {
        let log = JobLog::new();
        let mut out = StepOutput::new(log.clone());
        for c in chunks {
            out.data(c.as_bytes());
        }
        out.finish();
        (String::from_utf8(log.pending()).unwrap(), out.outputs)
    }

    #[test]
    fn commands_in_output() {
        let (text, outputs) = feed(&[
            "start\n::add-mask::hunter2\nthe password is hunter2\n",
            "::group::Build\nbuilding\n::endgroup::\n::set-output name=x::1\n::warning file=a.rs,line=2::careful\n",
            "no newline at end",
        ]);
        assert_eq!(
            text,
            "start\n::add-mask::***\nthe password is ***\n##[group]Build\nbuilding\n##[endgroup]\n##[warning]file=a.rs,line=2: careful\nno newline at end\n"
        );
        assert_eq!(outputs, vec![("x".to_string(), "1".to_string())]);
    }

    #[test]
    fn masks_values_split_across_reads() {
        let (text, _) = feed(&["::add-mask::abcdef\nx abc", "def y\n"]);
        assert_eq!(text, "::add-mask::***\nx *** y\n");
    }

    #[test]
    fn stop_commands() {
        let (text, outputs) = feed(&["::stop-commands::tok\n::set-output name=a::1\n::tok::\n::set-output name=b::2\n"]);
        assert_eq!(text, "::stop-commands::tok\n::set-output name=a::1\n::tok::\n");
        assert_eq!(outputs, vec![("b".to_string(), "2".to_string())]);
    }

    #[test]
    fn partial_lines_flush_on_tick() {
        let log = JobLog::new();
        let mut out = StepOutput::new(log.clone());
        out.data(b"progress 50%");
        out.tick();
        assert_eq!(log.pending(), b"progress 50%");
        out.data(b" ::add-mask::not-a-command\n");
        out.finish();
        assert_eq!(String::from_utf8(log.pending()).unwrap(), "progress 50% ::add-mask::not-a-command\n");
    }
}
