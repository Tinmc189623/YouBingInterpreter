//! 栈式虚拟机：一条指令一个大 match，帧栈与值栈分离，for 的迭代器挂在帧上。
//! 调用点同时是分层升级点：解释累计到阈值且类型反馈仍是纯整数时，交给 Cranelift 编成机器码；
//! 之后实参类型一旦破供就永久退回解释（blacklist），保证 JIT 与 VM 的结果始终一致。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::bytecode::{BinOp, Op, Proto, UnOp};
use crate::compile::compile_module;
use crate::error::{CompileError, Pos, RunError};
use crate::host::Host;
use crate::jit;
use crate::parse::parse_file;
use crate::tier;
use crate::value::{BuiltinMethod, Function, FuncState, Globals, Value, YbModule};

/// 帧数上限，递归过深时变成可报的运行期错误而不是崩栈。
pub const MAX_FRAMES: usize = 512;

/// 一次调用的帧。
struct Frame {
    proto: Rc<Proto>,
    globals: Globals,
    slots: Vec<Value>,
    /// 值栈起点，返回时按它回收调用点的操作数。
    base: usize,
    ip: usize,
    /// for 循环的迭代器栈：(元素表, 游标)。
    iters: Vec<(Vec<Value>, usize)>,
}

/// 解释器实例：一个 Vm 跑一个项目，模块缓存与宿主状态都挂在上面。
pub struct Vm {
    pub host: Host,
    frames: Vec<Frame>,
    stack: Vec<Value>,
    /// 已加载的油饼模块，键是点号路径。
    loaded: HashMap<String, Value>,
    /// 目录包（<名字>/init.yb）集合，决定 `.` 相对导入落在哪一层。
    packages: HashSet<String>,
    /// 已执行指令数，基准测试报告用。
    pub executed: u64,
    pub jit_compiles: u32,
    pub jit_calls: u64,
    pub jit_on: bool,
}

impl Vm {
    /// 以项目根目录与脚本参数建一个虚拟机。
    pub fn new(root: PathBuf, args: Vec<String>) -> Vm {
        Vm {
            host: Host::new(root, args),
            frames: Vec::new(),
            stack: Vec::with_capacity(256),
            loaded: HashMap::new(),
            packages: HashSet::new(),
            executed: 0,
            jit_compiles: 0,
            jit_calls: 0,
            jit_on: true,
        }
    }

    /// 运行一个 .yb 文件；`exit(n)` 与正常结束都折算成进程退出码。
    pub fn run_file(&mut self, path: &Path) -> Result<i32, RunError> {
        let module_id = self.host.module_id_for_file(path);
        if module_id.contains('.') {
            self.packages.insert(module_id.rsplit_once('.').map(|(head, _)| head.to_string()).unwrap_or_default());
        }
        let text = read_source(path)?;
        let module = parse_file(&module_id, &text).map_err(RunError::from)?;
        let proto = compile_module(&module).map_err(RunError::from)?;
        self.execute_module(proto)
    }

    /// 编译好的顶层原型直接跑，返回退出码。
    pub fn execute_module(&mut self, proto: Rc<Proto>) -> Result<i32, RunError> {
        let module_id = proto.source.to_string();
        let globals = self.fresh_globals(&module_id);
        let handle = Value::Yb(Rc::new(YbModule { name: module_id.clone().into(), globals: globals.clone() }));
        self.loaded.insert(module_id, handle);
        self.frames.clear();
        self.stack.clear();
        self.push_frame(proto, globals, Vec::new())?;
        match self.pump(1) {
            Ok(_) => Ok(0),
            Err(RunError::Exit(code)) => Ok(code),
            Err(other) => Err(other),
        }
    }

    /// 新建模块命名空间，注入 `name` 让 `xorweb.Webservice(name)` 这类写法拿得到模块名。
    fn fresh_globals(&self, module_id: &str) -> Globals {
        let globals: Globals = Rc::new(RefCell::new(HashMap::new()));
        globals.borrow_mut().insert("name".into(), Value::text(module_id));
        globals
    }

    /// 宿主回调油饼函数：临时开帧跑到返回。
    pub fn invoke(&mut self, callee: &Value, args: Vec<Value>, kwargs: Vec<(Box<str>, Value)>)
        -> Result<Value, RunError> {
        match callee {
            Value::Func(function) => {
                let function = function.clone();
                let entry = self.frames.len() + 1;
                if let Some(value) = self.call_function(function, args, kwargs)? {
                    return Ok(value);
                }
                self.pump(entry)
            }
            Value::Native(native) => {
                let native = native.clone();
                native(self, args, kwargs)
            }
            other => Err(RunError::TypeError(format!("{} 不可调用", other.type_name()))),
        }
    }

    /// 取一个已加载或可加载的模块值，宿主绑定用。
    pub fn module(&mut self, path: &str) -> Result<Value, RunError> {
        let package = String::new();
        self.require(path, &package)
    }

