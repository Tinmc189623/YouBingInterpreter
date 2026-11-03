//! 词法扫描：源码 → 带位置的 token 流，缩进按 INDENT/DEDENT 栈产出。
//! 标识符只认 ASCII；括号未闭合期间的换行不产出 Newline；行尾注释仍然结束当前逻辑行。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use crate::error::{CompileError, Pos};

/// 关键字，词法阶段直接定型，解析器不再回查字符串。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Keyword {
    Import, From, Def, Fnt, Return, If, Elif, Else, For, While, In, Class,
    Pass, Break, Continue, And, Or, Not, True, False, None,
}

impl Keyword {
    /// 由标识符文本查关键字，不是关键字返回 None。
    pub fn from_ident(text: &str) -> Option<Keyword> {
        Some(match text {
            "import" => Keyword::Import,
            "from" => Keyword::From,
            "def" => Keyword::Def,
            "fnt" => Keyword::Fnt,
            "return" => Keyword::Return,
            "if" => Keyword::If,
            "elif" => Keyword::Elif,
            "else" => Keyword::Else,
            "for" => Keyword::For,
            "while" => Keyword::While,
            "in" => Keyword::In,
            "class" => Keyword::Class,
            "pass" => Keyword::Pass,
            "break" => Keyword::Break,
            "continue" => Keyword::Continue,
            "and" => Keyword::And,
            "or" => Keyword::Or,
            "not" => Keyword::Not,
            "True" => Keyword::True,
            "False" => Keyword::False,
            "None" => Keyword::None,
            _ => return None,
        })
    }

    /// 关键字原文，错误信息与 AST 展示用。
    pub fn as_str(self) -> &'static str {
        use Keyword::*;
        match self {
            Import => "import", From => "from", Def => "def", Fnt => "fnt", Return => "return",
            If => "if", Elif => "elif", Else => "else", For => "for", While => "while",
            In => "in", Class => "class", Pass => "pass", Break => "break", Continue => "continue",
            And => "and", Or => "or", Not => "not", True => "True", False => "False", None => "None",
        }
    }
}

/// f 串拆出的片段：字面文本或插值表达式（表达式在词法阶段就递归扫成 token）。
#[derive(Clone, Debug)]
pub enum FStrPiece {
    Lit(String),
    Expr(Vec<Token>),
}

/// token 类别。
#[derive(Clone, Debug)]
pub enum Tok {
    Ident(Box<str>),
    Keyword(Keyword),
    Int(i64),
    Str(Box<str>),
    FStr(Vec<FStrPiece>),
    LParen, RParen, LBracket, RBracket,
    Comma, Colon, Semi, Dot, At,
    Assign, Eq, Ne, Lt, Le, Gt, Ge,
    Plus, Minus, Star, Slash, Percent,
    Newline, Indent, Dedent, Eof,
}

impl Tok {
    /// token 的文本形式，报错时回给用户看。
    pub fn show(&self) -> String {
        match self {
            Tok::Ident(s) => s.to_string(),
            Tok::Keyword(k) => k.as_str().to_string(),
            Tok::Int(v) => v.to_string(),
            Tok::Str(s) => format!("\"{s}\""),
            Tok::FStr(_) => "f\"...\"".to_string(),
            Tok::LParen => "(".into(), Tok::RParen => ")".into(),
            Tok::LBracket => "[".into(), Tok::RBracket => "]".into(),
            Tok::Comma => ",".into(), Tok::Colon => ":".into(), Tok::Semi => ";".into(),
            Tok::Dot => ".".into(), Tok::At => "@".into(),
            Tok::Assign => "=".into(), Tok::Eq => "==".into(), Tok::Ne => "!=".into(),
            Tok::Lt => "<".into(), Tok::Le => "<=".into(), Tok::Gt => ">".into(), Tok::Ge => ">=".into(),
            Tok::Plus => "+".into(), Tok::Minus => "-".into(), Tok::Star => "*".into(),
            Tok::Slash => "/".into(), Tok::Percent => "%".into(),
            Tok::Newline => "换行".into(), Tok::Indent => "缩进".into(), Tok::Dedent => "回退".into(),
            Tok::Eof => "文件结尾".into(),
        }
    }
}

