#[derive(Debug, Clone, Default)]
pub struct ScreenModel {
    lines: Vec<String>,
    current_line: Vec<char>,
    cursor_col: usize,
    max_lines: usize,
}

impl ScreenModel {
    pub fn new(max_lines: usize) -> Self {
        Self {
            max_lines,
            ..Self::default()
        }
    }

    pub fn ingest(&mut self, chunk: &str) {
        let mut chars = chunk.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\x1b' => self.apply_escape_sequence(&mut chars),
                '\r' => self.cursor_col = 0,
                '\n' => self.newline(),
                '\x08' => {
                    if self.cursor_col > 0 {
                        self.cursor_col -= 1;
                    }
                }
                c if !c.is_control() => self.write_char(c),
                _ => {}
            }
        }
    }

    pub fn seed_from_frame(&mut self, frame: &str) {
        self.lines.clear();
        self.current_line.clear();
        self.cursor_col = 0;
        self.ingest(frame);
    }

    pub fn seed(&mut self, frame: &str) {
        self.seed_from_frame(frame);
    }

    pub fn rebase(&mut self, frame: &str) {
        self.seed(frame);
    }

    pub fn render(&self) -> String {
        let mut out = self.lines.clone();
        if !self.current_line.is_empty() {
            let max_visible = self.max_lines.saturating_sub(1);
            if out.len() > max_visible {
                let excess = out.len() - max_visible;
                out.drain(0..excess);
            }
            out.push(self.current_line.iter().collect());
        }
        out.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.current_line.is_empty()
    }

    fn write_char(&mut self, ch: char) {
        if self.cursor_col < self.current_line.len() {
            self.current_line[self.cursor_col] = ch;
        } else if self.cursor_col > self.current_line.len() {
            self.current_line.resize(self.cursor_col, ' ');
            self.current_line.push(ch);
        } else {
            self.current_line.push(ch);
        }
        self.cursor_col += 1;
    }

    fn newline(&mut self) {
        self.lines.push(self.current_line.iter().collect());
        if self.lines.len() > self.max_lines {
            let excess = self.lines.len() - self.max_lines;
            self.lines.drain(0..excess);
        }
        self.current_line.clear();
        self.cursor_col = 0;
    }

    fn apply_escape_sequence(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        match chars.peek() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&c) {
                        self.apply_csi(&params, c);
                        break;
                    }
                    params.push(c);
                }
            }
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    match c {
                        '\x07' => break,
                        '\x1b' if matches!(chars.peek(), Some('\\')) => {
                            chars.next();
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }

    fn apply_csi(&mut self, params: &str, final_byte: char) {
        match final_byte {
            'K' => match params.trim() {
                "" | "0" => self.current_line.truncate(self.cursor_col),
                "2" => {
                    self.current_line.clear();
                    self.cursor_col = 0;
                }
                _ => {}
            },
            'J' if params.trim() == "2" => {
                self.lines.clear();
                self.current_line.clear();
                self.cursor_col = 0;
            }
            _ => {}
        }
    }
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::ScreenModel;
    use crate::classifier::{Classifier, SessionState};

    #[test]
    fn handles_basic_terminal_controls() {
        let mut model = ScreenModel::new(5);
        model.ingest("hello\rworld\nnext\x08!\n\x1b[31mred\x1b[0m");
        assert_eq!(model.render(), "world\nnex!\nred");
    }

    #[test]
    fn carriage_return_overwrites_without_erasing_tail() {
        let mut model = ScreenModel::new(3);
        model.ingest("hello\rhi");
        assert_eq!(model.render(), "hillo");
    }

    #[test]
    fn keeps_recent_visible_lines() {
        let mut model = ScreenModel::new(2);
        model.ingest("one\ntwo\nthree\nfour");
        assert_eq!(model.render(), "three\nfour");
    }

    #[test]
    fn ignores_ansi_and_supports_classification() {
        let mut model = ScreenModel::new(4);
        model.ingest("\x1b[32mClaude Code\x1b[0m\nMain chat input area\nEnter submit message");
        let rendered = model.render();
        let result = Classifier.classify("pane", &rendered);
        assert_eq!(result.state, SessionState::ChatReady);
    }

    #[test]
    fn ignores_osc_title_sequences() {
        let mut model = ScreenModel::new(3);
        model.ingest("\x1b]0;window title\x07Claude\nready");
        assert_eq!(model.render(), "Claude\nready");
    }

    #[test]
    fn clears_stale_tail_on_carriage_return_and_erase_line() {
        let mut model = ScreenModel::new(3);
        model.ingest("Enter submit message\r\x1b[KAllow once");
        assert_eq!(model.render(), "Allow once");
    }

    #[test]
    fn seeds_and_rebases_from_capture_frames() {
        let mut model = ScreenModel::new(4);
        model.seed("base\nframe");
        assert_eq!(model.render(), "base\nframe");
        model.ingest("\nstream");
        assert_eq!(model.render(), "base\nframe\nstream");

        model.rebase("fresh\nview");
        assert_eq!(model.render(), "fresh\nview");
    }
}
