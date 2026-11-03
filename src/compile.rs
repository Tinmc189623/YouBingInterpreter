//! 编译：AST → 字节码原型。
//! 局部名在函数入口一次性预声明（与 Python 同规则：函数里被赋值过的名字就是局部名），
//! 其余名字走运行时的三层解析：模块全局 → 内置表 → 报错。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{BOp, Expr, ImportTarget, Module, SrcRef, Stmt, UOp};
use crate::bytecode::{BinOp, Instr, Op, Proto, UnOp};
use crate::error::{CompileError, Pos};
use crate::lex::FStrPiece;
use crate::parse::parse_interpolation;
use crate::value::Value;

/// 编译整个模块，返回交给 VM 执行的顶层原型。
pub fn compile_module(module: &Module) -> Result<Rc<Proto>, CompileError> {
    let mut unit = Unit::new(&module.path, &module.path, true, Vec::new());
    for stmt in &module.stmts {
        unit.stmt(stmt)?;
    }
    unit.finish()
}

/// 循环上下文：break 需要回填，continue 目标在编译时已知。
struct Loop {
    is_for: bool,
    continue_at: u32,
    breaks: Vec<u32>,
}

/// 一个正在构造中的函数（模块顶层也当成一个函数来编）。
struct Unit {
    name: Box<str>,
    source: Box<str>,
    is_module: bool,
    params: Vec<Box<str>>,
    code: Vec<Instr>,
    consts: Vec<Value>,
    const_index: HashMap<(u8, String), u32>,
    names: Vec<Box<str>>,
    name_index: HashMap<Box<str>, u32>,
    locals: Vec<Box<str>>,
    local_index: HashMap<Box<str>, u16>,
    kwsets: Vec<Vec<Box<str>>>,
    protos: Vec<Rc<Proto>>,
    loops: Vec<Loop>,
}

impl Unit {
    /// 新建构造器。函数会用声明集预开槽位，模块顶层不需要。
    fn new(name: &str, source: &str, is_module: bool, params: Vec<Box<str>>) -> Unit {
        let mut unit = Unit {
            name: name.into(),
            source: source.into(),
            is_module,
            params: Vec::new(),
            code: Vec::new(),
            consts: Vec::new(),
            const_index: HashMap::new(),
            names: Vec::new(),
            name_index: HashMap::new(),
            locals: Vec::new(),
            local_index: HashMap::new(),
            kwsets: Vec::new(),
            protos: Vec::new(),
            loops: Vec::new(),
        };
        for param in params {
            unit.declare(&param);
        }
        unit.params = unit.locals.clone();
        unit
    }

    /// 收口成 Proto。
    fn finish(self) -> Result<Rc<Proto>, CompileError> {
        let arity = self.params.len();
        Ok(Rc::new(Proto {
            name: self.name,
            source: self.source,
            params: self.params,
            locals: self.locals,
            code: self.code,
            consts: self.consts,
            names: self.names,
            kwsets: self.kwsets,
            protos: self.protos,
            arity,
        }))
    }

    /// 追加一条指令，返回它的下标（跳转目标就是下标）。
    fn push(&mut self, op: Op, pos: Pos) -> u32 {
        self.code.push(Instr { op, pos });
        (self.code.len() - 1) as u32
    }

    /// 当前代码长度，即「下一条指令」的下标。
    fn here(&self) -> u32 {
        self.code.len() as u32
    }

