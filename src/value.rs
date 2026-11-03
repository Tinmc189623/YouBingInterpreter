//! 值模型：16 字节左右的 tagged enum，堆上类型统一走 Rc 引用计数。
//! tag() 供分层升级的类型反馈使用；相等/比较/下标/属性都在这里定义，VM 与 JIT 共用同一套语义。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::any::Any;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::bytecode::Proto;
use crate::error::RunError;
use crate::vm::Vm;

/// 宿主函数：可以拿到 `&mut Vm`，因此宿主回调里能再调油饼函数（例如 app.run 派发路由）。
pub type Native = Rc<dyn Fn(&mut Vm, Vec<Value>, Vec<(Box<str>, Value)>) -> Result<Value, RunError>>;

/// JIT 后的本机代码入口：一个指针装着全部整型实参，返回值也是整数。
pub type JitFn = extern "system" fn(*const i64) -> i64;

/// 模块级命名空间，函数对象带着它走，保证定义处的可见性。
pub type Globals = Rc<RefCell<HashMap<Box<str>, Value>>>;

/// 分层升级需要的运行期状态：调用计数、实参类型位图、已编译入口与黑名单。
pub struct FuncState {
    pub calls: u32,
    /// 每个形参位见过的类型标签集合，全为 INT 才允许进 JIT。
    pub types: Vec<u8>,
    pub jit: Option<JitFn>,
    /// 类型反馈被打破后置真，本函数不再尝试编译，只走解释。
    pub blacklisted: bool,
}

impl FuncState {
    /// 初始状态。
    pub fn fresh(arity: usize) -> FuncState {
        FuncState { calls: 0, types: vec![0; arity], jit: None, blacklisted: false }
    }
}

/// 油饼值。
#[derive(Clone)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<str>),
    List(Rc<RefCell<Vec<Value>>>),
    Dict(Rc<RefCell<DictData>>),
    Func(Rc<Function>),
    Native(Native),
    Obj(Rc<RefCell<HostObj>>),
    Module(Rc<HostModule>),
    /// 油饼模块：命名空间是活的，导入方看到的永远是最新绑定。
    Yb(Rc<YbModule>),
}

/// 字节码函数。
pub struct Function {
    pub proto: Rc<Proto>,
    pub globals: Globals,
    pub state: RefCell<FuncState>,
}

/// 油饼模块（一个 .yb 文件跑完后的活命名空间）。
pub struct YbModule {
    pub name: Box<str>,
    pub globals: Globals,
}

/// 宿主模块：import 回来的 std / xorweb / os，成员表只读。
pub struct HostModule {
    pub name: &'static str,
    pub members: HashMap<&'static str, Value>,
}

/// 宿主对象：字段 + 方法 + 一份私有状态（downcast 用）。
pub struct HostObj {
    pub type_name: &'static str,
    pub fields: HashMap<Box<str>, Value>,
    pub methods: HashMap<&'static str, Native>,
    pub opaque: Option<Rc<dyn Any>>,
}

/// 有序字典：pairs 保插入序（模板渲染和 JSON 输出要稳定），index 负责 O(1) 取键。
pub struct DictData {
    pub pairs: Vec<(Value, Value)>,
    index: HashMap<DictKey, usize>,
}

/// 字典键：字符串/整数/布尔/None 直接编码，其余按对象身份兜底。
#[derive(PartialEq, Eq, Hash)]
enum DictKey {
    Nil,
    Flag(bool),
    Whole(i64),
    Real(u64),
    Text(Rc<str>),
    Ptr(usize),
}

/// 类型标签，和 `tag()` 一一对应，JIT 的类型掩码用它做单态判定。
pub mod tag {
    pub const NONE: u8 = 0;
    pub const BOOL: u8 = 1;
    pub const INT: u8 = 2;
    pub const FLOAT: u8 = 3;
    pub const TEXT: u8 = 4;
    pub const LIST: u8 = 5;
    pub const DICT: u8 = 6;
    pub const FUNC: u8 = 7;
    pub const NATIVE: u8 = 8;
    pub const OBJ: u8 = 9;
    pub const MODULE: u8 = 10;
    pub const YB: u8 = 11;
}