    /// 执行指令直到给定帧层返回。
    fn pump(&mut self, min_frames: usize) -> Result<Value, RunError> {
        loop {
            if self.frames.is_empty() {
                return Ok(self.stack.last().cloned().unwrap_or(Value::None));
            }
            let top = self.frames.len() - 1;
            let proto = self.frames[top].proto.clone();
            let globals = self.frames[top].globals.clone();
            if self.frames[top].ip >= proto.code.len() {
                // 代码走完等同于 return None。
                self.leave_frame(Value::None);
                if self.frames.len() < min_frames {
                    return Ok(Value::None);
                }
                continue;
            }
            let instr = proto.code[self.frames[top].ip];
            self.frames[top].ip += 1;
            self.executed += 1;
            let pos = instr.pos;
            match instr.op {
                Op::Const(index) => {
                    let value = proto.consts[index as usize].clone();
                    self.stack.push(value);
                }
                Op::Pop => {
                    self.stack.pop();
                }
                Op::Dup => {
                    let value = self.top(pos)?.clone();
                    self.stack.push(value);
                }
                Op::Swap => {
                    let len = self.stack.len();
                    if len < 2 {
                        return Err(self.fail(pos, RunError::RangeError("栈顶不足两个值".into())));
                    }
                    self.stack.swap(len - 1, len - 2);
                }
                Op::LoadLocal(slot) => {
                    let value = self.frames[top].slots[slot as usize].clone();
                    self.stack.push(value);
                }
                Op::StoreLocal(slot) => {
                    let value = self.pop(pos)?;
                    self.frames[top].slots[slot as usize] = value;
                }
                Op::LoadName(index) => {
                    let key = &*proto.names[index as usize];
                    let found = globals.borrow().get(key).cloned().or_else(|| self.host.builtin(key));
                    match found {
                        Some(value) => self.stack.push(value),
                        None => return Err(self.fail(pos, RunError::NameError(key.to_string()))),
                    }
                }
                Op::StoreName(index) => {
                    let value = self.pop(pos)?;
                    let key = &*proto.names[index as usize];
                    globals.borrow_mut().insert(key.into(), value);
                }
                Op::GetAttr(index) => {
                    let key = &*proto.names[index as usize];
                    let receiver = self.pop(pos)?;
                    match receiver.get_attr(key) {
                        Some(value) => self.stack.push(value),
                        None => return Err(self.fail(pos, RunError::NameError(format!(
                            "{} 里没有成员 {key}", receiver.type_name())))),
                    }
                }
                Op::SetAttr(index) => {
                    let key = &*proto.names[index as usize];
                    let value = self.pop(pos)?;
                    let receiver = self.pop(pos)?;
                    receiver.set_attr(key, value).map_err(|error| self.fail(pos, error))?;
                }
                Op::GetIndex => {
                    let index = self.pop(pos)?;
                    let object = self.pop(pos)?;
                    let value = object.index_get(&index).map_err(|error| self.fail(pos, error))?;
                    self.stack.push(value);
                }
                Op::SetIndex => {
                    let value = self.pop(pos)?;
                    let index = self.pop(pos)?;
                    let object = self.pop(pos)?;
                    object.index_set(&index, value).map_err(|error| self.fail(pos, error))?;
                }
                Op::BuildList(count) => {
                    let count = count as usize;
                    if self.stack.len() < count {
                        return Err(self.fail(pos, RunError::RangeError("构造列表的操作数不足".into())));
                    }
                    let start = self.stack.len() - count;
                    let items = self.stack.drain(start..).collect();
                    self.stack.push(Value::list(items));
                }
                Op::JoinStr(count) => {
                    let count = count as usize;
                    if self.stack.len() < count {
                        return Err(self.fail(pos, RunError::RangeError("拼接字符串的操作数不足".into())));
                    }
                    let start = self.stack.len() - count;
                    let parts: Vec<Value> = self.stack.drain(start..).collect();
                    let mut text = String::new();
                    for part in parts {
                        text.push_str(&part.text_of());
                    }
                    self.stack.push(Value::text(&text));
                }
                Op::Binary(op) => {
                    let right = self.pop(pos)?;
                    let left = self.pop(pos)?;
                    let value = binary_values(op, &left, &right).map_err(|error| self.fail(pos, error))?;
                    self.stack.push(value);
                }
                Op::Unary(op) => {
                    let value = self.pop(pos)?;
                    let result = unary_value(op, &value).map_err(|error| self.fail(pos, error))?;
                    self.stack.push(result);
                }
                Op::Jump(target) => self.frames[top].ip = target as usize,
                Op::JumpIfFalse(target) => {
                    let value = self.pop(pos)?;
                    if !value.truthy() {
                        self.frames[top].ip = target as usize;
                    }
                }
                Op::JumpIfTrue(target) => {
                    let value = self.pop(pos)?;
                    if value.truthy() {
                        self.frames[top].ip = target as usize;
                    }
                }
                Op::JumpIfFalsePeek(target) => {
                    if !self.top(pos)?.truthy() {
                        self.frames[top].ip = target as usize;
                    }
                }
                Op::JumpIfTruePeek(target) => {
                    if self.top(pos)?.truthy() {
                        self.frames[top].ip = target as usize;
                    }
                }
                Op::IterStart => {
                    let iterable = self.pop(pos)?;
                    let items = elements_of(&iterable).map_err(|error| self.fail(pos, error))?;
                    self.frames[top].iters.push((items, 0));
                }
                Op::IterNext { end } => {
                    let next = {
                        let frame = &self.frames[top];
                        frame.iters.last().map(|(items, cursor)| items.get(*cursor).cloned())
                    };
                    match next {
                        Some(Some(value)) => {
                            self.frames[top].iters.last_mut().expect("上一行刚确认迭代器栈非空").1 += 1;
                            self.stack.push(value);
                        }
                        Some(None) => {
                            self.frames[top].iters.pop();
                            self.frames[top].ip = end as usize;
                        }
                        None => return Err(self.fail(pos, RunError::RangeError("迭代器栈空了".into()))),
                    }
                }
                Op::IterPop => {
                    self.frames[top].iters.pop();
                }
                Op::MakeFunc(index) => {
                    let child = proto.protos[index as usize].clone();
                    let state = RefCell::new(FuncState::fresh(child.arity));
                    self.stack.push(Value::Func(Rc::new(Function {
                        proto: child,
                        globals: globals.clone(),
                        state,
                    })));
                }
                Op::ImportModule(index) => {
                    let path = proto.names[index as usize].to_string();
                    let package = self.package_of(&proto.source);
                    let value = self.require(&path, &package).map_err(|error| self.fail(pos, error))?;
                    self.stack.push(value);
                }
                Op::ImportFrom { source, target } => {
                    let source_text = proto.names[source as usize].to_string();
                    let bind = proto.names[target as usize].to_string();
                    let package = self.package_of(&proto.source);
                    let path = resolve_source(&source_text, &package);
                    let module = self.require(&path, &package).map_err(|error| self.fail(pos, error))?;
                    let value = match module.get_attr(&bind) {
                        Some(value) => value,
                        // 名字不在来源模块的绑定里，就按子模块再解析一次（from . import "routes"）。
                        None => {
                            let child = if path.is_empty() { bind.clone() } else { format!("{path}.{bind}") };
                            self.require(&child, &package)?
                        }
                    };
                    self.stack.push(value);
                }
                Op::Call { argc, kwset } => {
                    let (args, kwargs) = self.take_arguments(&proto, argc, kwset, pos)?;
                    let callee = self.pop(pos)?;
                    self.dispatch_call(callee, args, kwargs, pos)?;
                }
                Op::CallMethod { name, argc, kwset } => {
                    let key = proto.names[name as usize].to_string();
                    let (args, kwargs) = self.take_arguments(&proto, argc, kwset, pos)?;
                    let receiver = self.pop(pos)?;
                    self.dispatch_method(receiver, &key, args, kwargs, pos)?;
                }
                Op::Return => {
                    let value = self.pop(pos)?;
                    self.leave_frame(value.clone());
                    if self.frames.len() < min_frames {
                        return Ok(value);
                    }
                }
            }
        }
    }