/// 带位置的 token。
#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub pos: Pos,
}

const TAB_WIDTH: u32 = 4;

/// 扫描器状态。
struct Lexer {
    src: Vec<char>,
    at: usize,
    line: u32,
    col: u32,
    /// 缩进层级栈，栈底永远是顶层 0。
    stack: Vec<u32>,
    /// 括号深度，非零时物理换行不产生 Newline。
    depth: u32,
    /// 未闭合括号的起始位置，到文件结尾还非空就报错。
    open: Vec<Pos>,
    /// 是否已经遇到过第一条有效语句行。
    started: bool,
}

/// 扫描源码，产出 token 流；词法问题首错即停。
pub fn scan(text: &str) -> Result<Vec<Token>, CompileError> {
    // 工作区源码统一是 UTF-8 BOM，先摘掉 BOM；行尾统一成 \n，CRLF 与裸 CR 都按换行处理。
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut lx = Lexer {
        src: normalized.chars().collect(),
        at: 0,
        line: 1,
        col: 1,
        stack: vec![0],
        depth: 0,
        open: Vec::new(),
        started: false,
    };
    lx.run()
}

impl Lexer {
    fn here(&self) -> Pos {
        Pos::new(self.line, self.col)
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.src.get(self.at + ahead).copied()
    }