impl DictData {
    /// 空字典。
    pub fn new() -> Self {
        DictData { pairs: Vec::new(), index: HashMap::new() }
    }

    /// 按键取值。
    pub fn find(&self, key: &Value) -> Option<&Value> {
        self.index.get(&DictKey::of(key)).map(|slot| &self.pairs[*slot].1)
    }

    /// 写入或覆盖一个键，覆盖时保持原有位置。
    pub fn insert(&mut self, key: Value, value: Value) {
        let dict_key = DictKey::of(&key);
        match self.index.get(&dict_key) {
            Some(slot) => {
                let slot = *slot;
                self.pairs[slot].0 = key;
                self.pairs[slot].1 = value;
            }
            None => {
                self.index.insert(dict_key, self.pairs.len());
                self.pairs.push((key, value));
            }
        }
    }

    /// 删除一个键，返回被删的值。
    pub fn remove(&mut self, key: &Value) -> Option<Value> {
        let dict_key = DictKey::of(key);
        let slot = self.index.remove(&dict_key)?;
        let removed = self.pairs.remove(slot);
        // Vec::remove 让后面的下标整体前移，索引必须重建。
        for (index, (key, _)) in self.pairs.iter().enumerate().skip(slot) {
            self.index.insert(DictKey::of(key), index);
        }
        Some(removed.1)
    }

    /// 键是否存在。
    pub fn contains(&self, key: &Value) -> bool {
        self.index.contains_key(&DictKey::of(key))
    }
}

impl Default for DictData {
    fn default() -> Self {
        Self::new()
    }
}

impl DictKey {
    /// 把油饼值折成可哈希的键。
    fn of(value: &Value) -> DictKey {
        match value {
            Value::None => DictKey::Nil,
            Value::Bool(flag) => DictKey::Flag(*flag),
            Value::Int(number) => DictKey::Whole(*number),
            // f64 不可哈希，按位模式存：同一个浮点值必然命中同一个槽。
            Value::Float(float) => DictKey::Real(float.to_bits()),
            Value::Str(text) => DictKey::Text(text.clone()),
            other => DictKey::Ptr(other.addr()),
        }
    }
}

impl Value {
    /// 类型标签，供 JIT 与类型反馈使用。
    pub fn tag(&self) -> u8 {
        match self {
            Value::None => tag::NONE,
            Value::Bool(_) => tag::BOOL,
            Value::Int(_) => tag::INT,
            Value::Float(_) => tag::FLOAT,
            Value::Str(_) => tag::TEXT,
            Value::List(_) => tag::LIST,
            Value::Dict(_) => tag::DICT,
            Value::Func(_) => tag::FUNC,
            Value::Native(_) => tag::NATIVE,
            Value::Obj(_) => tag::OBJ,
            Value::Module(_) => tag::MODULE,
            Value::Yb(_) => tag::YB,
        }
    }

