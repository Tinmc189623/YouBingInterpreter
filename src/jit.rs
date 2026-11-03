//! Cranelift 后端：把纯整数子集的字节码编成本机码。
//!
//! 入参约定与 VM 一致：所有实参都是整数，按形参顺序打包成 `*const i64` 传进来，返回值也是整数。
//! 编译前先做两件事——栈形分析（每个基本块进出时操作数栈必须为空，布尔值不允许跨块）
//! 和定值分析（读局部槽位前必须在所有路径上都写过）。任一条不满足就返回 Err，
//! 调用方把函数拉黑退回解释，绝不产生和 VM 不一致的结果。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{
    AbiParam, Block, Function, InstBuilder, MemFlags, MemFlagsData, Signature, UserFuncName,
    Value as Clif,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_control::ControlPlane;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_native;

use crate::bytecode::{BinOp, Op, Proto, UnOp};
use crate::value::JitFn;

/// 已分配的可执行内存总字节数，`youbing bench` 报告用。
static CODE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// 本机码函数编号，只用来给 Cranelift 生成唯一的符号名。
static FUNC_SEQ: AtomicUsize = AtomicUsize::new(1);

/// 已编译成功的次数。
pub fn code_bytes() -> usize {
    CODE_BYTES.load(Ordering::Relaxed)
}

#[link(name = "kernel32")]
extern "system" {
    fn VirtualAlloc(address: *mut c_void, size: usize, alloc_type: u32, protect: u32) -> *mut c_void;
}

const MEM_COMMIT: u32 = 0x0000_1000;
const MEM_RESERVE: u32 = 0x0000_2000;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// 栈上的一个槽位：整数本机值，或比较指令产出的布尔值（只允许被紧随的分支消费）。
#[derive(Copy, Clone)]
enum Slot {
    Int(Clif),
    Flag(Clif),
}

/// 编译一个原型，返回可直接调用的本机码入口。
pub fn compile(proto: &Proto) -> Result<JitFn, String> {
    let graph = Graph::build(proto)?;
    graph.check_locals(proto)?;
    let code = emit(proto, &graph)?;
    unsafe { install(&code) }
}

/// 基本图：块起点、跳转目标与每个块的指令区间。
struct Graph {
    /// 指令下标 → 块序号。
    block_of: Vec<usize>,
    /// 每块的起始指令下标。
    starts: Vec<usize>,
    /// 每块的后继块。
    successors: Vec<Vec<usize>>,
    /// 每块的前驱块。
    predecessors: Vec<Vec<usize>>,
}

impl Graph {
    /// 从字节码切块。跳转目标一律作为块起点，终止指令之后的指令也是块起点。
    fn build(proto: &Proto) -> Result<Graph, String> {
        let code = &proto.code;
        let mut is_start = vec![false; code.len() + 1];
        is_start[0] = true;
        for (index, instr) in code.iter().enumerate() {
            match instr.op {
                Op::Jump(target) | Op::JumpIfFalse(target) | Op::JumpIfTrue(target)
                | Op::JumpIfFalsePeek(target) | Op::JumpIfTruePeek(target) => {
                    if target as usize > code.len() {
                        return Err(format!("跳转目标 {target} 超出代码长度 {}", code.len()));
                    }
                    is_start[target as usize] = true;
                }
                _ => {}
            }
            if is_terminator(instr.op) && index + 1 < code.len() {
                is_start[index + 1] = true;
            }
        }
        let mut block_of = vec![usize::MAX; code.len() + 1];
        let mut starts = Vec::new();
        for (index, start) in is_start.iter().enumerate() {
            if *start {
                block_of[index] = starts.len();
                starts.push(index);
            }
        }
        // 落在块尾之后或终止指令之后的下标，归属它前面那个块起点。
        let mut current = 0usize;
        for index in 0..code.len() {
            if block_of[index] != usize::MAX {
                current = block_of[index];
            }
            block_of[index] = current;
        }
        let mut successors = vec![Vec::new(); starts.len()];
        for bid in 0..starts.len() {
            let end = starts.get(bid + 1).copied().unwrap_or(code.len());
            let Some(last) = code.get(end - 1) else {
                return Err("存在空基本块".to_string());
            };
            let mut succs = Vec::new();
            match last.op {
                Op::Jump(target) => succs.push(block_of[target as usize] as usize),
                Op::JumpIfFalse(target) | Op::JumpIfTrue(target)
                | Op::JumpIfFalsePeek(target) | Op::JumpIfTruePeek(target) => {
                    succs.push(block_of[target as usize] as usize);
                    succs.push(block_of[end] as usize);
                }
                Op::Return => {}
                _ => return Err(format!("块 {bid} 以 {:?} 结尾，本机码需要显式终止指令", last.op)),
            }
            successors[bid] = succs;
        }
        let mut predecessors = vec![Vec::new(); starts.len()];
        for (bid, succs) in successors.iter().enumerate() {
            for &succ in succs {
                if !predecessors[succ].contains(&bid) {
                    predecessors[succ].push(bid);
                }
            }
        }
        Ok(Graph { block_of, starts, successors, predecessors })
    }

