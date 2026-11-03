//! 字节码：指令、原型与常量/名字池。
//! 指令直接放 `Vec<Instr>`（不压平成字节），代价是内存略大，好处是 VM 主循环少一层解码、
//! 跳转目标就是下标，补丁与反汇编都直观。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::rc::Rc;

use crate::error::Pos;
use crate::value::Value;

/// 二元运算符，and/or 在编译期就折成短路跳转，不进这里。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOp { Add, Sub, Mul, Div, Mod, Eq, Ne, Lt, Le, Gt, Ge }

/// 一元运算符。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnOp { Neg, Not }

impl BinOp {
    /// 原文，报错与反汇编用。
    pub fn text(self) -> &'static str {
        match self {
            BinOp::Add => "+", BinOp::Sub => "-", BinOp::Mul => "*", BinOp::Div => "/",
            BinOp::Mod => "%", BinOp::Eq => "==", BinOp::Ne => "!=", BinOp::Lt => "<",
            BinOp::Le => "<=", BinOp::Gt => ">", BinOp::Ge => ">=",
        }
    }
}

/// 一条指令。操作数都是池下标或跳转目标，运行期不再解释语法。
#[derive(Clone, Copy, Debug)]
pub enum Op {
    /// 常量池入栈。
    Const(u32),
    Pop,
    Dup,
    /// 交换栈顶两个值，装饰器应用时把可调用换到参数下面。
    Swap,
    /// 局部槽位入栈。
    LoadLocal(u16),
    /// 弹出栈顶写入局部槽位。
    StoreLocal(u16),
    /// 名字池入栈：先查模块全局，再查内置表。
    LoadName(u32),
    /// 弹出栈顶写入模块全局（同时作为语句结果的绑定）。
    StoreName(u32),
    /// 栈顶属性读。
    GetAttr(u32),
    /// [对象, 值] → 属性写，值出栈。
    SetAttr(u32),
    /// [对象, 下标] → 取下标。
    GetIndex,
    /// [对象, 下标, 值] → 写下标。
    SetIndex,
    /// 弹出 n 个构造列表。
    BuildList(u16),
    /// 弹出 n 段拼成字符串（f 串）。
    JoinStr(u16),
    Binary(BinOp),
    Unary(UnOp),
    /// 无条件跳转。
    Jump(u32),
    /// 弹出后为假跳转。
    JumpIfFalse(u32),
    /// 弹出后为真跳转。
    JumpIfTrue(u32),
    /// 保留栈顶，为假跳转（and 的短路）。
    JumpIfFalsePeek(u32),
    /// 保留栈顶，为真跳转（or 的短路）。
    JumpIfTruePeek(u32),
    /// [可调用, 位置参..., 关键字值...]；kwset 为 u32::MAX 表示没有关键字参。
    Call { argc: u16, kwset: u32 },
    /// [接收者, 位置参..., 关键字值...]：按接收者实际类型找方法。
    CallMethod { name: u32, argc: u16, kwset: u32 },
    /// 弹出可迭代值，压进当前帧的迭代器栈。
    IterStart,
    /// 从栈顶迭代器取下一个元素入栈；取完则弹出迭代器并跳到 end。
    IterNext { end: u32 },
    /// 丢掉栈顶迭代器（break 离开 for 时使用）。
    IterPop,
    /// 返回栈顶。
    Return,
    /// 子原型建函数对象入栈。
    MakeFunc(u32),
    /// 载入模块值入栈（`import "std"` 的右半）。
    ImportModule(u32),
    /// 从来源模块取一个绑定入栈（`from app import "app"` 的右半）。
    ImportFrom { source: u32, target: u32 },
}

/// 指令 + 源码位置，运行期报错能指回原行。
#[derive(Clone, Copy, Debug)]
pub struct Instr {
    pub op: Op,
    pub pos: Pos,
}

/// 函数原型：代码与全部静态信息。
pub struct Proto {
    /// 展示名，函数名或模块路径。
    pub name: Box<str>,
    /// 定义所在文件，报错用。
    pub source: Box<str>,
    /// 形参名，顺序即槽位顺序。
    pub params: Vec<Box<str>>,
    /// 全部局部名（含形参），下标即槽位。
    pub locals: Vec<Box<str>>,
    pub code: Vec<Instr>,
    pub consts: Vec<Value>,
    pub names: Vec<Box<str>>,
    /// 每个调用点的关键字参名集合。
    pub kwsets: Vec<Vec<Box<str>>>,
    /// 嵌套函数原型。
    pub protos: Vec<Rc<Proto>>,
    /// 形参个数，调用检查用。
    pub arity: usize,
}

impl Proto {
    /// 新建一个空原型。
    pub fn new(name: &str, source: &str) -> Proto {
        Proto {
            name: name.into(),
            source: source.into(),
            params: Vec::new(),
            locals: Vec::new(),
            code: Vec::new(),
            consts: Vec::new(),
            names: Vec::new(),
            kwsets: Vec::new(),
            protos: Vec::new(),
            arity: 0,
        }
    }

    /// 反汇编成一行一条的文本，`youbing dump` 与调试用。
    pub fn disassemble(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "; 函数 {} （{} 个形参，{} 个槽位）", self.name, self.arity, self.locals.len());
        for (index, instr) in self.code.iter().enumerate() {
            let _ = writeln!(out, "{index:4} {}:{}  {:?}", instr.pos.line, instr.pos.col, instr.op);
        }
        for (index, constant) in self.consts.iter().enumerate() {
            let _ = writeln!(out, "常量 {index}: {constant:?}");
        }
        for child in &self.protos {
            out.push_str(&child.disassemble());
        }
        out
    }
}
