//! A parser for the parts of POSIX shell syntax that decide how a command
//! line runs (design 8.1): lists, pipelines, background jobs, groups,
//! redirections, command and process substitution, and here-documents.
//! Control structures and function definitions are not parsed; a line using
//! them, or one that does not parse, is reported as unclear.

/// A word after quote removal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    pub text: String,
    /// The value depends on expansion (variables, substitutions, globs),
    /// so `text` is not what the shell will use.
    pub dynamic: bool,
    /// Contains a process substitution, which the command sees as a pipe.
    pub process: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// The operator, such as `>`, `>>`, `<`, `2>`, `&>`, `>&`, `<<`.
    pub op: String,
    pub target: Word,
}

impl Redirect {
    /// The file this redirection writes, if any.
    pub fn written_file(&self) -> Option<&str> {
        let writes = self.op.contains('>') && !self.op.ends_with('&');
        let file = self.target.text.as_str();
        (writes && file != "/dev/null" && !file.starts_with('&')).then_some(file)
    }

    /// The file this redirection reads, if any.
    pub fn read_file(&self) -> Option<&str> {
        matches!(
            self.op.trim_start_matches(|c: char| c.is_ascii_digit()),
            "<" | "<>"
        )
        .then_some(self.target.text.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Simple {
    pub assigns: Vec<(String, Word)>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    /// Command substitutions, which run before the command.
    pub substitutions: Vec<Ast>,
    /// Process substitutions, which run alongside it.
    pub processes: Vec<Ast>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sep {
    /// `;`, newline, `&&` or `||`: the next item starts after this one.
    Then,
    /// `&`: this item runs in the background.
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ast {
    Simple(Simple),
    Pipeline(Vec<Ast>),
    List(Vec<(Ast, Sep)>),
    /// `( ... )` or `{ ...; }`, with redirections applying to the whole.
    Group(Box<Ast>, Vec<Redirect>),
}

/// Why a line could not be analyzed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unclear(pub String);

pub fn parse(src: &str) -> Result<Ast, Unclear> {
    let mut p = Parser {
        s: src.chars().collect(),
        i: 0,
        heredocs: Vec::new(),
    };
    let ast = p.list(None)?;
    if p.i < p.s.len() {
        return Err(Unclear(format!("unexpected {:?}", p.s[p.i])));
    }
    Ok(ast)
}

const RESERVED: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "[[", "]]", "coproc",
];

struct Parser {
    s: Vec<char>,
    i: usize,
    /// Here-documents whose bodies start after the next newline:
    /// (delimiter, strip leading tabs, quoted).
    heredocs: Vec<(String, bool, bool)>,
}

/// What ends a list: end of input, `)` or the word `}`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Paren,
    Brace,
}

fn unclear<T>(msg: impl Into<String>) -> Result<T, Unclear> {
    Err(Unclear(msg.into()))
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.s.get(self.i + n).copied()
    }

    fn starts_with(&self, t: &str) -> bool {
        t.chars()
            .enumerate()
            .all(|(k, c)| self.peek_at(k) == Some(c))
    }

    fn skip_blanks(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t') => self.i += 1,
                Some('\\') if self.peek_at(1) == Some('\n') => self.i += 2,
                Some('#') => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// Whether the next word is `}` standing alone.
    fn at_close_brace(&self) -> bool {
        self.peek() == Some('}') && self.peek_at(1).is_none_or(|c| " \t\n;&|)".contains(c))
    }

    fn list(&mut self, end: Option<End>) -> Result<Ast, Unclear> {
        let mut items = Vec::new();
        loop {
            self.skip_separators()?;
            match (self.peek(), end) {
                (None, None) => break,
                (None, Some(_)) => return unclear("unterminated group or substitution"),
                (Some(')'), Some(End::Paren)) => break,
                (Some(')'), _) => return unclear("unexpected )"),
                _ if end == Some(End::Brace) && self.at_close_brace() => break,
                _ => {}
            }
            let item = self.pipeline(end)?;
            self.skip_blanks();
            let sep = if self.peek() == Some('&') && !matches!(self.peek_at(1), Some('&' | '>')) {
                self.i += 1;
                Sep::Background
            } else if self.starts_with("&&") || self.starts_with("||") {
                self.i += 2;
                Sep::Then
            } else if self.peek() == Some(';') {
                if self.peek_at(1) == Some(';') {
                    return unclear(";; outside case");
                }
                self.i += 1;
                Sep::Then
            } else {
                Sep::Then
            };
            items.push((item, sep));
        }
        Ok(match items.len() {
            1 if items[0].1 == Sep::Then => items.pop().expect("one item").0,
            _ => Ast::List(items),
        })
    }

    /// Skips blanks and newlines, reading here-document bodies after each
    /// newline.
    fn skip_separators(&mut self) -> Result<(), Unclear> {
        loop {
            self.skip_blanks();
            if self.peek() == Some('\n') {
                self.i += 1;
                self.read_heredocs()?;
            } else {
                return Ok(());
            }
        }
    }

    fn read_heredocs(&mut self) -> Result<(), Unclear> {
        for (delim, strip_tabs, quoted) in std::mem::take(&mut self.heredocs) {
            loop {
                let start = self.i;
                while self.peek().is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
                let line: String = self.s[start..self.i].iter().collect();
                if self.peek() == Some('\n') {
                    self.i += 1;
                }
                let line = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if line == delim {
                    break;
                }
                if !quoted && (line.contains("$(") || line.contains('`')) {
                    return unclear("command substitution in a here-document");
                }
                if self.peek().is_none() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn pipeline(&mut self, end: Option<End>) -> Result<Ast, Unclear> {
        self.skip_blanks();
        if self.peek() == Some('!') && self.peek_at(1).is_some_and(|c| c == ' ' || c == '\t') {
            self.i += 1;
        }
        let mut stages = vec![self.command(end)?];
        loop {
            self.skip_blanks();
            if self.peek() == Some('|') && self.peek_at(1) != Some('|') {
                self.i += if self.peek_at(1) == Some('&') { 2 } else { 1 };
                self.skip_separators()?;
                stages.push(self.command(end)?);
            } else {
                break;
            }
        }
        Ok(if stages.len() == 1 {
            stages.pop().expect("one stage")
        } else {
            Ast::Pipeline(stages)
        })
    }

    fn command(&mut self, end: Option<End>) -> Result<Ast, Unclear> {
        self.skip_blanks();
        if self.peek() == Some('(') {
            if self.peek_at(1) == Some('(') {
                return unclear("arithmetic command");
            }
            self.i += 1;
            let inner = self.list(Some(End::Paren))?;
            self.i += 1;
            let redirects = self.trailing_redirects()?;
            return Ok(Ast::Group(Box::new(inner), redirects));
        }
        if self.peek() == Some('{') && self.peek_at(1).is_some_and(|c| " \t\n".contains(c)) {
            self.i += 1;
            let inner = self.list(Some(End::Brace))?;
            self.i += 1;
            let redirects = self.trailing_redirects()?;
            return Ok(Ast::Group(Box::new(inner), redirects));
        }
        let mut cmd = Simple::default();
        loop {
            self.skip_blanks();
            match self.peek() {
                None | Some('\n' | ';' | '&' | '|') if !self.starts_with("&>") => break,
                Some(')') if end == Some(End::Paren) => break,
                Some(')') => return unclear("unexpected )"),
                Some('(') => return unclear("function definition or unexpected ("),
                _ => {}
            }
            if end == Some(End::Brace) && cmd.words.is_empty() && self.at_close_brace() {
                break;
            }
            if let Some(r) = self.redirect(&mut cmd)? {
                cmd.redirects.push(r);
                continue;
            }
            let start = self.i;
            let word = self.word(&mut cmd)?;
            if cmd.words.is_empty() {
                // `[[` reads as a glob, so dynamic words are checked too.
                if RESERVED.contains(&word.text.as_str()) {
                    return unclear(format!("shell keyword {}", word.text));
                }
                let raw: String = self.s[start..self.i].iter().collect();
                if let Some((name, value)) = assignment(&word, &raw) {
                    cmd.assigns.push((name, value));
                    continue;
                }
            }
            cmd.words.push(word);
        }
        if cmd.words.is_empty() && cmd.assigns.is_empty() && cmd.redirects.is_empty() {
            return unclear("empty command");
        }
        Ok(Ast::Simple(cmd))
    }

    fn trailing_redirects(&mut self) -> Result<Vec<Redirect>, Unclear> {
        let mut holder = Simple::default();
        loop {
            self.skip_blanks();
            match self.redirect(&mut holder)? {
                Some(r) => holder.redirects.push(r),
                None => break,
            }
        }
        if !holder.substitutions.is_empty() || !holder.processes.is_empty() {
            return unclear("substitution in a group redirection");
        }
        Ok(holder.redirects)
    }

    fn redirect(&mut self, cmd: &mut Simple) -> Result<Option<Redirect>, Unclear> {
        let start = self.i;
        let mut op = String::new();
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            op.push(self.peek().expect("digit"));
            self.i += 1;
        }
        let ops = [
            "&>>", "&>", "<<<", "<<-", "<<", ">>", ">|", ">&", "<&", "<>", ">", "<",
        ];
        let Some(found) = ops.iter().find(|o| self.starts_with(o)) else {
            self.i = start;
            return Ok(None);
        };
        // `<(` and `>(` are process substitutions.
        if op.is_empty() && matches!(*found, "<" | ">") && self.peek_at(1) == Some('(') {
            self.i = start;
            return Ok(None);
        }
        op.push_str(found);
        self.i += found.chars().count();
        self.skip_blanks();
        if self.peek().is_none_or(|c| "\n;&|()<>".contains(c)) {
            return unclear(format!("missing target after {op}"));
        }
        let raw_start = self.i;
        let target = self.word(cmd)?;
        if found.starts_with("<<") && *found != "<<<" {
            let raw: String = self.s[raw_start..self.i].iter().collect();
            let quoted = raw.contains(['\'', '"', '\\']);
            self.heredocs
                .push((target.text.clone(), *found == "<<-", quoted));
        }
        Ok(Some(Redirect { op, target }))
    }

    fn word(&mut self, cmd: &mut Simple) -> Result<Word, Unclear> {
        let mut w = Word::default();
        let at_start = self.i;
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\n' | ';' | '&' | '|' | ')' => break,
                '(' => return unclear("unexpected ("),
                '<' | '>' if self.peek_at(1) == Some('(') => {
                    self.i += 2;
                    let inner = self.list(Some(End::Paren))?;
                    self.i += 1;
                    cmd.processes.push(inner);
                    w.dynamic = true;
                    w.process = true;
                }
                '<' | '>' => break,
                '\\' => {
                    self.i += 1;
                    match self.peek() {
                        Some('\n') => self.i += 1,
                        Some(e) => {
                            w.text.push(e);
                            self.i += 1;
                        }
                        None => {}
                    }
                }
                '\'' => {
                    self.i += 1;
                    loop {
                        match self.peek() {
                            None => return unclear("unterminated single quote"),
                            Some('\'') => {
                                self.i += 1;
                                break;
                            }
                            Some(q) => {
                                w.text.push(q);
                                self.i += 1;
                            }
                        }
                    }
                }
                '"' => {
                    self.i += 1;
                    loop {
                        match self.peek() {
                            None => return unclear("unterminated double quote"),
                            Some('"') => {
                                self.i += 1;
                                break;
                            }
                            Some('\\') => {
                                self.i += 1;
                                match self.peek() {
                                    Some(e @ ('$' | '`' | '"' | '\\')) => {
                                        w.text.push(e);
                                        self.i += 1;
                                    }
                                    Some('\n') => self.i += 1,
                                    _ => w.text.push('\\'),
                                }
                            }
                            Some('$') => self.dollar(&mut w, cmd)?,
                            Some('`') => self.backtick(&mut w, cmd)?,
                            Some(q) => {
                                w.text.push(q);
                                self.i += 1;
                            }
                        }
                    }
                }
                '$' => self.dollar(&mut w, cmd)?,
                '`' => self.backtick(&mut w, cmd)?,
                '*' | '?' | '[' => {
                    w.text.push(c);
                    w.dynamic = true;
                    self.i += 1;
                }
                '~' if self.i == at_start => {
                    self.i += 1;
                    match std::env::var("HOME") {
                        Ok(home)
                            if self
                                .peek()
                                .is_none_or(|n| n == '/' || " \t\n;&|)".contains(n)) =>
                        {
                            w.text.push_str(&home)
                        }
                        _ => {
                            w.text.push('~');
                            w.dynamic = true;
                        }
                    }
                }
                _ => {
                    w.text.push(c);
                    self.i += 1;
                }
            }
        }
        Ok(w)
    }

    fn dollar(&mut self, w: &mut Word, cmd: &mut Simple) -> Result<(), Unclear> {
        w.dynamic = true;
        self.i += 1;
        match self.peek() {
            Some('(') if self.peek_at(1) == Some('(') => {
                self.skip_balanced('(', ')', "unterminated $((")
            }
            Some('(') => {
                self.i += 1;
                let inner = self.list(Some(End::Paren))?;
                self.i += 1;
                cmd.substitutions.push(inner);
                Ok(())
            }
            Some('{') => self.skip_balanced('{', '}', "unterminated ${"),
            Some('\'') => {
                // $'...': ANSI-C quoting, a literal.
                w.dynamic = false;
                self.i += 1;
                loop {
                    match self.peek() {
                        None => return unclear("unterminated $'"),
                        Some('\\') => {
                            self.i += 2;
                        }
                        Some('\'') => {
                            self.i += 1;
                            return Ok(());
                        }
                        Some(c) => {
                            w.text.push(c);
                            self.i += 1;
                        }
                    }
                }
            }
            Some(c) if c.is_alphanumeric() || c == '_' => {
                while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    self.i += 1;
                }
                w.text.push('$');
                Ok(())
            }
            Some('@' | '*' | '#' | '?' | '-' | '$' | '!') => {
                self.i += 1;
                w.text.push('$');
                Ok(())
            }
            _ => {
                w.dynamic = false;
                w.text.push('$');
                Ok(())
            }
        }
    }

    fn skip_balanced(&mut self, open: char, close: char, msg: &str) -> Result<(), Unclear> {
        let mut depth = 0;
        loop {
            match self.peek() {
                None => return unclear(msg),
                Some(c) if c == open => depth += 1,
                Some(c) if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return Ok(());
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
    }

    fn backtick(&mut self, w: &mut Word, cmd: &mut Simple) -> Result<(), Unclear> {
        w.dynamic = true;
        self.i += 1;
        let mut src = String::new();
        loop {
            match self.peek() {
                None => return unclear("unterminated backquote"),
                Some('`') => {
                    self.i += 1;
                    break;
                }
                Some('\\') if self.peek_at(1).is_some_and(|c| "$`\\".contains(c)) => {
                    src.push(self.peek_at(1).expect("escaped char"));
                    self.i += 2;
                }
                Some(c) => {
                    src.push(c);
                    self.i += 1;
                }
            }
        }
        cmd.substitutions.push(parse(&src)?);
        Ok(())
    }
}

/// `NAME=value` written without quotes in the name.
fn assignment(word: &Word, raw: &str) -> Option<(String, Word)> {
    let eq = raw.find('=')?;
    let name = &raw[..eq];
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if !valid {
        return None;
    }
    let value = word.text.get(eq + 1..).unwrap_or_default().to_string();
    Some((
        name.to_string(),
        Word {
            text: value,
            ..word.clone()
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(ast: &Ast) -> Vec<String> {
        match ast {
            Ast::Simple(s) => s.words.iter().map(|w| w.text.clone()).collect(),
            other => panic!("not a simple command: {other:?}"),
        }
    }

    fn simple(ast: &Ast) -> &Simple {
        match ast {
            Ast::Simple(s) => s,
            other => panic!("not a simple command: {other:?}"),
        }
    }

    #[test]
    fn quoting_and_escapes() {
        let ast = parse(r#"echo 'a b' "c $HOME d" e\ f "g\"h""#).unwrap();
        assert_eq!(words(&ast), ["echo", "a b", "c $ d", "e f", "g\"h"]);
        assert!(simple(&ast).words[2].dynamic);
        assert!(!simple(&ast).words[1].dynamic);
    }

    #[test]
    fn lists_pipelines_and_background() {
        let ast = parse("cd app && make -j8 | tee log; ./serve & curl -sf localhost").unwrap();
        let Ast::List(items) = ast else { panic!() };
        assert_eq!(items.len(), 4);
        assert_eq!(words(&items[0].0), ["cd", "app"]);
        assert!(matches!(&items[1].0, Ast::Pipeline(stages) if stages.len() == 2));
        assert_eq!(items[2].1, Sep::Background);
        assert_eq!(words(&items[3].0), ["curl", "-sf", "localhost"]);
    }

    #[test]
    fn redirections() {
        let ast = parse("cargo build 2>&1 > out.log < in.txt &> /dev/null").unwrap();
        let s = simple(&ast);
        assert_eq!(s.words.len(), 2);
        let written: Vec<_> = s
            .redirects
            .iter()
            .filter_map(|r| r.written_file())
            .collect();
        assert_eq!(written, ["out.log"]);
        let read: Vec<_> = s.redirects.iter().filter_map(|r| r.read_file()).collect();
        assert_eq!(read, ["in.txt"]);
    }

    #[test]
    fn substitutions() {
        let ast = parse("echo $(git rev-parse HEAD) `date` <(sort a) $((1+2))").unwrap();
        let s = simple(&ast);
        assert_eq!(s.substitutions.len(), 2);
        assert_eq!(s.processes.len(), 1);
        assert_eq!(words(&s.substitutions[0]), ["git", "rev-parse", "HEAD"]);
    }

    #[test]
    fn assignments_and_groups() {
        let ast =
            parse("RUST_LOG=debug FOO='x y' V=$(id -u) cargo test; (cd a && make) > log").unwrap();
        let Ast::List(items) = ast else { panic!() };
        let s = simple(&items[0].0);
        assert_eq!(s.assigns[1].0, "FOO");
        assert_eq!(s.assigns[1].1.text, "x y");
        assert_eq!(s.assigns[2].0, "V");
        assert_eq!(s.substitutions.len(), 1);
        assert_eq!(words(&items[0].0), ["cargo", "test"]);
        assert!(matches!(&items[1].0, Ast::Group(_, r) if r.len() == 1));
        assert!(parse("{ make; make test; } 2>&1").is_ok());
    }

    #[test]
    fn heredocs() {
        let src = "git commit -F - <<'EOF'\nfix: thing $(not run)\nEOF\necho done";
        let Ast::List(items) = parse(src).unwrap() else {
            panic!()
        };
        assert_eq!(words(&items[1].0), ["echo", "done"]);
        assert!(parse("cat <<EOF\n$(rm -rf x)\nEOF").is_err());
        let msg = "git commit -m \"$(cat <<'EOF'\nmessage\nEOF\n)\"";
        let ast = parse(msg).unwrap();
        assert_eq!(simple(&ast).substitutions.len(), 1);
    }

    #[test]
    fn unclear_lines() {
        for src in [
            "if true; then make; fi",
            "for f in *; do echo $f; done",
            "f() { make; }",
            "echo 'unterminated",
            "echo $(make",
            "(( i++ ))",
            "[[ -f x ]] && make",
        ] {
            assert!(parse(src).is_err(), "{src}");
        }
    }
}