    /// 每块进出时操作数栈必须为空，且读槽位前必须在所有路径上都定过值。
    ///
    /// 栈形在这里只验证"终止点栈深与出口一致"这一必要条件：本机码里每个值都是块内可见的
    /// Cranelift Value，跨块携带需要块参数，整数子集不允许出现这种情况。
    fn check_locals(&self, proto: &Proto) -> Result<(), String> {
        let slots = proto.locals.len();
        let mut entry = vec![false; slots];
        for index in 0..proto.arity {
            entry[index] = true;
        }
        // 不动点迭代：入口块初值为"只有形参已定值"，其余块从全真开始逐轮求交。
        let mut ins = vec![vec![true; slots]; self.starts.len()];
        ins[0] = entry;
        let mut changed = true;
        while changed {
            changed = false;
            for bid in 0..self.starts.len() {
                let mut set = self.flow_block(proto, bid, &ins[bid])?;
                if bid == 0 {
                    for index in 0..proto.arity {
                        set[index] = true;
                    }
                }
                for &succ in &self.successors[bid] {
                    let merged = &mut ins[succ];
                    for (seen, next) in merged.iter_mut().zip(&set) {
                        if *seen != *next {
                            *seen = *seen && *next;
                            changed = true;
                        }
                    }
                }
            }
        }
        // 最后再走一遍，用收敛后的入口集合报出真正的未定值读取。
        for bid in 0..self.starts.len() {
            self.flow_block(proto, bid, &ins[bid])?;
        }
        Ok(())
    }

    /// 按块内指令顺序推进定值集合，遇到未定值读取直接报错。
    fn flow_block(&self, proto: &Proto, bid: usize, entry: &[bool]) -> Result<Vec<bool>, String> {
        let start = self.starts[bid];
        let end = self.starts.get(bid + 1).copied().unwrap_or(proto.code.len());
        let mut set = entry.to_vec();
        for instr in &proto.code[start..end] {
            match instr.op {
                Op::LoadLocal(slot) => {
                    if !set.get(slot as usize).copied().unwrap_or(false) {
                        return Err(format!(
                            "槽位 {} 在 {}:{} 被读取，但存在未赋值的路径",
                            proto.locals[slot as usize], instr.pos.line, instr.pos.col
                        ));
                    }
                }
                Op::StoreLocal(slot) => {
                    if let Some(flag) = set.get_mut(slot as usize) {
                        *flag = true;
                    }
                }
                _ => {}
            }
        }
        Ok(set)
    }
}

/// 终止指令判定。
fn is_terminator(op: Op) -> bool {
    matches!(op, Op::Jump(_) | Op::JumpIfFalse(_) | Op::JumpIfTrue(_)
        | Op::JumpIfFalsePeek(_) | Op::JumpIfTruePeek(_) | Op::Return)
}