    /// 类型名，报错文本里出现。
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::None => "None",
            Value::Bool(_) => "Bool",
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::Str(_) => "Str",
            Value::List(_) => "List",
            Value::Dict(_) => "Dict",
            Value::Func(_) => "Func",
            Value::Native(_) => "Native",
            Value::Obj(o) => o.borrow().type_name,
            Value::Module(_) | Value::Yb(_) => "Module",
        }
    }

    /// 对象身份，字典键兜底用。
    fn addr(&self) -> usize {
        match self {
            Value::List(inner) => Rc::as_ptr(inner) as usize,
            Value::Dict(inner) => Rc::as_ptr(inner) as usize,
            Value::Obj(inner) => Rc::as_ptr(inner) as usize,
            Value::Func(inner) => Rc::as_ptr(inner) as usize,
            Value::Native(inner) => Rc::as_ptr(inner) as *const u8 as usize,
            Value::Module(inner) => Rc::as_ptr(inner) as usize,
            Value::Yb(inner) => Rc::as_ptr(inner) as usize,
            _ => 0,
        }
    }

    /// 新建空字典。
    pub fn dict() -> Value {
        Value::Dict(Rc::new(RefCell::new(DictData::new())))
    }

    /// 新建列表。
    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(RefCell::new(items)))
    }

    /// 从字符串构造值。
    pub fn text(text: &str) -> Value {
        Value::Str(Rc::from(text))
    }

    /// 是否可调用。
    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Func(_) | Value::Native(_))
    }

    /// 真值判定：None 为假，0/空串/空表为假，其余为真。
    pub fn truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Bool(flag) => *flag,
            Value::Int(number) => *number != 0,
            Value::Float(float) => *float != 0.0,
            Value::Str(text) => !text.is_empty(),
            Value::List(items) => !items.borrow().is_empty(),
            Value::Dict(map) => !map.borrow().pairs.is_empty(),
            _ => true,
        }
    }

    /// 相等比较：数值跨 Int/Float 比较，其余同类才可能相等。
    pub fn equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::None, Value::None) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Bool(a), Value::Int(b)) | (Value::Int(b), Value::Bool(a)) => *a as i64 == *b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => (*a as f64) == *b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.equals(y))
            }
            (Value::Dict(a), Value::Dict(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                a.pairs.len() == b.pairs.len() && a.pairs.iter().all(|(k, v)| {
                    b.find(k).is_some_and(|found| found.equals(v))
                })
            }
            (Value::Func(a), Value::Func(b)) => Rc::ptr_eq(a, b),
            (Value::Obj(a), Value::Obj(b)) => Rc::ptr_eq(a, b),
            (Value::Module(a), Value::Module(b)) => Rc::ptr_eq(a, b),
            (Value::Yb(a), Value::Yb(b)) => Rc::ptr_eq(a, b),
            (Value::Native(a), Value::Native(b)) => Rc::as_ptr(a) == Rc::as_ptr(b),
            _ => false,
        }
    }

    /// 大小比较：整数/浮点/字符串可比，其余返回 None 让调用方报错。
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Int(a), Value::Float(b)) => Some((*a as f64).partial_cmp(b)?),
            (Value::Float(a), Value::Int(b)) => Some(a.partial_cmp(&(*b as f64))?),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Str(a), Value::Str(b)) => Some(a.as_ref().cmp(b.as_ref())),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// 输出文本：print 与 f 串插值都用它，容器递归时给字符串加引号。
    pub fn text_of(&self) -> String {
        match self {
            Value::None => "None".to_string(),
            Value::Bool(true) => "True".to_string(),
            Value::Bool(false) => "False".to_string(),
            Value::Int(number) => number.to_string(),
            Value::Float(float) => format_f64(*float),
            Value::Str(text) => text.to_string(),
            Value::List(items) => {
                let body = items.borrow().iter().map(Value::quoted).collect::<Vec<_>>().join(", ");
                format!("[{body}]")
            }
            Value::Dict(map) => {
                let body = map.borrow().pairs.iter()
                    .map(|(k, v)| format!("{}: {}", k.quoted(), v.text_of()))
                    .collect::<Vec<_>>().join(", ");
                format!("{{{body}}}")
            }
            Value::Func(f) => format!("<函数 {}>", f.proto.name),
            Value::Native(_) => "<宿主函数>".to_string(),
            Value::Obj(o) => format!("<对象 {}>", o.borrow().type_name),
            Value::Module(m) => format!("<模块 {}>", m.name),
            Value::Yb(m) => format!("<模块 {}>", m.name),
        }
    }

    /// 容器成员用的带引号形式，`repr` 内置函数也走它。
    pub fn quoted(&self) -> String {
        match self {
            Value::Str(text) => format!("\"{text}\""),
            other => other.text_of(),
        }
    }

    /// 内置方法查找：字符串/列表/字典的方法静态给出，返回枚举避免每次造闭包。
    pub fn builtin_method(&self, name: &str) -> Option<BuiltinMethod> {
        Some(match self {
            Value::Str(_) => match name {
                "len" => BuiltinMethod::Len,
                "split" => BuiltinMethod::Split,
                "join" => BuiltinMethod::Join,
                "strip" => BuiltinMethod::Strip,
                "replace" => BuiltinMethod::Replace,
                "upper" => BuiltinMethod::Upper,
                "lower" => BuiltinMethod::Lower,
                "starts_with" => BuiltinMethod::StartsWith,
                "ends_with" => BuiltinMethod::EndsWith,
                "contains" => BuiltinMethod::Contains,
                _ => return None,
            },
            Value::List(_) => match name {
                "len" => BuiltinMethod::Len,
                "append" => BuiltinMethod::Append,
                "insert" => BuiltinMethod::Insert,
                "remove" => BuiltinMethod::RemoveAt,
                "pop" => BuiltinMethod::Pop,
                "reverse" => BuiltinMethod::Reverse,
                "sort" => BuiltinMethod::Sort,
                "contains" => BuiltinMethod::Contains,
                "join" => BuiltinMethod::Join,
                _ => return None,
            },
            Value::Dict(_) => match name {
                "len" => BuiltinMethod::Len,
                "get" => BuiltinMethod::Get,
                "set" => BuiltinMethod::Set,
                "keys" => BuiltinMethod::Keys,
                "values" => BuiltinMethod::Values,
                "items" => BuiltinMethod::Items,
                "contains" => BuiltinMethod::Contains,
                "remove" => BuiltinMethod::RemoveKey,
                _ => return None,
            },
            _ => return None,
        })
    }

    /// 点号取属性：字典按键、宿主对象按字段再按方法、模块按成员或活命名空间。
    pub fn get_attr(&self, name: &str) -> Option<Value> {
        match self {
            Value::Dict(map) => map.borrow().find(&Value::text(name)).cloned(),
            Value::Obj(obj) => {
                let obj = obj.borrow();
                obj.fields.get(name).cloned()
                    .or_else(|| obj.methods.get(name).map(|native| Value::Native(native.clone())))
            }
            Value::Module(module) => module.members.get(name).cloned(),
            Value::Yb(module) => module.globals.borrow().get(name).cloned(),
            Value::List(items) => if name == "len" { Some(Value::Int(items.borrow().len() as i64)) } else { None },
            Value::Str(text) => if name == "len" { Some(Value::Int(text.chars().count() as i64)) } else { None },
            _ => None,
        }
    }

    /// 点号写属性：只允许字典与宿主对象字段。
    pub fn set_attr(&self, name: &str, value: Value) -> Result<(), RunError> {
        match self {
            Value::Dict(map) => { map.borrow_mut().insert(Value::text(name), value); Ok(()) }
            Value::Obj(obj) => { obj.borrow_mut().fields.insert(name.into(), value); Ok(()) }
            other => Err(RunError::TypeError(format!("{} 不能按属性赋值", other.type_name()))),
        }
    }

    /// 下标读：列表按整数下标，字典按键，字符串按字符。
    pub fn index_get(&self, index: &Value) -> Result<Value, RunError> {
        match (self, index) {
            (Value::List(items), Value::Int(number)) => {
                let items = items.borrow();
                let index = *number;
                slice_pos(index, items.len()).map(|slot| items[slot].clone())
                    .ok_or_else(|| RunError::RangeError(format!("下标 {index} 超出列表长度 {}", items.len())))
            }
            (Value::Dict(map), key) => map.borrow().find(key).cloned()
                .ok_or_else(|| RunError::NameError(format!("字典里没有键 {}", key.text_of()))),
            (Value::Str(text), Value::Int(number)) => {
                let chars: Vec<char> = text.chars().collect();
                let index = *number;
                let slot = slice_pos(index, chars.len()).ok_or_else(|| {
                    RunError::RangeError(format!("下标 {index} 超出字符串长度 {}", chars.len()))
                })?;
                Ok(Value::text(&chars[slot].to_string()))
            }
            (Value::List(_), other) => Err(RunError::TypeError(format!("列表下标要整数，给的是 {}", other.type_name()))),
            (other, index) => Err(RunError::TypeError(format!(
                "{} 不能按下标取，下标类型 {}", other.type_name(), index.type_name()))),
        }
    }

    /// 下标写：列表与字典。
    pub fn index_set(&self, index: &Value, value: Value) -> Result<(), RunError> {
        match (self, index) {
            (Value::List(items), Value::Int(number)) => {
                let mut items = items.borrow_mut();
                let index = *number;
                let slot = slice_pos(index, items.len()).ok_or_else(|| {
                    RunError::RangeError(format!("下标 {index} 超出列表长度 {}", items.len()))
                })?;
                items[slot] = value;
                Ok(())
            }
            (Value::Dict(map), key) => { map.borrow_mut().insert(key.clone(), value); Ok(()) }
            (other, index) => Err(RunError::TypeError(format!(
                "{} 不能按下标赋值，下标类型 {}", other.type_name(), index.type_name()))),
        }
    }
}