    /// 值栈顶。
    fn top(&self, pos: Pos) -> Result<&Value, RunError> {
        self.stack.last()
            .ok_or_else(|| RunError::RangeError(format!("{}:{} 处值栈为空", pos.line, pos.col)))
    }

    /// 弹栈顶。
    fn pop(&mut self, pos: Pos) -> Result<Value, RunError> {
        self.stack.pop()
            .ok_or_else(|| RunError::RangeError(format!("{}:{} 处值栈为空", pos.line, pos.col)))
    }

    /// 给运行期错误补上「哪个模块哪一行哪个函数」。
    fn fail(&self, pos: Pos, error: RunError) -> RunError {
        let (source, name) = self.frames.last()
            .map(|frame| (frame.proto.source.to_string(), frame.proto.name.to_string()))
            .unwrap_or_else(|| ("?".to_string(), "?".to_string()));
        let tail = format!("（{source}:{}:{} 于 {name}）", pos.line, pos.col);
        match error {
            RunError::NameError(text) => RunError::NameError(text + &tail),
            RunError::TypeError(text) => RunError::TypeError(text + &tail),
            RunError::RangeError(text) => RunError::RangeError(text + &tail),
            RunError::Unsupported(text) => RunError::Unsupported(text + &tail),
            other => other,
        }
    }

