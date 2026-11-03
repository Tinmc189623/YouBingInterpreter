//! AST：只描述语法结构，不带任何执行语义。
//! 每个节点存源码位置，编译期与运行期报错都能指回原行。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use crate::error::Pos;
use crate::lex::FStrPiece;

/// 一元运算符。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UOp { Neg, Not }

/// 二元运算符。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BOp { Add, Sub, Mul, Div, Mod, Eq, Ne, Lt, Le, Gt, Ge, And, Or }

/// 表达式节点。
#[derive(Debug)]
pub enum Expr {
    Int { value: i64, pos: Pos },
    Bool { value: bool, pos: Pos },
    NoneLit { pos: Pos },
    Str { text: Box<str>, pos: Pos },
    /// f 串：字面段与表达式段交替。
    FStr { pieces: Vec<FStrPiece>, pos: Pos },
    Name { name: Box<str>, pos: Pos },
    /// 点号成员链，root 是首段名，segs 是后续各段。
    Path { root: Box<str>, segs: Vec<Box<str>>, pos: Pos },
    List { items: Vec<Expr>, pos: Pos },
    Unary { op: UOp, operand: Box<Expr>, pos: Pos },
    Binary { op: BOp, left: Box<Expr>, right: Box<Expr>, pos: Pos },
    Index { object: Box<Expr>, index: Box<Expr>, pos: Pos },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        kwargs: Vec<(Box<str>, Expr)>,
        pos: Pos,
    },
}

impl Expr {
    /// 表达式起始位置。
    pub fn pos(&self) -> Pos {
        match self {
            Expr::Int { pos, .. } | Expr::Bool { pos, .. } | Expr::NoneLit { pos }
            | Expr::Str { pos, .. } | Expr::FStr { pos, .. } | Expr::Name { pos, .. }
            | Expr::Path { pos, .. } | Expr::List { pos, .. } | Expr::Unary { pos, .. }
            | Expr::Binary { pos, .. } | Expr::Index { pos, .. } | Expr::Call { pos, .. } => *pos,
        }
    }
}

/// from 后面的来源：具名模块或若干层相对定位。
#[derive(Debug)]
pub enum SrcRef {
    Module(Box<str>),
    Relative(u32),
}

/// import 后面被绑定的名字。
#[derive(Debug)]
pub enum ImportTarget {
    /// 带引号的模块名，绑定名取路径最后一段。
    Quoted(Box<str>),
    /// 裸标识符。
    Bare(Box<str>),
}

impl ImportTarget {
    /// 实际绑进命名空间的名字。
    pub fn binding(&self) -> &str {
        match self {
            ImportTarget::Quoted(raw) => raw.split(['.', '/']).filter(|s| !s.is_empty()).next_back().unwrap_or(raw),
            ImportTarget::Bare(name) => name,
        }
    }
}

/// 语句节点。
#[derive(Debug)]
pub enum Stmt {
    Expr { expr: Expr, pos: Pos },
    /// 赋值：target 是名字、点号链或下标表达式。
    Assign { target: Expr, value: Expr, pos: Pos },
    If { arms: Vec<(Expr, Vec<Stmt>)>, orelse: Option<Vec<Stmt>>, pos: Pos },
    While { cond: Expr, body: Vec<Stmt>, pos: Pos },
    For { var: Box<str>, iter: Expr, body: Vec<Stmt>, pos: Pos },
    FuncDef {
        name: Box<str>,
        params: Vec<Box<str>>,
        body: Vec<Stmt>,
        decorators: Vec<Expr>,
        pos: Pos,
    },
    Return { value: Option<Expr>, pos: Pos },
    Import { source: Option<SrcRef>, target: ImportTarget, pos: Pos },
    Pass { pos: Pos },
    Break { pos: Pos },
    Continue { pos: Pos },
}

impl Stmt {
    /// 语句起始位置。
    pub fn pos(&self) -> Pos {
        match self {
            Stmt::Expr { pos, .. } | Stmt::Assign { pos, .. } | Stmt::If { pos, .. }
            | Stmt::While { pos, .. } | Stmt::For { pos, .. } | Stmt::FuncDef { pos, .. }
            | Stmt::Return { pos, .. } | Stmt::Import { pos, .. }
            | Stmt::Pass { pos } | Stmt::Break { pos } | Stmt::Continue { pos } => *pos,
        }
    }
}

/// 一个 .yb 文件解析出的模块。
#[derive(Debug)]
pub struct Module {
    pub path: String,
    pub stmts: Vec<Stmt>,
}