    /// 把一条跳转指令的操作数改成 target。
    fn patch(&mut self, index: u32, target: u32) -> Result<(), CompileError> {
        let pos = self.code[index as usize].pos;
        match &mut self.code[index as usize].op {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t) | Op::IterNext { end: t } => { *t = target; Ok(()) }
            other => Err(CompileError::new("internal/patch-not-jump", pos,
                format!("回填目标时发现的不是跳转指令，而是 {other:?}"))),
        }
    }

    /// 常量入池，按「类型 + 文本」去重。
    fn konst(&mut self, value: Value) -> u32 {
        let key = (value.tag(), value.text_of());
        if let Some(index) = self.const_index.get(&key) {
            return *index;
        }
        let index = self.consts.len() as u32;
        self.consts.push(value.clone());
        self.const_index.insert(key, index);
        index
    }

    /// 名字入池。
    fn intern(&mut self, text: &str) -> u32 {
        if let Some(index) = self.name_index.get(text) {
            return *index;
        }
        let index = self.names.len() as u32;
        self.names.push(text.into());
        self.name_index.insert(text.into(), index);
        index
    }

    /// 局部槽位，没有就新开一个。
    fn declare(&mut self, text: &str) -> u16 {
        if let Some(slot) = self.local_index.get(text) {
            return *slot;
        }
        let slot = self.locals.len() as u16;
        self.locals.push(text.into());
        self.local_index.insert(text.into(), slot);
        slot
    }

    /// 查已有局部槽位。
    fn slot(&self, text: &str) -> Option<u16> {
        self.local_index.get(text).copied()
    }

    /// 写绑定：模块顶层写全局，函数里优先写已声明的局部。
    fn emit_store(&mut self, name: &str, pos: Pos) {
        if !self.is_module {
            if let Some(slot) = self.slot(name) {
                self.push(Op::StoreLocal(slot), pos);
                return;
            }
        }
        let index = self.intern(name);
        self.push(Op::StoreName(index), pos);
    }

    /// 读名字：函数里的局部名直接取槽位，其余交给运行时解析。
    fn emit_load(&mut self, name: &str, pos: Pos) {
        if let Some(slot) = self.slot(name) {
            if !self.is_module {
                self.push(Op::LoadLocal(slot), pos);
                return;
            }
        }
        let index = self.intern(name);
        self.push(Op::LoadName(index), pos);
    }

    /// 一条语句。
    fn stmt(&mut self, stmt: &Stmt) -> Result<(), CompileError> {
        match stmt {
            Stmt::Expr { expr, pos } => {
                self.expr(expr)?;
                self.push(Op::Pop, *pos);
            }
            Stmt::Assign { target, value, pos } => self.assign(target, value, *pos)?,
            Stmt::If { arms, orelse, pos } => self.if_chain(arms, orelse.as_deref(), *pos)?,
            Stmt::While { cond, body, pos } => self.while_loop(cond, body, *pos)?,
            Stmt::For { var, iter, body, pos } => self.for_loop(var, iter, body, *pos)?,
            Stmt::FuncDef { name, params, body, decorators, pos } => {
                self.funcdef(name, params, body, decorators, *pos)?
            }
            Stmt::Return { value, pos } => {
                if self.is_module {
                    return Err(CompileError::new("syntax/return-outside-function", *pos,
                        "return 只能出现在函数体里"));
                }
                match value {
                    Some(expr) => self.expr(expr)?,
                    None => { let index = self.konst(Value::None); self.push(Op::Const(index), *pos); }
                }
                self.push(Op::Return, *pos);
            }
            Stmt::Import { source, target, pos } => self.import(source, target, *pos)?,
            Stmt::Pass { pos } => { let index = self.konst(Value::None); self.push(Op::Const(index), *pos); self.push(Op::Pop, *pos); }
            Stmt::Break { pos } => {
                let index = self.loops.len().checked_sub(1)
                    .ok_or_else(|| CompileError::new("syntax/break-outside-loop", *pos, "break 不在循环里"))?;
                let jump = self.push(Op::Jump(0), *pos);
                self.loops[index].breaks.push(jump);
            }
            Stmt::Continue { pos } => {
                let index = self.loops.len().checked_sub(1)
                    .ok_or_else(|| CompileError::new("syntax/continue-outside-loop", *pos, "continue 不在循环里"))?;
                let target = self.loops[index].continue_at;
                self.push(Op::Jump(target), *pos);
            }
        }
        Ok(())
    }

    /// 赋值：目标是名字、点号链或下标。
    fn assign(&mut self, target: &Expr, value: &Expr, pos: Pos) -> Result<(), CompileError> {
        match target {
            Expr::Name { name, .. } => {
                self.expr(value)?;
                self.emit_store(name, pos);
            }
            Expr::Path { root, segs, .. } => {
                let last = segs.last().ok_or_else(|| CompileError::new("syntax/invalid-assignment-target", pos,
                    "点号链的末端不能做赋值目标"))?;
                self.expr_root(root, pos);
                for seg in segs.iter().take(segs.len() - 1) {
                    let index = self.intern(seg);
                    self.push(Op::GetAttr(index), pos);
                }
                self.expr(value)?;
                let index = self.intern(last);
                self.push(Op::SetAttr(index), pos);
            }
            Expr::Index { object, index, .. } => {
                self.expr(object)?;
                self.expr(index)?;
                self.expr(value)?;
                self.push(Op::SetIndex, pos);
            }
            other => return Err(CompileError::new("syntax/invalid-assignment-target", other.pos(),
                "等号左边只能是名字、点号链或下标")),
        }
        Ok(())
    }

    /// if / elif / else：arms 是 (条件, 体) 的序列，orelse 是最后的兜底。
    fn if_chain(&mut self, arms: &[(Expr, Vec<Stmt>)], orelse: Option<&[Stmt]>, pos: Pos) -> Result<(), CompileError> {
        let mut exits = Vec::new();
        for (cond, body) in arms {
            self.expr(cond)?;
            let skip = self.push(Op::JumpIfFalse(0), pos);
            self.block(body)?;
            if !body_is_exit(body) {
                exits.push(self.push(Op::Jump(0), pos));
            }
            self.patch(skip, self.here())?;
        }
        if let Some(body) = orelse {
            self.block(body)?;
        }
        let end = self.here();
        for exit in exits {
            self.patch(exit, end)?;
        }
        Ok(())
    }

    /// while 循环。
    fn while_loop(&mut self, cond: &Expr, body: &[Stmt], pos: Pos) -> Result<(), CompileError> {
        let start = self.here();
        self.expr(cond)?;
        let exit = self.push(Op::JumpIfFalse(0), pos);
        self.loops.push(Loop { is_for: false, continue_at: start, breaks: Vec::new() });
        self.block(body)?;
        self.push(Op::Jump(start), pos);
        let loop_ctx = self.loops.pop().expect("循环上下文成对出入");
        self.patch(exit, self.here())?;
        for jump in loop_ctx.breaks {
            self.patch(jump, self.here())?;
        }
        Ok(())
    }

    /// for 循环：迭代器活在帧的迭代器栈上，break 出口要先回收。
    fn for_loop(&mut self, var: &str, iter: &Expr, body: &[Stmt], pos: Pos) -> Result<(), CompileError> {
        self.expr(iter)?;
        self.push(Op::IterStart, pos);
        let start = self.here();
        let exit = self.push(Op::IterNext { end: 0 }, pos);
        if self.is_module {
            let index = self.intern(var);
            self.push(Op::Dup, pos);
            self.push(Op::StoreName(index), pos);
            self.push(Op::Pop, pos);
        } else {
            let slot = self.declare(var);
            self.push(Op::StoreLocal(slot), pos);
        }
        self.loops.push(Loop { is_for: true, continue_at: start, breaks: Vec::new() });
        self.block(body)?;
        self.push(Op::Jump(start), pos);
        let loop_ctx = self.loops.pop().expect("循环上下文成对出入");
        // break 跳到这里：先把迭代器丢掉，再落到循环后。
        let cleanup = self.here();
        for jump in &loop_ctx.breaks {
            self.patch(*jump, cleanup)?;
        }
        if !loop_ctx.breaks.is_empty() {
            self.push(Op::IterPop, pos);
        }
        self.patch(exit, self.here())?;
        Ok(())
    }

    /// 函数定义：先编子原型，再按装饰器应用。
    fn funcdef(&mut self, name: &str, params: &[Box<str>], body: &[Stmt], decorators: &[Expr], pos: Pos) -> Result<(), CompileError> {
        if !self.is_module {
            // 函数名在函数作用域里就是局部名，预声明好让读也能命中槽位。
            self.declare(name);
        }
        let mut child = Unit::new(name, &self.source, false, params.to_vec());
        for stmt in body {
            child.stmt(stmt)?;
        }
        let proto = child.finish()?;
        let index = self.protos.len() as u32;
        self.protos.push(proto);
        self.push(Op::MakeFunc(index), pos);
        // 装饰器由内向外应用：MakeFunc 压栈后，每个装饰器表达式压到它上面再交换、调用。
        for decorator in decorators.iter().rev() {
            self.expr(decorator)?;
            self.push(Op::Swap, pos);
            self.push(Op::Call { argc: 1, kwset: u32::MAX }, decorator.pos());
        }
        self.emit_store(name, pos);
        Ok(())
    }

    /// import：`import "x"` 绑模块本身，`from 来源 import "x"` 绑来源里的一个绑定。
    fn import(&mut self, source: &Option<SrcRef>, target: &ImportTarget, pos: Pos) -> Result<(), CompileError> {
        match source {
            None => {
                let path = match target {
                    ImportTarget::Quoted(raw) => raw.as_ref().to_string(),
                    ImportTarget::Bare(raw) => raw.as_ref().to_string(),
                };
                let index = self.intern(&path);
                self.push(Op::ImportModule(index), pos);
            }
            Some(src) => {
                let source_text = match src {
                    SrcRef::Module(name) => name.as_ref().to_string(),
                    SrcRef::Relative(depth) => ".".repeat(*depth as usize),
                };
                let target_text = match target {
                    ImportTarget::Quoted(raw) => raw.as_ref().to_string(),
                    ImportTarget::Bare(raw) => raw.as_ref().to_string(),
                };
                let source = self.intern(&source_text);
                let name = self.intern(&target_text);
                self.push(Op::ImportFrom { source, target: name }, pos);
            }
        }
        self.emit_store(target.binding(), pos);
        Ok(())
    }

    /// 编一组语句。
    fn block(&mut self, stmts: &[Stmt]) -> Result<(), CompileError> {
        for stmt in stmts {
            self.stmt(stmt)?;
        }
        Ok(())
    }

    /// 表达式：求值结果留在栈顶。
    fn expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        match expr {
            Expr::Int { value, pos } => { let index = self.konst(Value::Int(*value)); self.push(Op::Const(index), *pos); }
            Expr::Bool { value, pos } => { let index = self.konst(Value::Bool(*value)); self.push(Op::Const(index), *pos); }
            Expr::NoneLit { pos } => { let index = self.konst(Value::None); self.push(Op::Const(index), *pos); }
            Expr::Str { text, pos } => { let index = self.konst(Value::text(text)); self.push(Op::Const(index), *pos); }
            Expr::FStr { pieces, pos } => self.fstr(pieces, *pos)?,
            Expr::Name { name, pos } => self.emit_load(name, *pos),
            Expr::Path { root, segs, pos } => {
                self.expr_root(root, *pos);
                for seg in segs {
                    let index = self.intern(seg);
                    self.push(Op::GetAttr(index), *pos);
                }
            }
            Expr::List { items, pos } => {
                for item in items {
                    self.expr(item)?;
                }
                self.push(Op::BuildList(items.len() as u16), *pos);
            }
            Expr::Unary { op, operand, pos } => {
                self.expr(operand)?;
                let op = match op {
                    UOp::Neg => UnOp::Neg,
                    UOp::Not => UnOp::Not,
                };
                self.push(Op::Unary(op), *pos);
            }
            Expr::Binary { op: BOp::And, left, right, pos } => {
                self.expr(left)?;
                let skip = self.push(Op::JumpIfFalsePeek(0), *pos);
                self.push(Op::Pop, *pos);
                self.expr(right)?;
                self.patch(skip, self.here())?;
            }
            Expr::Binary { op: BOp::Or, left, right, pos } => {
                self.expr(left)?;
                let skip = self.push(Op::JumpIfTruePeek(0), *pos);
                self.push(Op::Pop, *pos);
                self.expr(right)?;
                self.patch(skip, self.here())?;
            }
            Expr::Binary { op, left, right, pos } => {
                let Some(op) = binop(*op) else {
                    return Err(CompileError::new("internal/logical-op-late", *pos,
                        "and/or 应当在前面就被折成短路跳转，不该走到这里"));
                };
                self.expr(left)?;
                self.expr(right)?;
                self.push(Op::Binary(op), *pos);
            }
            Expr::Index { object, index, pos } => {
                self.expr(object)?;
                self.expr(index)?;
                self.push(Op::GetIndex, *pos);
            }
            Expr::Call { callee, args, kwargs, pos } => self.call(callee, args, kwargs, *pos)?,
        }
        Ok(())
    }

    /// 点号链的首段：局部名走槽位，其余按名字解析（模块、全局都在这一层）。
    fn expr_root(&mut self, root: &str, pos: Pos) {
        self.emit_load(root, pos);
    }

    /// f 串：字面段与表达式段依次入栈，最后 JoinStr 合并。
    fn fstr(&mut self, pieces: &[FStrPiece], pos: Pos) -> Result<(), CompileError> {
        let mut count = 0u16;
        for piece in pieces {
            match piece {
                FStrPiece::Lit(text) => {
                    let index = self.konst(Value::text(text));
                    self.push(Op::Const(index), pos);
                    count += 1;
                }
                FStrPiece::Expr(tokens) => {
                    let expr = parse_interpolation(tokens)?;
                    self.expr(&expr)?;
                    count += 1;
                }
            }
        }
        if count == 0 {
            let index = self.konst(Value::text(""));
            self.push(Op::Const(index), pos);
            return Ok(());
        }
        self.push(Op::JoinStr(count), pos);
        Ok(())
    }

    /// 调用：`a.b(...)` 编成 CallMethod，其余先求值可调用再 Call。
    fn call(&mut self, callee: &Expr, args: &[Expr], kwargs: &[(Box<str>, Expr)], pos: Pos) -> Result<(), CompileError> {
        let kwset = if kwargs.is_empty() {
            u32::MAX
        } else {
            let names: Vec<Box<str>> = kwargs.iter().map(|(key, _)| key.clone()).collect();
            let index = self.kwsets.len() as u32;
            self.kwsets.push(names);
            index
        };
        if let Expr::Path { root, segs, .. } = callee {
            if !segs.is_empty() {
                self.expr_root(root, pos);
                for seg in segs.iter().take(segs.len() - 1) {
                    let index = self.intern(seg);
                    self.push(Op::GetAttr(index), pos);
                }
                for arg in args {
                    self.expr(arg)?;
                }
                for (_, value) in kwargs {
                    self.expr(value)?;
                }
                let last = self.intern(segs.last().expect("前面已判非空"));
                self.push(Op::CallMethod { name: last, argc: args.len() as u16, kwset }, pos);
                return Ok(());
            }
        }
        self.expr(callee)?;
        for arg in args {
            self.expr(arg)?;
        }
        for (_, value) in kwargs {
            self.expr(value)?;
        }
        self.push(Op::Call { argc: args.len() as u16, kwset }, pos);
        Ok(())
    }
}

/// AST 二元运算符映射到字节码；and/or 由短路跳转承担，这里返回 None。
fn binop(op: BOp) -> Option<BinOp> {
    Some(match op {
        BOp::Add => BinOp::Add,
        BOp::Sub => BinOp::Sub,
        BOp::Mul => BinOp::Mul,
        BOp::Div => BinOp::Div,
        BOp::Mod => BinOp::Mod,
        BOp::Eq => BinOp::Eq,
        BOp::Ne => BinOp::Ne,
        BOp::Lt => BinOp::Lt,
        BOp::Le => BinOp::Le,
        BOp::Gt => BinOp::Gt,
        BOp::Ge => BinOp::Ge,
        BOp::And | BOp::Or => return None,
    })
}

/// 块体末尾是否是 return，用来省掉永远执行不到的跳转。
fn body_is_exit(body: &[Stmt]) -> bool {
    matches!(body.last(), Some(Stmt::Return { .. }))
}