    /// 收帧并把返回值放到调用点该在的位置。
    fn leave_frame(&mut self, value: Value) {
        let frame = self.frames.pop().expect("返回时栈里必须有帧");
        self.stack.truncate(frame.base);
        self.stack.push(value);
    }

    /// 压一个新帧。
    fn push_frame(&mut self, proto: Rc<Proto>, globals: Globals, args: Vec<Value>) -> Result<(), RunError> {
        if self.frames.len() >= MAX_FRAMES {
            return Err(RunError::StackOverflow);
        }
        let base = self.stack.len();
        let mut slots = vec![Value::None; proto.locals.len()];
        for (slot, value) in slots.iter_mut().zip(args) {
            *slot = value;
        }
        self.frames.push(Frame { proto, globals, slots, base, ip: 0, iters: Vec::new() });
        Ok(())
    }

    /// 实参绑定：位置参按序入槽，关键字参按形参名入槽。
    fn bind_arguments(&self, function: &Function, args: Vec<Value>, kwargs: Vec<(Box<str>, Value)>)
        -> Result<Vec<Value>, RunError> {
        let arity = function.proto.arity;
        if args.len() > arity {
            return Err(RunError::TypeError(format!(
                "{} 有 {arity} 个形参，位置实参给了 {} 个", function.proto.name, args.len())));
        }
        let mut slots: Vec<Option<Value>> = vec![None; arity];
        for (index, value) in args.into_iter().enumerate() {
            slots[index] = Some(value);
        }
        for (key, value) in kwargs {
            let position = function.proto.params.iter().position(|param| param.as_ref() == &*key);
            match position {
                Some(index) if slots[index].is_none() => slots[index] = Some(value),
                Some(index) => return Err(RunError::TypeError(format!(
                    "形参 {} 被传了两次", function.proto.params[index]))),
                None => return Err(RunError::TypeError(format!(
                    "{} 没有名为 {key} 的形参", function.proto.name))),
            }
        }
        if let Some(missing) = slots.iter().position(Option::is_none) {
            return Err(RunError::TypeError(format!(
                "{} 缺实参：{}", function.proto.name, function.proto.params[missing])));
        }
        Ok(slots.into_iter().map(|slot| slot.expect("上面已确认全部填上")).collect())
    }

    /// 调用一个油饼函数，返回 Some 表示已经算完（走的是本机码）。
    fn call_function(&mut self, function: Rc<Function>, args: Vec<Value>, kwargs: Vec<(Box<str>, Value)>)
        -> Result<Option<Value>, RunError> {
        let bound = self.bind_arguments(&function, args.clone(), kwargs)?;
        if self.jit_on {
            if let Some(value) = self.tier_and_try_jit(&function, &bound) {
                return Ok(Some(value));
            }
        }
        self.push_frame(function.proto.clone(), function.globals.clone(), bound)?;
        Ok(None)
    }

    /// 分层计数 + 类型反馈 + 达标编译；返回 Some 表示本次用本机码算完。
    fn tier_and_try_jit(&mut self, function: &Rc<Function>, args: &[Value]) -> Option<Value> {
        let numbers: Vec<i64> = args.iter().filter_map(|value| match value {
            Value::Int(number) => Some(*number),
            _ => None,
        }).collect();
        let pure_int = numbers.len() == args.len();

        let entry = {
            let mut state = function.state.borrow_mut();
            state.calls = state.calls.wrapping_add(1);
            for (seen, value) in state.types.iter_mut().zip(args) {
                *seen = if *seen == 0 || *seen == value.tag() { value.tag() } else { tier::MIXED };
            }
            if state.blacklisted {
                return None;
            }
            if !pure_int {
                // 类型破供：已有的机器码不再进，本函数永久退回解释执行。
                if state.jit.is_some() {
                    state.jit = None;
                    state.blacklisted = true;
                }
                return None;
            }
            if state.jit.is_none()
                && state.calls >= tier::CALL_THRESHOLD
                && state.types.iter().all(|seen| *seen == crate::value::tag::INT)
                && tier::eligible(&function.proto)
            {
                match jit::compile(&function.proto) {
                    Ok(entry) => {
                        self.jit_compiles += 1;
                        state.jit = Some(entry);
                    }
                    Err(_) => {
                        // 编不出来就当纯解释函数，后续不再重复尝试。
                        state.blacklisted = true;
                        return None;
                    }
                }
            }
            state.jit
        };
        entry.map(|pointer| {
            self.jit_calls += 1;
            Value::Int(pointer(numbers.as_ptr()))
        })
    }