    fn done(&self) -> bool {
        self.at >= self.src.len()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek(0)?;
        self.at += 1;
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn push(&self, out: &mut Vec<Token>, tok: Tok, pos: Pos) {
        out.push(Token { tok, pos });
    }

    /// 主循环：一个个物理行往下扫，扫到文件结尾补收尾 token。
    fn run(&mut self) -> Result<Vec<Token>, CompileError> {
        let mut out: Vec<Token> = Vec::new();
        while !self.done() {
            self.scan_line(&mut out)?;
        }
        if let Some(pos) = self.open.first() {
            return Err(CompileError::new("syntax/bracket-unclosed", *pos,
                "这一处的括号一直到文件结尾都没关上"));
        }
        self.finish(&mut out);
        Ok(out)
    }

    /// 收尾：补齐 Newline 与所有没回退的缩进层，保证解析器看到块结尾。
    fn finish(&mut self, out: &mut Vec<Token>) {
        if !matches!(out.last().map(|t| &t.tok), Some(Tok::Newline) | Some(Tok::Dedent) | Some(Tok::Eof)) {
            let pos = self.here();
            self.push(out, Tok::Newline, pos);
        }
        let pos = self.here();
        while self.stack.len() > 1 {
            self.stack.pop();
            self.push(out, Tok::Dedent, pos);
        }
        self.push(out, Tok::Eof, pos);
    }

    /// 扫描一个物理行：先处理行首缩进，再扫本行 token。
    fn scan_line(&mut self, out: &mut Vec<Token>) -> Result<(), CompileError> {
        let mut width = 0u32;
        while matches!(self.peek(0), Some(' ') | Some('\t')) {
            width += if self.peek(0) == Some('\t') { TAB_WIDTH } else { 1 };
            self.bump();
        }

        let empty = matches!(self.peek(0), Some('\n') | None);
        let comment_only = self.depth == 0 && self.peek(0) == Some('/') && self.peek(1) == Some('/');

        if self.depth == 0 && !empty && !comment_only {
            self.apply_indent(width, out)?;
        }

        if (empty || comment_only) && self.depth == 0 {
            if comment_only {
                self.skip_comment(out)?;
            } else {
                self.skip_to_newline();
            }
            return Ok(());
        }

        while !self.done() {
            let pos = self.here();
            match self.peek(0) {
                Some('\n') => {
                    self.bump();
                    if self.depth == 0 && !matches!(out.last().map(|t| &t.tok), Some(Tok::Newline)) {
                        self.push(out, Tok::Newline, pos);
                    }
                    return Ok(());
                }
                Some(' ') | Some('\t') => {
                    self.bump();
                }
                Some('/') if self.peek(1) == Some('/') => {
                    return self.skip_comment(out);
                }
                Some('/') if self.peek(1) == Some('*') => {
                    self.skip_block_comment()?;
                }
                Some('"') | Some('\'') => {
                    let text = self.scan_quoted()?;
                    self.push(out, Tok::Str(text.into()), pos);
                }
                Some('f' | 'F') if matches!(self.peek(1), Some('"') | Some('\'')) => {
                    self.bump();
                    let pieces = self.scan_fstring()?;
                    self.push(out, Tok::FStr(pieces), pos);
                }
                Some(ch) if ch.is_ascii_digit() => {
                    let value = self.scan_number()?;
                    self.push(out, Tok::Int(value), pos);
                }
                Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {
                    let word = self.scan_ident();
                    let tok = match Keyword::from_ident(&word) {
                        Some(kw) => Tok::Keyword(kw),
                        None => Tok::Ident(word.into()),
                    };
                    self.push(out, tok, pos);
                }
                Some(ch) if !ch.is_ascii() => {
                    return Err(CompileError::new("lex/non-ascii-outside-string", pos,
                        format!("标识符只允许 ASCII，这里出现 {ch}；中文只能写在字符串里")));
                }
                Some(_) => {
                    self.scan_punct(out)?;
                }
                None => return Ok(()),
            }
        }
        Ok(())
    }

    /// 行首缩进与 INDENT/DEDENT 栈对齐。
    fn apply_indent(&mut self, width: u32, out: &mut Vec<Token>) -> Result<(), CompileError> {
        let pos = self.here();
        if !self.started {
            self.started = true;
            if width > 0 {
                return Err(CompileError::new("block/indent-misaligned", pos,
                    "文件第一行不能带缩进，顶层语句要顶格写"));
            }
            return Ok(());
        }
        if width > *self.stack.last().unwrap() {
            self.stack.push(width);
            self.push(out, Tok::Indent, pos);
            return Ok(());
        }
        while width < *self.stack.last().unwrap() {
            self.stack.pop();
            self.push(out, Tok::Dedent, pos);
        }
        if width != *self.stack.last().unwrap() {
            return Err(CompileError::new("block/indent-misaligned", pos,
                format!("这一行缩进 {width} 格，和上面任何一层都对不上（现有层级 {:?}）", self.stack)));
        }
        Ok(())
    }

    /// 跳过到行尾；在顶层时这条注释仍算作逻辑行结束。
    fn skip_to_newline(&mut self) {
        while !self.done() && self.peek(0) != Some('\n') {
            self.bump();
        }
        if !self.done() {
            self.bump();
        }
    }

    /// 行注释：吃掉注释本体与换行，并按需在顶层补 Newline 结束语句。
    fn skip_comment(&mut self, out: &mut Vec<Token>) -> Result<(), CompileError> {
        let pos = self.here();
        self.skip_to_newline();
        if self.depth == 0 && !matches!(out.last().map(|t| &t.tok),
            Some(Tok::Newline) | Some(Tok::Indent) | Some(Tok::Dedent)) {
            self.push(out, Tok::Newline, pos);
        }
        Ok(())
    }

    /// 块注释 /* ... */，没等到 */ 就报错。
    fn skip_block_comment(&mut self) -> Result<(), CompileError> {
        let pos = self.here();
        self.bump();
        self.bump();
        loop {
            if self.done() {
                return Err(CompileError::new("lex/unterminated-comment", pos,
                    "/* 一直到文件结尾都没等到 */"));
            }
            if self.peek(0) == Some('*') && self.peek(1) == Some('/') {
                self.bump();
                self.bump();
                return Ok(());
            }
            self.bump();
        }
    }

    /// 读单双引号字符串，反斜杠转义按常见集合处理。
    fn scan_quoted(&mut self) -> Result<String, CompileError> {
        let pos = self.here();
        let quote = self.bump().expect("调用点已确认有引号");
        let mut text = String::new();
        loop {
            match self.peek(0) {
                None | Some('\n') => {
                    return Err(CompileError::new("lex/unterminated-string", pos,
                        format!("{quote} 开头的字符串没在同一行收尾")));
                }
                Some(ch) if ch == quote => {
                    self.bump();
                    return Ok(text);
                }
                Some('\\') => {
                    self.bump();
                    text.push(self.read_escape(pos)?);
                }
                Some(_) => {
                    text.push(self.bump().expect("分支已确认有字符"));
                }
            }
        }
    }

    /// 把反斜杠后的一个字符翻译成转义值。
    fn read_escape(&mut self, quote_pos: Pos) -> Result<char, CompileError> {
        match self.bump() {
            Some('n') => Ok('\n'),
            Some('r') => Ok('\r'),
            Some('t') => Ok('\t'),
            Some('\\') => Ok('\\'),
            Some('"') => Ok('"'),
            Some('\'') => Ok('\''),
            Some('0') => Ok('\0'),
            Some(other) => Err(CompileError::new("lex/unknown-escape", self.here(),
                format!("反斜杠后面跟 {other} 不是认得的转义"))),
            None => Err(CompileError::new("lex/unterminated-string", quote_pos,
                "字符串以反斜杠结尾")),
        }
    }

    /// 读 f 串，拆成字面段与插值段；{{ }} 视为字面大括号。
    fn scan_fstring(&mut self) -> Result<Vec<FStrPiece>, CompileError> {
        let pos = self.here();
        let quote = self.bump().expect("调用点已确认有引号");
        let mut pieces: Vec<FStrPiece> = Vec::new();
        let mut lit = String::new();
        loop {
            match self.peek(0) {
                None | Some('\n') => {
                    return Err(CompileError::new("lex/unterminated-string", pos,
                        "f 串没在同一行收尾"));
                }
                Some(ch) if ch == quote => {
                    self.bump();
                    if !lit.is_empty() {
                        pieces.push(FStrPiece::Lit(std::mem::take(&mut lit)));
                    }
                    return Ok(pieces);
                }
                Some('{') if self.peek(1) == Some('{') => {
                    self.bump();
                    self.bump();
                    lit.push('{');
                }
                Some('}') if self.peek(1) == Some('}') => {
                    self.bump();
                    self.bump();
                    lit.push('}');
                }
                Some('{') => {
                    self.bump();
                    if !lit.is_empty() {
                        pieces.push(FStrPiece::Lit(std::mem::take(&mut lit)));
                    }
                    let inner = self.scan_interpolation(pos)?;
                    pieces.push(FStrPiece::Expr(inner));
                }
                Some('\\') => {
                    self.bump();
                    lit.push(self.read_escape(pos)?);
                }
                Some(_) => {
                    lit.push(self.bump().expect("分支已确认有字符"));
                }
            }
        }
    }

    /// 扫描 { 与 } 之间的表达式。
    fn scan_interpolation(&mut self, start: Pos) -> Result<Vec<Token>, CompileError> {
        let mut inner: Vec<Token> = Vec::new();
        loop {
            while matches!(self.peek(0), Some(' ') | Some('\t')) {
                self.bump();
            }
            match self.peek(0) {
                None | Some('\n') => {
                    return Err(CompileError::new("lex/unterminated-string", start,
                        "f 串的插值花括号没关上"));
                }
                Some('}') => {
                    self.bump();
                    return Ok(inner);
                }
                Some(ch) => {
                    let pos = self.here();
                    match ch {
                        '"' | '\'' => {
                            let text = self.scan_quoted()?;
                            inner.push(Token { tok: Tok::Str(text.into()), pos });
                        }
                        ch if ch.is_ascii_digit() => {
                            let value = self.scan_number()?;
                            inner.push(Token { tok: Tok::Int(value), pos });
                        }
                        ch if ch.is_ascii_alphabetic() || ch == '_' => {
                            let word = self.scan_ident();
                            let tok = match Keyword::from_ident(&word) {
                                Some(kw) => Tok::Keyword(kw),
                                None => Tok::Ident(word.into()),
                            };
                            inner.push(Token { tok, pos });
                        }
                        '{' if self.peek(1) == Some('{') => {
                            return Err(CompileError::new("lex/nested-interpolation", pos,
                                "插值里不再嵌套插值"));
                        }
                        _ => {
                            let before = inner.len();
                            self.scan_punct(&mut inner)?;
                            if inner.len() == before {
                                return Err(CompileError::new("lex/unexpected-char", pos,
                                    format!("插值里出现不该有的字符 {ch}")));
                            }
                        }
                    }
                }
            }
        }
    }

    /// 读 ASCII 标识符。
    fn scan_ident(&mut self) -> String {
        let mut word = String::new();
        while let Some(ch) = self.peek(0) {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                word.push(ch);
                self.bump();
            } else {
                break;
            }
        }
        word
    }