/// 支持负下标的取槽位，-1 是最后一个；越界返回 None。
fn slice_pos(index: i64, len: usize) -> Option<usize> {
    if index >= 0 {
        let slot = index as usize;
        if slot < len { Some(slot) } else { None }
    } else {
        let back = index.checked_neg()? as usize;
        len.checked_sub(back)
    }
}

/// 浮点打印：整数值的浮点也保留一位小数，避免和 Int 混起来。
fn format_f64(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text_of())
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text_of())
    }
}

/// 内置方法枚举，VM 里一个大 match 执行，省掉每次调用的闭包分配。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BuiltinMethod {
    Len, Append, Insert, RemoveAt, RemoveKey, Pop, Reverse, Sort,
    Split, Join, Strip, Replace, Upper, Lower, StartsWith, EndsWith, Contains,
    Get, Set, Keys, Values, Items,
}

impl BuiltinMethod {
    /// 方法名，报错信息用。
    pub fn name(self) -> &'static str {
        use BuiltinMethod::*;
        match self {
            Len => "len", Append => "append", Insert => "insert",
            RemoveAt => "remove", RemoveKey => "remove", Pop => "pop",
            Reverse => "reverse", Sort => "sort", Split => "split", Join => "join",
            Strip => "strip", Replace => "replace", Upper => "upper", Lower => "lower",
            StartsWith => "starts_with", EndsWith => "ends_with", Contains => "contains",
            Get => "get", Set => "set", Keys => "keys", Values => "values", Items => "items",
        }
    }
}

/// 构造宿主对象的小工具，host 模块反复用。
pub fn host_obj(type_name: &'static str) -> Value {
    Value::Obj(Rc::new(RefCell::new(HostObj {
        type_name,
        fields: HashMap::new(),
        methods: HashMap::new(),
        opaque: None,
    })))
}

/// 构造宿主模块。
pub fn host_module(name: &'static str, members: &[(&'static str, Value)]) -> Value {
    let mut map = HashMap::new();
    for (key, value) in members {
        map.insert(*key, value.clone());
    }
    Value::Module(Rc::new(HostModule { name, members: map }))
}

/// 给宿主对象挂方法。
pub fn with_method(value: &Value, name: &'static str, native: Native) -> Value {
    if let Value::Obj(obj) = value {
        obj.borrow_mut().methods.insert(name, native);
    }
    value.clone()
}