    /// 统一调用出口：油饼函数压帧，宿主函数就地求值。
    fn dispatch_call(&mut self, callee: Value, args: Vec<Value>, kwargs: Vec<(Box<str>, Value)>, pos: Pos)
        -> Result<(), RunError> {
        match callee {
            Value::Func(function) => {
                match self.call_function(function, args, kwargs) {
                    Ok(Some(value)) => { self.stack.push(value); Ok(()) }
                    Ok(None) => Ok(()),
                    Err(error) => Err(self.fail(pos, error)),
                }
            }
            Value::Native(native) => {
                let native = native.clone();
                let value = native(self, args, kwargs).map_err(|error| self.fail(pos, error))?;
                self.stack.push(value);
                Ok(())
            }
            other => Err(self.fail(pos, RunError::TypeError(format!("{} 不可调用", other.type_name())))),
        }
    }

    /// 方法调用：内置方法优先，其次宿主对象方法，最后把属性当可调用值。
    fn dispatch_method(&mut self, receiver: Value, name: &str, args: Vec<Value>,
        kwargs: Vec<(Box<str>, Value)>, pos: Pos) -> Result<(), RunError> {
        if let Some(method) = receiver.builtin_method(name) {
            let value = self.run_builtin(method, &receiver, args, kwargs)
                .map_err(|error| self.fail(pos, error))?;
            self.stack.push(value);
            return Ok(());
        }
        if let Value::Obj(object) = &receiver {
            let native = object.borrow().methods.get(name).cloned();
            if let Some(native) = native {
                let mut all = vec![receiver.clone()];
                all.extend(args);
                let value = native(self, all, kwargs).map_err(|error| self.fail(pos, error))?;
                self.stack.push(value);
                return Ok(());
            }
        }
        match receiver.get_attr(name) {
            Some(callee) => self.dispatch_call(callee, args, kwargs, pos),
            None => Err(self.fail(pos, RunError::NameError(format!(
                "{} 里没有方法或成员 {name}", receiver.type_name())))),
        }
    }

    /// 取出栈上的实参：先位置参，后按 kwset 顺序压入的关键字参值。
    fn take_arguments(&mut self, proto: &Rc<Proto>, argc: u16, kwset: u32, pos: Pos)
        -> Result<(Vec<Value>, Vec<(Box<str>, Value)>), RunError> {
        let keys: Vec<Box<str>> = if kwset == u32::MAX {
            Vec::new()
        } else {
            proto.kwsets[kwset as usize].clone()
        };
        let total = argc as usize + keys.len();
        if self.stack.len() < total {
            return Err(self.fail(pos, RunError::RangeError("调用实参不足".into())));
        }
        let start = self.stack.len() - total;
        let pieces: Vec<Value> = self.stack.drain(start..).collect();
        let (positional, kw_values) = pieces.split_at(argc as usize);
        let kwargs = keys.into_iter().zip(kw_values.iter().cloned()).collect();
        Ok((positional.to_vec(), kwargs))
    }

    /// 在一个给定命名空间里跑一个表达式原型，模板引擎与 `youbing eval` 用。
    ///
    /// 调用方负责保证 globals 是这个表达式的作用域（模板里就是渲染上下文）。
    pub fn eval_proto(&mut self, proto: Rc<Proto>, globals: Globals) -> Result<Value, RunError> {
        let entry = self.frames.len() + 1;
        self.push_frame(proto, globals, Vec::new())?;
        self.pump(entry)
    }