    /// 读整数，溢出报错而不是静默截断。
    fn scan_number(&mut self) -> Result<i64, CompileError> {
        let pos = self.here();
        let mut digits = String::new();
        while matches!(self.peek(0), Some(ch) if ch.is_ascii_digit()) {
            digits.push(self.bump().expect("分支已确认是数字"));
        }
        digits.parse::<i64>().map_err(|_| CompileError::new("lex/int-overflow", pos,
            format!("{digits} 超出 64 位有符号整数的范围")))
    }

    /// 扫描符号，处理 ==、!=、<=、>= 这类双字符组合，并跟踪括号深度。
    fn scan_punct(&mut self, out: &mut Vec<Token>) -> Result<(), CompileError> {
        let pos = self.here();
        let tok = match (self.peek(0), self.peek(1)) {
            (Some('='), Some('=')) => { self.bump(); self.bump(); Tok::Eq }
            (Some('!'), Some('=')) => { self.bump(); self.bump(); Tok::Ne }
            (Some('<'), Some('=')) => { self.bump(); self.bump(); Tok::Le }
            (Some('>'), Some('=')) => { self.bump(); self.bump(); Tok::Ge }
            (Some('!'), _) => {
                return Err(CompileError::new("lex/unexpected-char", pos,
                    "! 单独出现没有意义，只认 != 这个写法"));
            }
            _ => match self.peek(0) {
                Some('(') => { self.bump(); self.depth += 1; self.open.push(pos); Tok::LParen }
                Some('[') => { self.bump(); self.depth += 1; self.open.push(pos); Tok::LBracket }
                Some(')') => {
                    self.open.pop();
                    self.bump();
                    self.depth = self.depth.saturating_sub(1);
                    Tok::RParen
                }
                Some(']') => {
                    self.open.pop();
                    self.bump();
                    self.depth = self.depth.saturating_sub(1);
                    Tok::RBracket
                }
                Some(',') => { self.bump(); Tok::Comma }
                Some(':') => { self.bump(); Tok::Colon }
                Some(';') => { self.bump(); Tok::Semi }
                Some('.') => { self.bump(); Tok::Dot }
                Some('@') => { self.bump(); Tok::At }
                Some('=') => { self.bump(); Tok::Assign }
                Some('<') => { self.bump(); Tok::Lt }
                Some('>') => { self.bump(); Tok::Gt }
                Some('+') => { self.bump(); Tok::Plus }
                Some('-') => { self.bump(); Tok::Minus }
                Some('*') => { self.bump(); Tok::Star }
                Some('%') => { self.bump(); Tok::Percent }
                Some(ch) => {
                    return Err(CompileError::new("lex/unexpected-char", pos,
                        format!("认得的符号里没有 {ch}")));
                }
                None => return Ok(()),
            },
        };
        out.push(Token { tok, pos });
        Ok(())
    }
}
