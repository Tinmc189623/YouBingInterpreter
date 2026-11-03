//! 诊断与错误：编译期问题带源码位置，运行期错误带退出码映射。
//! 规则码沿用 yb-cj 插件的命名（lex/* syntax/* block/* ...），让编辑器与解释器报同一个名字。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::fmt;

/// 源码位置，行与列都从 1 开始，列按字符计（不按字节）。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

impl Pos {
    /// 构造一个位置。
    pub fn new(line: u32, col: u32) -> Self {
        Pos { line, col }
    }
}

/// 编译期诊断，首错即停，但词法阶段的可恢复问题会一起带出来。
#[derive(Debug)]
pub struct CompileError {
    pub code: &'static str,
    pub msg: String,
    pub pos: Pos,
}

impl CompileError {
    /// 造一条诊断。
    pub fn new(code: &'static str, pos: Pos, msg: impl Into<String>) -> Self {
        CompileError { code, msg: msg.into(), pos }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}:{}: {}", self.code, self.pos.line, self.pos.col, self.msg)
    }
}

/// 运行期错误种类，决定进程退出码。
#[derive(Debug)]
pub enum RunError {
    /// 脚本调用 exit(n)，按 n 结束。
    Exit(i32),
    /// import 找不到的模块。
    ModuleNotFound(String),
    /// 名字没绑定就被使用。
    NameError(String),
    /// 运算符或调用拿到不该拿的类型。
    TypeError(String),
    /// 除零或索引越界。
    RangeError(String),
    /// 帧数超过上限。
    StackOverflow,
    /// 语法书写的构造解释器暂不支持。
    Unsupported(String),
}

impl RunError {
    /// 错误对应的进程退出码。
    pub fn exit_code(&self) -> i32 {
        match self {
            RunError::Exit(code) => *code,
            RunError::ModuleNotFound(_) => 3,
            RunError::NameError(_) => 4,
            RunError::TypeError(_) => 4,
            RunError::RangeError(_) => 5,
            RunError::StackOverflow => 6,
            RunError::Unsupported(_) => 7,
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Exit(code) => write!(f, "exit({code})"),
            RunError::ModuleNotFound(name) => write!(f, "module/not-found: 找不到模块 {name}"),
            RunError::NameError(name) => write!(f, "name/unbound: {name} 没有绑定"),
            RunError::TypeError(msg) => write!(f, "type/mismatch: {msg}"),
            RunError::RangeError(msg) => write!(f, "value/out-of-range: {msg}"),
            RunError::StackOverflow => write!(f, "runtime/stack-overflow: 帧数超过上限"),
            RunError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
        }
    }
}