    /// 内置方法实现。
    fn run_builtin(&mut self, method: BuiltinMethod, receiver: &Value, args: Vec<Value>,
        kwargs: Vec<(Box<str>, Value)>) -> Result<Value, RunError> {
        if !kwargs.is_empty() {
            return Err(RunError::TypeError(format!("内置方法 {} 不接受关键字参", method.name())));
        }
        use BuiltinMethod as M;
        let want = |count: usize| -> Result<(), RunError> {
            if args.len() == count {
                Ok(())
            } else {
                Err(RunError::TypeError(format!("{} 要 {count} 个实参，给了 {}", method.name(), args.len())))
            }
        };
        match method {
            M::Len => {
                want(0)?;
                Ok(Value::Int(match receiver {
                    Value::Str(text) => text.chars().count() as i64,
                    Value::List(items) => items.borrow().len() as i64,
                    Value::Dict(map) => map.borrow().pairs.len() as i64,
                    other => return Err(RunError::TypeError(format!("{} 没有长度", other.type_name()))),
                }))
            }
            M::Append => {
                want(1)?;
                receiver.as_list()?.borrow_mut().push(args[0].clone());
                Ok(Value::None)
            }
            M::Insert => {
                want(2)?;
                let list = receiver.as_list()?;
                let mut items = list.borrow_mut();
                let index = args[0].as_index(items.len())?;
                items.insert(index, args[1].clone());
                Ok(Value::None)
            }
            M::RemoveAt => {
                want(1)?;
                let list = receiver.as_list()?;
                let mut items = list.borrow_mut();
                let index = args[0].as_index(items.len())?;
                Ok(items.remove(index))
            }
            M::RemoveKey => {
                want(1)?;
                let map = receiver.as_dict()?;
                map.borrow_mut().remove(&args[0])
                    .ok_or_else(|| RunError::NameError(format!("字典里没有键 {}", args[0].text_of())))
            }
            M::Pop => {
                want(0)?;
                receiver.as_list()?.borrow_mut().pop()
                    .ok_or_else(|| RunError::RangeError("空列表不能 pop".into()))
            }
            M::Reverse => {
                want(0)?;
                receiver.as_list()?.borrow_mut().reverse();
                Ok(Value::None)
            }
            M::Sort => {
                want(0)?;
                let list = receiver.as_list()?;
                let mut items = list.borrow_mut();
                for pair in items.windows(2) {
                    if pair[0].compare(&pair[1]).is_none() {
                        return Err(RunError::TypeError(format!(
                            "{} 与 {} 不能排序比较", pair[0].type_name(), pair[1].type_name())));
                    }
                }
                items.sort_by(|a, b| a.compare(b).unwrap_or(std::cmp::Ordering::Equal));
                Ok(Value::None)
            }
            M::Split => {
                want(1)?;
                let text = receiver.as_text()?;
                let sep = args[0].as_text()?;
                let parts = if sep.is_empty() {
                    text.chars().map(|ch| Value::text(&ch.to_string())).collect()
                } else {
                    text.split(sep.as_ref()).map(Value::text).collect()
                };
                Ok(Value::list(parts))
            }
            M::Join => {
                want(1)?;
                match receiver {
                    Value::Str(sep) => {
                        let items = args[0].as_list()?;
                        let joined = items.borrow().iter().map(Value::text_of).collect::<Vec<_>>().join(sep.as_ref());
                        Ok(Value::text(&joined))
                    }
                    Value::List(items) => {
                        let sep = args[0].as_text()?;
                        let joined = items.borrow().iter().map(Value::text_of).collect::<Vec<_>>().join(sep);
                        Ok(Value::text(&joined))
                    }
                    other => Err(RunError::TypeError(format!("{} 不能 join", other.type_name()))),
                }
            }
            M::Strip => { want(0)?; Ok(Value::text(receiver.as_text()?.trim())) }
            M::Replace => {
                want(2)?;
                let text = receiver.as_text()?;
                let from = args[0].as_text()?;
                let to = args[1].as_text()?;
                Ok(Value::text(&text.replace(from.as_ref(), to.as_ref())))
            }
            M::Upper => { want(0)?; Ok(Value::text(&receiver.as_text()?.to_uppercase())) }
            M::Lower => { want(0)?; Ok(Value::text(&receiver.as_text()?.to_lowercase())) }
            M::StartsWith => {
                want(1)?;
                Ok(Value::Bool(receiver.as_text()?.starts_with(args[0].as_text()?.as_ref())))
            }
            M::EndsWith => {
                want(1)?;
                Ok(Value::Bool(receiver.as_text()?.ends_with(args[0].as_text()?.as_ref())))
            }
            M::Contains => {
                want(1)?;
                Ok(Value::Bool(match receiver {
                    Value::Str(text) => text.contains(args[0].as_text()?.as_ref()),
                    Value::List(items) => items.borrow().iter().any(|item| item.equals(&args[0])),
                    Value::Dict(map) => map.borrow().contains(&args[0]),
                    other => return Err(RunError::TypeError(format!("{} 不能 contains", other.type_name()))),
                }))
            }
            M::Get => {
                if args.is_empty() || args.len() > 2 {
                    return Err(RunError::TypeError(format!("get 要 1~2 个实参，给了 {}", args.len())));
                }
                let map = receiver.as_dict()?;
                Ok(map.borrow().find(&args[0]).cloned().or_else(|| args.get(1).cloned())
                    .unwrap_or(Value::None))
            }
            M::Set => {
                want(2)?;
                receiver.as_dict()?.borrow_mut().insert(args[0].clone(), args[1].clone());
                Ok(Value::None)
            }
            M::Keys => {
                want(0)?;
                Ok(Value::list(receiver.as_dict()?.borrow().pairs.iter().map(|(k, _)| k.clone()).collect()))
            }
            M::Values => {
                want(0)?;
                Ok(Value::list(receiver.as_dict()?.borrow().pairs.iter().map(|(_, v)| v.clone()).collect()))
            }
            M::Items => {
                want(0)?;
                Ok(Value::list(receiver.as_dict()?.borrow().pairs.iter()
                    .map(|(k, v)| Value::list(vec![k.clone(), v.clone()]))
                    .collect()))
            }
        }
    }

    /// 一个模块所属的包：目录包返回自己，普通模块返回上一层。
    fn package_of(&self, module: &str) -> String {
        if self.packages.contains(module) {
            return module.to_string();
        }
        match module.rsplit_once('.') {
            Some((head, _)) => head.to_string(),
            None => String::new(),
        }
    }