/// 生成机器码字节。
fn emit(proto: &Proto, graph: &Graph) -> Result<Vec<u8>, String> {
    // opt_level 与机器码校验器都是 shared settings，要放在共享 flags 构造器上，
    // 架构构造器只负责本机扩展（cranelift_native 已经按当前 CPU 推断过）。
    let mut shared_builder = settings::builder();
    shared_builder.set("opt_level", "speed").map_err(|error| error.to_string())?;
    shared_builder.enable("enable_machine_verifier").map_err(|error| error.to_string())?;
    let shared_flags = settings::Flags::new(shared_builder);
    let isa_builder = cranelift_native::builder().map_err(|error| error.to_string())?;
    let isa = isa_builder.finish(shared_flags).map_err(|error| error.to_string())?;

    let mut signature = Signature::new(isa.frontend_config().default_call_conv);
    // 只有一个入参：实参表指针，函数体内按 8 字节步长取出各形参。
    signature.params.push(AbiParam::new(I64));
    signature.returns.push(AbiParam::new(I64));

    let id = FUNC_SEQ.fetch_add(1, Ordering::Relaxed) as u32;
    let mut function = Function::with_name_signature(UserFuncName::user(0, id), signature);
    // 0.137 起 MemFlags 是 DFG 内部去重表的下标，访存前要先登记一份"无附加标志"的条目。
    let mem_flags = function.dfg.mem_flags.insert(MemFlagsData::new())
        .map_err(|_| "memflags 登记表已满".to_string())?;
    {
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let mut blocks: Vec<Block> = graph.starts.iter().map(|_| builder.create_block()).collect();
        if blocks.is_empty() {
            blocks.push(builder.create_block());
        }
        let mut jit = Emitter {
            proto,
            blocks,
            vars: Vec::new(),
            stack: Vec::new(),
            current: usize::MAX,
            emitted: HashSet::new(),
            block_of: graph.block_of.clone(),
            mem_flags,
        };
        jit.run(&mut builder)?;
        builder.finalize(isa.frontend_config());
    }
    let mut ctx = Context::for_function(function);
    let mut plane = ControlPlane::default();
    let compiled = ctx.compile(isa.as_ref(), &mut plane).map_err(|error| format!("{error:?}"))?;
    Ok(compiled.code_buffer().to_vec())
}

/// 一个原型的本机码生成器。
struct Emitter<'p> {
    proto: &'p Proto,
    blocks: Vec<Block>,
    /// 局部槽位对应的 Cranelift 可变变量，跨块由前端自动插 phi。
    vars: Vec<Variable>,
    stack: Vec<Slot>,
    current: usize,
    /// 真正被生成过的块，用来给不可达空块补终止指令。
    emitted: HashSet<usize>,
    /// 指令下标 → 块序号，来自图分析。
    block_of: Vec<usize>,
    /// 访存指令共用的标志位下标（本模块所有加载/存储都不带附加标志）。
    mem_flags: MemFlags,
}

