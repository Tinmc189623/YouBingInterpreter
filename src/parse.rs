//! 递归下降解析器：token 流 → AST，缩进块靠词法产出的 INDENT/DEDENT 定界。
//! 块必须换行缩进，不接受一行体；诊断首错即停，规则码与 yb-cj 插件保持一致。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use crate::ast::{BOp, Expr, ImportTarget, Module, SrcRef, Stmt, UOp};
use crate::error::{CompileError, Pos};
use crate::lex::{FStrPiece, Keyword, Tok, Token, scan};

/// 解析一个 .yb 文件。
pub fn parse_file(path: &str, text: &str) -> Result<Module, CompileError> {
    let tokens = scan(text)?;
    let mut parser = Parser { tokens, at: 0 };
    let stmts = parser.parse_program()?;
    Ok(Module { path: path.to_string(), stmts })
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn cur(&self) -> &Token {
        self.tokens.get(self.at).unwrap_or_else(|| self.tokens.last().expect("token 流不会为空"))
    }

    fn here(&self) -> Pos {
        self.cur().pos
    }

    fn bump(&mut self) -> Token {
        let token = self.cur().clone();
        if self.at < self.tokens.len() {
            self.at += 1;
        }
        token
    }

    fn at_eof(&self) -> bool {
        matches!(self.cur().tok, Tok::Eof)
    }

    /// 当前 token 是否匹配给定类别。
    fn is(&self, pred: fn(&Tok) -> bool) -> bool {
        pred(&self.cur().tok)
    }

    fn is_newlineish(&self) -> bool {
        matches!(self.cur().tok, Tok::Newline | Tok::Semi)
    }

    /// 吃掉语句之间的换行与分号。
    fn skip_terminators(&mut self) {
        while self.is_newlineish() {
            self.bump();
        }
    }

    /// 要求一个语句终止符；同一行还接着写别的东西就报错。
    fn expect_terminator(&mut self, what: &str) -> Result<(), CompileError> {
        let pos = self.here();
        if self.is_newlineish() || self.at_eof() {
            self.bump();
            return Ok(());
        }
        Err(CompileError::new("syntax/statement-not-terminated", pos,
            format!("{what} 之后要么换行要么加分号，这里实际是 {:?}", self.cur().tok.show())))
    }

    /// 期望并消费一个特定 token。
    fn expect(&mut self, pred: fn(&Tok) -> bool, want: &str, code: &'static str) -> Result<Token, CompileError> {
        if !pred(&self.cur().tok) {
            let token = self.cur().clone();
            return Err(CompileError::new(code, token.pos,
                format!("这里要的是 {want}，实际是 {:?}", token.tok.show())));
        }
        Ok(self.bump())
    }

    /// 整个文件：顶层语句一直读到 Eof。
    fn parse_program(&mut self) -> Result<Vec<Stmt>, CompileError> {
        let mut stmts = Vec::new();
        loop {
            self.skip_terminators();
            if self.at_eof() {
                return Ok(stmts);
            }
            if self.is(|t| matches!(t, Tok::Dedent)) {
                return Err(CompileError::new("block/indent-misaligned", self.here(),
                    "顶层出现了多余的缩进回退"));
            }
            stmts.push(self.parse_stmt()?);
        }
    }

    /// 块体：调用点已消费块头的冒号与换行，这里吃 Indent、语句、Dedent。
    fn parse_block(&mut self) -> Result<Vec<Stmt>, CompileError> {
        let pos = self.here();
        self.expect(|t| matches!(t, Tok::Indent), "缩进进来的块体", "block/empty-body")?;
        let mut stmts = Vec::new();
        loop {
            self.skip_terminators();
            if self.at_eof() {
                return Ok(stmts);
            }
            if self.is(|t| matches!(t, Tok::Dedent)) {
                self.bump();
                if stmts.is_empty() {
                    return Err(CompileError::new("block/empty-body", pos,
                        "缩进块里一条语句都没有"));
                }
                return Ok(stmts);
            }
            stmts.push(self.parse_stmt()?);
        }
    }

    /// 块头通用收尾：冒号 + 换行，然后进块。
    fn parse_header_block(&mut self, header: &str) -> Result<Vec<Stmt>, CompileError> {
        self.expect(|t| matches!(t, Tok::Colon), "块头行末的冒号", "block/header-missing-colon")?;
        let pos = self.here();
        if !self.is_newlineish() {
            return Err(CompileError::new("block/header-needs-newline", pos,
                format!("{header} 的冒号后面要换行，不接受写成一行")));
        }
        self.skip_terminators();
        self.parse_block()
    }

    /// 一条语句。
    fn parse_stmt(&mut self) -> Result<Stmt, CompileError> {
        let pos = self.here();
        match self.cur().tok.clone() {
            Tok::At => self.parse_decorated(),
            Tok::Keyword(kw) => match kw {
                Keyword::If => self.parse_if(pos),
                Keyword::While => self.parse_while(pos),
                Keyword::For => self.parse_for(pos),
                Keyword::Def | Keyword::Fnt => self.parse_funcdef(pos, Vec::new()),
                Keyword::Return => {
                    self.bump();
                    let value = if self.is_newlineish() || self.at_eof() {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    self.expect_terminator("return")?;
                    Ok(Stmt::Return { value, pos })
                }
                Keyword::Import => self.parse_import(pos, None),
                Keyword::From => self.parse_from(pos),
                Keyword::Class => Err(CompileError::new("unsupported/class", pos,
                    "class 关键字保留，但这版解释器还没有实现类")),
                Keyword::Pass => { self.bump(); self.expect_terminator("pass")?; Ok(Stmt::Pass { pos }) }
                Keyword::Break => { self.bump(); self.expect_terminator("break")?; Ok(Stmt::Break { pos }) }
                Keyword::Continue => { self.bump(); self.expect_terminator("continue")?; Ok(Stmt::Continue { pos }) }
                Keyword::In => Err(CompileError::new("syntax/unexpected-keyword", pos,
                    "in 只能出现在 for 语句里")),
                other => Err(CompileError::new("syntax/unexpected-token", pos,
                    format!("语句不能以 {} 开头", other.as_str()))),
            },
            Tok::Ident(_) => self.parse_assignment_or_expr(pos),
            _ => {
                let expr = self.parse_expr()?;
                self.expect_terminator("表达式语句")?;
                Ok(Stmt::Expr { expr, pos })
            }
        }
    }

    /// `目标 = 值` 记成赋值，其余按表达式语句处理。目标只能是名字、点号链或下标。
    fn parse_assignment_or_expr(&mut self, pos: Pos) -> Result<Stmt, CompileError> {
        let expr = self.parse_expr()?;
        if self.is(|t| matches!(t, Tok::Assign)) {
            self.bump();
            let value = self.parse_expr()?;
            self.expect_terminator("赋值")?;
            return match expr {
                Expr::Name { .. } | Expr::Path { .. } | Expr::Index { .. } =>
                    Ok(Stmt::Assign { target: expr, value, pos }),
                other => Err(CompileError::new("syntax/invalid-assignment-target", other.pos(),
                    "等号左边只能是名字、点号链或下标")),
            };
        }
        self.expect_terminator("表达式语句")?;
        Ok(Stmt::Expr { expr, pos })
    }

    /// 装饰器序列：连续的 @目标 行，下面必须跟 def/fnt。
    fn parse_decorated(&mut self) -> Result<Stmt, CompileError> {
        let mut decorators = Vec::new();
        loop {
            self.expect(|t| matches!(t, Tok::At), "@", "syntax/decorator-needs-target")?;
            let target = self.parse_expr()?;
            self.expect_terminator("装饰器")?;
            decorators.push(target);
            self.skip_terminators();
            if !self.is(|t| matches!(t, Tok::At)) {
                break;
            }
        }
        let head = self.here();
        match self.cur().tok.clone() {
            Tok::Keyword(Keyword::Def | Keyword::Fnt) => self.parse_funcdef(head, decorators),
            other => Err(CompileError::new("syntax/decorator-needs-funcdef", head,
                format!("装饰器下面要跟 def 或 fnt，这里实际是 {:?}", other.show()))),
        }
    }

    /// def/fnt：名字、形参、冒号、块体。
    fn parse_funcdef(&mut self, pos: Pos, decorators: Vec<Expr>) -> Result<Stmt, CompileError> {
        self.bump();
        let name_token = self.expect(is_ident, "函数名", "syntax/def-needs-name")?;
        let name = match name_token.tok {
            Tok::Ident(name) => name,
            _ => unreachable!("expect 已确认是标识符"),
        };
        self.expect(|t| matches!(t, Tok::LParen), "形参左括号", "syntax/def-needs-paren")?;
        let mut params: Vec<Box<str>> = Vec::new();
        loop {
            if self.is(|t| matches!(t, Tok::RParen)) {
                self.bump();
                break;
            }
            let token = self.expect(is_ident, "形参名", "syntax/def-needs-param")?;
            match token.tok {
                Tok::Ident(param) => params.push(param),
                _ => unreachable!("expect 已确认是标识符"),
            }
            if self.is(|t| matches!(t, Tok::Comma)) {
                self.bump();
                continue;
            }
            self.expect(|t| matches!(t, Tok::RParen), "形参右括号", "syntax/def-needs-paren")?;
            break;
        }
        let body = self.parse_header_block(&name)?;
        Ok(Stmt::FuncDef { name, params, body, decorators, pos })
    }

    /// import "x" 与 from 来源 import "y"。
    fn parse_import(&mut self, pos: Pos, source: Option<SrcRef>) -> Result<Stmt, CompileError> {
        self.expect(|t| matches!(t, Tok::Keyword(Keyword::Import)), "import", "syntax/from-needs-import")?;
        let target = self.parse_import_target()?;
        self.expect_terminator("import")?;
        Ok(Stmt::Import { source, target, pos })
    }

    fn parse_import_target(&mut self) -> Result<ImportTarget, CompileError> {
        let token = self.cur().clone();
        match token.tok {
            Tok::Str(text) => {
                self.bump();
                Ok(ImportTarget::Quoted(text))
            }
            Tok::Ident(name) => {
                self.bump();
                Ok(ImportTarget::Bare(name))
            }
            _ => Err(CompileError::new("syntax/import-needs-target", token.pos,
                format!("import 后面要带引号包住的模块名或标识符，实际是 {:?}", token.tok.show()))),
        }
    }

    /// from app / from 'app' / from . / from .. 后面接 import。
    fn parse_from(&mut self, pos: Pos) -> Result<Stmt, CompileError> {
        self.bump();
        let source = self.parse_src_ref()?;
        self.parse_import(pos, Some(source))
    }

    /// 解析 from 后面的来源。
    fn parse_src_ref(&mut self) -> Result<SrcRef, CompileError> {
        if self.is(|t| matches!(t, Tok::Dot)) {
            let mut level = 0u32;
            while self.is(|t| matches!(t, Tok::Dot)) {
                self.bump();
                level += 1;
            }
            return Ok(SrcRef::Relative(level));
        }
        let token = self.cur().clone();
        match token.tok {
            Tok::Str(text) => { self.bump(); Ok(SrcRef::Module(text)) }
            Tok::Ident(name) => { self.bump(); Ok(SrcRef::Module(name)) }
            _ => Err(CompileError::new("syntax/from-needs-source", token.pos,
                format!("from 后面要写模块名、引号名或点号，实际是 {:?}", token.tok.show()))),
        }
    }

    /// if / elif / else 链。
    fn parse_if(&mut self, pos: Pos) -> Result<Stmt, CompileError> {
        let mut arms: Vec<(Expr, Vec<Stmt>)> = Vec::new();
        let mut orelse: Option<Vec<Stmt>> = None;
        loop {
            let header = match self.cur().tok {
                Tok::Keyword(Keyword::If) => "if",
                Tok::Keyword(Keyword::Elif) => "elif",
                Tok::Keyword(Keyword::Else) => "else",
                _ => return Err(CompileError::new("syntax/unexpected-token", self.here(),
                    "if 链里出现了不该有的关键字")),
            };
            self.bump();
            if header == "else" {
                orelse = Some(self.parse_header_block(header)?);
                break;
            }
            let cond = self.parse_expr()?;
            let body = self.parse_header_block(header)?;
            arms.push((cond, body));
            self.skip_terminators();
            if !matches!(self.cur().tok, Tok::Keyword(Keyword::Elif) | Tok::Keyword(Keyword::Else)) {
                break;
            }
        }
        Ok(Stmt::If { arms, orelse, pos })
    }

    /// while 循环。
    fn parse_while(&mut self, pos: Pos) -> Result<Stmt, CompileError> {
        self.bump();
        let cond = self.parse_expr()?;
        let body = self.parse_header_block("while")?;
        Ok(Stmt::While { cond, body, pos })
    }

    /// for 变量 in 可迭代: 块
    fn parse_for(&mut self, pos: Pos) -> Result<Stmt, CompileError> {
        self.bump();
        let token = self.expect(is_ident, "for 后面的循环变量", "syntax/for-needs-var")?;
        let var = match token.tok {
            Tok::Ident(name) => name,
            _ => unreachable!("expect 已确认是标识符"),
        };
        self.expect(|t| matches!(t, Tok::Keyword(Keyword::In)), "in", "syntax/for-needs-in")?;
        let iter = self.parse_expr()?;
        let body = self.parse_header_block("for")?;
        Ok(Stmt::For { var, iter, body, pos })
    }

    /// 表达式入口。
    fn parse_expr(&mut self) -> Result<Expr, CompileError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, CompileError> {
        let mut left = self.parse_and()?;
        while self.is(|t| matches!(t, Tok::Keyword(Keyword::Or))) {
            let pos = self.here();
            self.bump();
            let right = self.parse_and()?;
            left = Expr::Binary { op: BOp::Or, left: left.into(), right: right.into(), pos };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, CompileError> {
        let mut left = self.parse_not()?;
        while self.is(|t| matches!(t, Tok::Keyword(Keyword::And))) {
            let pos = self.here();
            self.bump();
            let right = self.parse_not()?;
            left = Expr::Binary { op: BOp::And, left: left.into(), right: right.into(), pos };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, CompileError> {
        if self.is(|t| matches!(t, Tok::Keyword(Keyword::Not))) {
            let pos = self.here();
            self.bump();
            let operand = self.parse_not()?;
            return Ok(Expr::Unary { op: UOp::Not, operand: operand.into(), pos });
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Expr, CompileError> {
        let mut left = self.parse_add()?;
        loop {
            let op = match self.cur().tok {
                Tok::Eq => BOp::Eq, Tok::Ne => BOp::Ne,
                Tok::Lt => BOp::Lt, Tok::Le => BOp::Le,
                Tok::Gt => BOp::Gt, Tok::Ge => BOp::Ge,
                _ => break,
            };
            let pos = self.here();
            self.bump();
            let right = self.parse_add()?;
            left = Expr::Binary { op, left: left.into(), right: right.into(), pos };
        }
        Ok(left)
    }

    fn parse_add(&mut self) -> Result<Expr, CompileError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.cur().tok {
                Tok::Plus => BOp::Add,
                Tok::Minus => BOp::Sub,
                _ => break,
            };
            let pos = self.here();
            self.bump();
            let right = self.parse_mul()?;
            left = Expr::Binary { op, left: left.into(), right: right.into(), pos };
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> Result<Expr, CompileError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.cur().tok {
                Tok::Star => BOp::Mul,
                Tok::Slash => BOp::Div,
                Tok::Percent => BOp::Mod,
                _ => break,
            };
            let pos = self.here();
            self.bump();
            let right = self.parse_unary()?;
            left = Expr::Binary { op, left: left.into(), right: right.into(), pos };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, CompileError> {
        if self.is(|t| matches!(t, Tok::Minus)) {
            let pos = self.here();
            self.bump();
            let operand = self.parse_unary()?;
            return Ok(Expr::Unary { op: UOp::Neg, operand: operand.into(), pos });
        }
        self.parse_postfix()
    }

    /// 后缀：调用、索引、点号成员链。
    fn parse_postfix(&mut self) -> Result<Expr, CompileError> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.cur().tok {
                Tok::LParen => { expr = self.parse_call_tail(expr)?; }
                Tok::LBracket => {
                    let pos = self.here();
                    self.bump();
                    let index = self.parse_expr()?;
                    self.expect(|t| matches!(t, Tok::RBracket), "索引右括号", "syntax/bracket-mismatch")?;
                    expr = Expr::Index { object: expr.into(), index: index.into(), pos };
                }
                Tok::Dot => {
                    let pos = self.here();
                    self.bump();
                    let token = self.expect(is_ident, "点号后面的成员名", "syntax/attr-needs-name")?;
                    let seg = match token.tok {
                        Tok::Ident(name) => name,
                        _ => unreachable!("expect 已确认是标识符"),
                    };
                    expr = match expr {
                        Expr::Path { root, mut segs, pos: p } => { segs.push(seg); Expr::Path { root, segs, pos: p } }
                        Expr::Name { name, .. } => Expr::Path { root: name, segs: vec![seg], pos },
                        other => return Err(CompileError::new("syntax/attr-needs-name", other.pos(),
                            format!("点号只能接在名字后面，{} 前面是个更复杂的表达式", seg))),
                    };
                }
                _ => return Ok(expr),
            }
        }
    }

    /// 实参表：位置实参与 name=value kwargs 混写。
    fn parse_call_tail(&mut self, callee: Expr) -> Result<Expr, CompileError> {
        let pos = callee.pos();
        self.bump();
        let mut args: Vec<Expr> = Vec::new();
        let mut kwargs: Vec<(Box<str>, Expr)> = Vec::new();
        loop {
            if self.is(|t| matches!(t, Tok::RParen)) {
                self.bump();
                break;
            }
            if let (Tok::Ident(name), Some(next)) = (self.cur().tok.clone(), self.tokens.get(self.at + 1)) {
                if matches!(next.tok, Tok::Assign) {
                    self.bump();
                    self.bump();
                    let value = self.parse_expr()?;
                    kwargs.push((name, value));
                    if self.is(|t| matches!(t, Tok::Comma)) { self.bump(); continue; }
                    self.expect(|t| matches!(t, Tok::RParen), "实参右括号", "syntax/bracket-mismatch")?;
                    break;
                }
            }
            let arg = self.parse_expr()?;
            args.push(arg);
            if self.is(|t| matches!(t, Tok::Comma)) { self.bump(); continue; }
            self.expect(|t| matches!(t, Tok::RParen), "实参右括号", "syntax/bracket-mismatch")?;
            break;
        }
        Ok(Expr::Call { callee: callee.into(), args, kwargs, pos })
    }

    fn parse_primary(&mut self) -> Result<Expr, CompileError> {
        let token = self.cur().clone();
        match token.tok {
            Tok::Int(value) => { self.bump(); Ok(Expr::Int { value, pos: token.pos }) }
            Tok::Str(text) => { self.bump(); Ok(Expr::Str { text, pos: token.pos }) }
            Tok::FStr(pieces) => { self.bump(); Ok(Expr::FStr { pieces, pos: token.pos }) }
            Tok::Ident(name) => { self.bump(); Ok(Expr::Name { name, pos: token.pos }) }
            Tok::Keyword(Keyword::True) => { self.bump(); Ok(Expr::Bool { value: true, pos: token.pos }) }
            Tok::Keyword(Keyword::False) => { self.bump(); Ok(Expr::Bool { value: false, pos: token.pos }) }
            Tok::Keyword(Keyword::None) => { self.bump(); Ok(Expr::NoneLit { pos: token.pos }) }
            Tok::LParen => {
                self.bump();
                let inner = self.parse_expr()?;
                self.expect(|t| matches!(t, Tok::RParen), "分组右括号", "syntax/bracket-mismatch")?;
                Ok(inner)
            }
            Tok::LBracket => {
                self.bump();
                let mut items: Vec<Expr> = Vec::new();
                loop {
                    if self.is(|t| matches!(t, Tok::RBracket)) { self.bump(); break; }
                    items.push(self.parse_expr()?);
                    if self.is(|t| matches!(t, Tok::Comma)) { self.bump(); continue; }
                    self.expect(|t| matches!(t, Tok::RBracket), "列表右方括号", "syntax/bracket-mismatch")?;
                    break;
                }
                Ok(Expr::List { items, pos: token.pos })
            }
            Tok::Eof => Err(CompileError::new("syntax/unexpected-eof", token.pos, "表达式在文件结尾前没写完")),
            other => Err(CompileError::new("syntax/unexpected-token", token.pos,
                format!("表达式不能以 {:?} 开头", other.show()))),
        }
    }
}

/// 是否为标识符 token。
fn is_ident(tok: &Tok) -> bool {
    matches!(tok, Tok::Ident(_))
}

/// 把 f 串插值里的 token 片段解析成表达式。
pub fn parse_interpolation(pieces: &[Token]) -> Result<Expr, CompileError> {
    let mut parser = Parser { tokens: pieces.to_vec(), at: 0 };
    let expr = parser.parse_expr()?;
    if parser.at < parser.tokens.len() {
        let token = parser.cur().clone();
        return Err(CompileError::new("syntax/interpolation-trailing", token.pos,
            format!("插值表达式后面还跟着 {:?}", token.tok.show())));
    }
    Ok(expr)
}

/// f 串片段里的表达式段是否需要求值。
pub fn piece_is_expr(piece: &FStrPiece) -> bool {
    matches!(piece, FStrPiece::Expr(_))
}