    /// 按导入路径取模块，油饼模块带循环保护。
    fn require(&mut self, path: &str, package: &str) -> Result<Value, RunError> {
        if let Some(module) = self.loaded.get(path) {
            return Ok(module.clone());
        }
        if let Some(module) = self.host.module(path) {
            return Ok(module);
        }
        let (file, is_package) = self.host.resolve_module_file(path, package)
            .ok_or_else(|| RunError::ModuleNotFound(path.to_string()))?;
        // 目录包（<名字>/init.yb）用包名登记，包内 `from . import x` 才落回自己。
        if is_package {
            self.packages.insert(path.to_string());
        }
        let module_id = path.to_string();
        let text = read_source(&file)?;
        let module = parse_file(&module_id, &text).map_err(RunError::from)?;
        let proto = compile_module(&module).map_err(RunError::from)?;
        let globals = self.fresh_globals(&module_id);
        let handle = Value::Yb(Rc::new(YbModule { name: module_id.clone().into(), globals: globals.clone() }));
        // 先登记再执行，循环导入时第二次 require 拿到的就是这份活命名空间。
        self.loaded.insert(module_id, handle.clone());
        let entry = self.frames.len() + 1;
        self.push_frame(proto, globals, Vec::new())?;
        self.pump(entry)?;
        Ok(handle)
    }
}

/// 一元运算。
fn unary_value(op: UnOp, value: &Value) -> Result<Value, RunError> {
    match (op, value) {
        (UnOp::Neg, Value::Int(number)) => Ok(Value::Int(-number)),
        (UnOp::Neg, Value::Float(float)) => Ok(Value::Float(-float)),
        (UnOp::Neg, other) => Err(RunError::TypeError(format!("负号用不了 {}", other.type_name()))),
        (UnOp::Not, _) => Ok(Value::Bool(!value.truthy())),
    }
}

/// 整数、浮点、字符串与列表的二元运算。
fn binary_values(op: BinOp, left: &Value, right: &Value) -> Result<Value, RunError> {
    use BinOp as B;
    match op {
        B::Add => match (left, right) {
            (Value::Str(a), Value::Str(b)) => Ok(Value::text(&format!("{a}{b}"))),
            (Value::List(a), Value::List(b)) => {
                let mut merged = a.borrow().clone();
                merged.extend(b.borrow().iter().cloned());
                Ok(Value::list(merged))
            }
            _ => arithmetic(op, left, right),
        },
        B::Eq => Ok(Value::Bool(left.equals(right))),
        B::Ne => Ok(Value::Bool(!left.equals(right))),
        B::Lt | B::Le | B::Gt | B::Ge => {
            let ordering = left.compare(right).ok_or_else(|| RunError::TypeError(format!(
                "{} 与 {} 不能比较大小", left.type_name(), right.type_name())))?;
            use std::cmp::Ordering::*;
            Ok(Value::Bool(match op {
                B::Lt => ordering == Less,
                B::Le => ordering != Greater,
                B::Gt => ordering == Greater,
                _ => ordering != Less,
            }))
        }
        B::Sub | B::Mul | B::Div | B::Mod => arithmetic(op, left, right),
    }
}

/// 数值运算：Int 保持 Int，混入 Float 走 Float，除零报错。
fn arithmetic(op: BinOp, left: &Value, right: &Value) -> Result<Value, RunError> {
    use BinOp as B;
    if op == B::Mul {
        let pair = (left, right);
        if let (Value::Str(text), Value::Int(count)) | (Value::Int(count), Value::Str(text)) = pair {
            if *count < 0 {
                return Err(RunError::RangeError("字符串重复次数不能为负".into()));
            }
            return Ok(Value::text(&text.repeat(*count as usize)));
        }
    }
    let as_int = |value: &Value| match value {
        Value::Int(number) => Some(*number),
        Value::Bool(flag) => Some(*flag as i64),
        _ => None,
    };
    let as_float = |value: &Value| match value {
        Value::Float(float) => Some(*float),
        _ => None,
    };
    if let (Some(a), Some(b)) = (as_int(left), as_int(right)) {
        return match op {
            B::Add => Ok(Value::Int(a.wrapping_add(b))),
            B::Sub => Ok(Value::Int(a.wrapping_sub(b))),
            B::Mul => Ok(Value::Int(a.wrapping_mul(b))),
            B::Div => {
                if b == 0 {
                    return Err(RunError::RangeError("除数为 0".into()));
                }
                if a.rem_euclid(b) == 0 {
                    Ok(Value::Int(a.wrapping_div(b)))
                } else {
                    Ok(Value::Float(a as f64 / b as f64))
                }
            }
            B::Mod => {
                if b == 0 {
                    return Err(RunError::RangeError("取模的除数为 0".into()));
                }
                Ok(Value::Int(a.rem_euclid(b)))
            }
            other => Err(RunError::Unsupported(format!("{other:?} 不是算术运算符"))),
        };
    }
    let fa = as_float(left).or_else(|| as_int(left).map(|value| value as f64));
    let fb = as_float(right).or_else(|| as_int(right).map(|value| value as f64));
    match (fa, fb) {
        (Some(a), Some(b)) => match op {
            B::Add => Ok(Value::Float(a + b)),
            B::Sub => Ok(Value::Float(a - b)),
            B::Mul => Ok(Value::Float(a * b)),
            B::Div => {
                if b == 0.0 {
                    return Err(RunError::RangeError("除数为 0".into()));
                }
                Ok(Value::Float(a / b))
            }
            B::Mod => {
                if b == 0.0 {
                    return Err(RunError::RangeError("取模的除数为 0".into()));
                }
                Ok(Value::Float(a % b))
            }
            other => Err(RunError::Unsupported(format!("{other:?} 不是算术运算符"))),
        },
        _ => Err(RunError::TypeError(format!(
            "{} {} {} 算不了", left.type_name(), op.text(), right.type_name()))),
    }
}