impl Emitter<'_> {
    /// 线性扫一遍字节码，在块边界切换 Cranelift 基本块。
    fn run(&mut self, fb: &mut FunctionBuilder) -> Result<(), String> {
        let code = &self.proto.code;
        // 入口块的块参数与函数签名按顺序对应，这里只有一个：实参表指针。
        let entry = self.blocks[0];
        let pointer = fb.func.dfg.append_block_param(entry, I64);
        // 先把所有槽位声明并在入口块初始化成 0：定值分析已经保证真正的读取都有前置赋值，
        // 这里的初值只是让未走的分支上也有可用的 def，前端才能安全插 phi。
        fb.switch_to_block(entry);
        for _ in 0..self.proto.locals.len() {
            let var = fb.declare_var(I64);
            let zero = fb.ins().iconst(I64, 0);
            fb.def_var(var, zero);
            self.vars.push(var);
        }
        for index in 0..self.proto.arity {
            // 形参就是实参表里第 index 个 8 字节槽，直接用带立即偏移的 load 取。
            let value = fb.ins().load(I64, self.mem_flags, pointer, (index as i32) * 8);
            fb.def_var(self.vars[index], value);
        }
        self.current = 0;

        let mut index = 0usize;
        while index < code.len() {
            let bid = self.block_of[index];
            if bid != self.current {
                self.current = bid;
                // 整数子集不允许把操作数带过块边界：进新块前先确认上一块已经把栈清干净。
                self.require_empty()?;
                self.emitted.insert(bid);
                fb.switch_to_block(self.blocks[bid]);
            }
            self.instruction(fb, code[index].op)?;
            index += 1;
        }
        // 没有任何跳转落到的空块：补一条返回，Cranelift 要求每个块都以终止指令收尾。
        for (bid, block) in self.blocks.iter().enumerate() {
            if !self.emitted.contains(&bid) {
                fb.switch_to_block(*block);
                let zero = fb.ins().iconst(I64, 0);
                fb.ins().return_(&[zero]);
            }
        }
        Ok(())
    }

    /// 单条指令的 lowering。
    fn instruction(&mut self, fb: &mut FunctionBuilder, op: Op) -> Result<(), String> {
        match op {
            Op::Const(slot) => match self.proto.consts.get(slot as usize) {
                Some(crate::value::Value::Int(number)) => {
                    let value = fb.ins().iconst(I64, *number);
                    self.push(Slot::Int(value));
                    Ok(())
                }
                _ => Err("常量不是整数".to_string()),
            },
            Op::LoadLocal(slot) => {
                let value = fb.use_var(self.var(slot)?);
                self.push(Slot::Int(value));
                Ok(())
            }
            Op::StoreLocal(slot) => {
                let value = self.pop_int()?;
                fb.def_var(self.var(slot)?, value);
                Ok(())
            }
            Op::Pop => {
                self.stack.pop().ok_or_else(|| "栈空时弹出".to_string())?;
                Ok(())
            }
            Op::Dup => {
                let top = self.stack.last().cloned().ok_or_else(|| "栈空时复制".to_string())?;
                self.push(top);
                Ok(())
            }
            Op::Swap => {
                let len = self.stack.len();
                if len < 2 {
                    return Err("栈里不足两个值时交换".to_string());
                }
                self.stack.swap(len - 1, len - 2);
                Ok(())
            }
            Op::Unary(UnOp::Neg) => {
                let value = self.pop_int()?;
                let negated = fb.ins().ineg(value);
                self.push(Slot::Int(negated));
                Ok(())
            }
            Op::Binary(binop) => self.binary(fb, binop),
            Op::Jump(target) => {
                self.require_empty()?;
                let block = self.block(target)?;
                fb.ins().jump(block, &[]);
                Ok(())
            }
            Op::JumpIfFalse(target) => {
                let cond = self.condition(fb)?;
                let taken = self.block(target)?;
                let next = self.next_block()?;
                fb.ins().brif(cond, next, &[], taken, &[]);
                Ok(())
            }
            Op::JumpIfTrue(target) => {
                let cond = self.condition(fb)?;
                let taken = self.block(target)?;
                let next = self.next_block()?;
                fb.ins().brif(cond, taken, &[], next, &[]);
                Ok(())
            }
            Op::JumpIfFalsePeek(target) => {
                let cond = self.peek_condition(fb)?;
                let taken = self.block(target)?;
                let next = self.next_block()?;
                fb.ins().brif(cond, next, &[], taken, &[]);
                Ok(())
            }
            Op::JumpIfTruePeek(target) => {
                let cond = self.peek_condition(fb)?;
                let taken = self.block(target)?;
                let next = self.next_block()?;
                fb.ins().brif(cond, taken, &[], next, &[]);
                Ok(())
            }
            Op::Return => {
                let value = self.pop_int()?;
                self.require_empty()?;
                fb.ins().return_(&[value]);
                Ok(())
            }
            other => Err(format!("本机码不支持 {:?}", other)),
        }
    }

    /// 算术产出整数，比较产出只给分支用的布尔。
    fn binary(&mut self, fb: &mut FunctionBuilder, binop: BinOp) -> Result<(), String> {
        let right = self.pop_int()?;
        let left = self.pop_int()?;
        match binop {
            BinOp::Add => {
                let value = fb.ins().iadd(left, right);
                self.push(Slot::Int(value));
            }
            BinOp::Sub => {
                let value = fb.ins().isub(left, right);
                self.push(Slot::Int(value));
            }
            BinOp::Mul => {
                let value = fb.ins().imul(left, right);
                self.push(Slot::Int(value));
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne => {
                let cond = match binop {
                    BinOp::Lt => IntCC::SignedLessThan,
                    BinOp::Le => IntCC::SignedLessThanOrEqual,
                    BinOp::Gt => IntCC::SignedGreaterThan,
                    BinOp::Ge => IntCC::SignedGreaterThanOrEqual,
                    BinOp::Eq => IntCC::Equal,
                    _ => IntCC::NotEqual,
                };
                let flag = fb.ins().icmp(cond, left, right);
                self.push(Slot::Flag(flag));
            }
            BinOp::Div | BinOp::Mod => return Err("本机码不含除法与取余".to_string()),
        }
        Ok(())
    }

    /// 弹出栈顶并折成"为真"的布尔条件。
    fn condition(&mut self, fb: &mut FunctionBuilder) -> Result<Clif, String> {
        let top = self.stack.pop().ok_or_else(|| "分支时栈是空的".to_string())?;
        self.to_truth(fb, top)
    }

    /// 保留栈顶的"为真"条件（and/or 短路）。
    fn peek_condition(&mut self, fb: &mut FunctionBuilder) -> Result<Clif, String> {
        let top = self.stack.last().cloned().ok_or_else(|| "分支时栈是空的".to_string())?;
        self.to_truth(fb, top)
    }

    /// 整数按非零为真；比较结果本身就是布尔，只在本块里被这条分支消费掉。
    fn to_truth(&self, fb: &mut FunctionBuilder, slot: Slot) -> Result<Clif, String> {
        Ok(match slot {
            Slot::Flag(flag) => flag,
            Slot::Int(value) => {
                let zero = fb.ins().iconst(I64, 0);
                fb.ins().icmp(IntCC::NotEqual, value, zero)
            }
        })
    }

    fn push(&mut self, slot: Slot) {
        self.stack.push(slot);
    }

    /// 取整数栈值；布尔值参与运算说明这段代码超出了整数子集。
    fn pop_int(&mut self) -> Result<Clif, String> {
        match self.stack.pop() {
            Some(Slot::Int(value)) => Ok(value),
            Some(Slot::Flag(_)) => Err("布尔值被当成整数使用，退回解释执行".to_string()),
            None => Err("操作数栈已经空了".to_string()),
        }
    }

    /// 块边界上不允许残留操作数。
    fn require_empty(&self) -> Result<(), String> {
        if self.stack.is_empty() {
            Ok(())
        } else {
            Err("跳转时操作数栈不为空".to_string())
        }
    }

    fn var(&self, slot: u16) -> Result<Variable, String> {
        self.vars.get(slot as usize).copied().ok_or_else(|| format!("槽位 {slot} 不存在"))
    }

    fn block(&self, target: u32) -> Result<Block, String> {
        self.blocks.get(target as usize).copied().ok_or_else(|| format!("跳转目标 {target} 越界"))
    }

    /// 顺序执行的下一条指令所在块。
    fn next_block(&self) -> Result<Block, String> {
        let next = self.current + 1;
        self.blocks.get(next).copied().ok_or_else(|| "分支的常规后继越界".to_string())
    }
}

/// 把机器码放进可执行内存，返回强类型入口。
unsafe fn install(code: &[u8]) -> Result<JitFn, String> {
    let size = code.len();
    let address = VirtualAlloc(std::ptr::null_mut(), size, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    if address.is_null() {
        return Err("VirtualAlloc 申请可执行内存失败".to_string());
    }
    std::ptr::copy_nonoverlapping(code.as_ptr(), address as *mut u8, size);
    CODE_BYTES.fetch_add(size, Ordering::Relaxed);
    Ok(std::mem::transmute::<*mut c_void, JitFn>(address))
}