/// 可迭代值展开成元素表，for 循环与模板 `{% for %}` 共用。
pub fn elements_of(value: &Value) -> Result<Vec<Value>, RunError> {
    Ok(match value {
        Value::List(items) => items.borrow().clone(),
        Value::Str(text) => text.chars().map(|ch| Value::text(&ch.to_string())).collect(),
        Value::Dict(map) => map.borrow().pairs.iter().map(|(k, _)| k.clone()).collect(),
        other => return Err(RunError::TypeError(format!("{} 不能迭代", other.type_name()))),
    })
}

/// 把 `.` / `..` / `app` 这类来源写成绝对模块名。
fn resolve_source(source: &str, package: &str) -> String {
    if !source.starts_with('.') {
        return source.to_string();
    }
    let depth = source.chars().take_while(|ch| *ch == '.').count();
    let tail = &source[depth..];
    let base = if depth == 1 {
        package.to_string()
    } else {
        let parts: Vec<&str> = package.split('.').collect();
        let keep = parts.len().saturating_sub(depth - 1);
        parts.iter().copied().take(keep).collect::<Vec<_>>().join(".")
    };
    if tail.is_empty() || base.is_empty() {
        format!("{base}{tail}")
    } else {
        format!("{base}.{tail}")
    }
}

/// 读源码，兼容 UTF-8 BOM。
fn read_source(path: &Path) -> Result<String, RunError> {
    let bytes = std::fs::read(path)
        .map_err(|error| RunError::ModuleNotFound(format!("{}（{error}）", path.display())))?;
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    String::from_utf8(body.to_vec())
        .map_err(|_| RunError::Unsupported(format!("{} 不是合法的 UTF-8 源码", path.display())))
}

impl From<CompileError> for RunError {
    fn from(error: CompileError) -> RunError {
        RunError::Unsupported(error.to_string())
    }
}

/// 内置方法里反复用到的取值小工具。
trait ValueHelpers {
    fn as_list(&self) -> Result<Rc<RefCell<Vec<Value>>>, RunError>;
    fn as_dict(&self) -> Result<Rc<RefCell<crate::value::DictData>>, RunError>;
    fn as_text(&self) -> Result<Rc<str>, RunError>;
    fn as_index(&self, len: usize) -> Result<usize, RunError>;
}

impl ValueHelpers for Value {
    fn as_list(&self) -> Result<Rc<RefCell<Vec<Value>>>, RunError> {
        match self {
            Value::List(items) => Ok(items.clone()),
            other => Err(RunError::TypeError(format!("这里要列表，给的是 {}", other.type_name()))),
        }
    }

    fn as_dict(&self) -> Result<Rc<RefCell<crate::value::DictData>>, RunError> {
        match self {
            Value::Dict(map) => Ok(map.clone()),
            other => Err(RunError::TypeError(format!("这里要字典，给的是 {}", other.type_name()))),
        }
    }

    fn as_text(&self) -> Result<Rc<str>, RunError> {
        match self {
            Value::Str(text) => Ok(text.clone()),
            other => Ok(Rc::from(other.text_of().as_str())),
        }
    }

    fn as_index(&self, len: usize) -> Result<usize, RunError> {
        match self {
            Value::Int(number) => {
                let slot = if *number >= 0 {
                    *number as usize
                } else {
                    let back = number.checked_neg().map(|value| value as usize)
                        .ok_or_else(|| RunError::RangeError(format!("下标 {number} 越界")))?;
                    len.checked_sub(back)
                        .ok_or_else(|| RunError::RangeError(format!("下标 {number} 超出长度 {len}")))?
                };
                if slot < len {
                    Ok(slot)
                } else {
                    Err(RunError::RangeError(format!("下标 {number} 超出长度 {len}")))
                }
            }
            other => Err(RunError::TypeError(format!("下标要整数，给的是 {}", other.type_name()))),
        }
    }
}
