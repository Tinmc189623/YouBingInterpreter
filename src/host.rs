//! 宿主绑定：内置函数、`std`、`os` 与 `xorweb` 模块，以及 .yb 模块的路径解析。
//!
//! 一切文件访问都被限制在项目根目录内：宿主 API 拿到的是脚本里写的相对路径，
//! 越出根目录（含 `..`）直接报错，避免解释器变成任意文件读取的跳板。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::error::RunError;
use crate::json;
use crate::value::{host_module, host_obj, with_method, Globals, Native, Value};
use crate::vm;
use crate::web::{self, WebState};
use crate::Vm;

/// 宿主：内置表 + 模块表 + Web 状态 + 项目根。
pub struct Host {
    pub root: PathBuf,
    /// 脚本参数（`youbing run 入口 -- 之后的内容`），`os.args` 原样给出。
    pub args: Vec<String>,
    builtins: HashMap<&'static str, Value>,
    modules: HashMap<&'static str, Value>,
    pub web: Rc<RefCell<WebState>>,
}

/// 把闭包装成宿主函数值，`web` 模块也用它注册成员。
pub fn host_native<F>(f: F) -> Value
where F: Fn(&mut Vm, Vec<Value>, Vec<(Box<str>, Value)>) -> Result<Value, RunError> + 'static {
    let function: Native = Rc::new(f);
    Value::Native(function)
}

impl Host {
    /// 以项目根与脚本参数建宿主。
    pub fn new(root: PathBuf, args: Vec<String>) -> Host {
        let web_state = Rc::new(RefCell::new(WebState::new(root.clone())));
        let builtins = build_builtins();
        let mut modules = HashMap::new();
        modules.insert("std", build_std(root.clone(), web_state.clone()));
        modules.insert("os", build_os(root.clone(), &args));
        modules.insert("xorweb", web::module(web_state.clone()));
        Host { root, args, builtins, modules, web: web_state }
    }

    /// 取内置函数值。
    pub fn builtin(&self, name: &str) -> Option<Value> {
        self.builtins.get(name).cloned()
    }

    /// 取宿主模块值。
    pub fn module(&self, path: &str) -> Option<Value> {
        self.modules.get(path).cloned()
    }

    /// 点号模块路径 → 磁盘文件。返回 (文件, 是否是目录包)。
    pub fn resolve_module_file(&self, path: &str, _package: &str) -> Option<(PathBuf, bool)> {
        if path.is_empty() {
            return None;
        }
        let mut dir = self.root.clone();
        let segments: Vec<&str> = path.split('.').collect();
        for (index, segment) in segments.iter().enumerate() {
            if segment.is_empty() || !segment.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
                return None;
            }
            if index + 1 == segments.len() {
                let plain = dir.join(format!("{segment}.yb"));
                if plain.is_file() {
                    return Some((plain, false));
                }
                let package = dir.join(segment).join("init.yb");
                if package.is_file() {
                    return Some((package, true));
                }
                return None;
            }
            dir = dir.join(segment);
        }
        None
    }

    /// 磁盘文件 → 模块 id：剥掉 .yb，目录包用目录名，相对根目录按点号拼。
    pub fn module_id_for_file(&self, path: &Path) -> String {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        let mut parts: Vec<String> = relative.components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy().to_string()),
                _ => None,
            })
            .collect();
        if let Some(last) = parts.last_mut() {
            if last.ends_with(".yb") {
                *last = last.trim_end_matches(".yb").to_string();
            }
            if last == "init" {
                parts.pop();
            }
        }
        if parts.is_empty() {
            return "main".to_string();
        }
        parts.join(".")
    }

    /// 把脚本给出的路径折到根目录内，越界就报错。
    pub fn resolve_path(&self, raw: &str) -> Result<PathBuf, RunError> {
        let input = Path::new(raw);
        let mut target = if input.is_absolute() { self.root.clone() } else { self.root.clone() };
        for component in input.components() {
            match component {
                Component::Normal(part) => target.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !target.pop() {
                        return Err(RunError::RangeError(format!("路径 {raw} 越出了项目根目录")));
                    }
                }
                _ => return Err(RunError::RangeError(format!("路径 {raw} 里有不允许的写法"))),
            }
        }
        if !target.starts_with(&self.root) {
            return Err(RunError::RangeError(format!("路径 {raw} 越出了项目根目录")));
        }
        Ok(target)
    }

    /// 当前秒级时间戳。
    pub fn now_secs() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    /// 秒 → "YYYY-MM-DD HH:MM:SS"（自研日历换算，避免为格式化再拉一个依赖）。
    pub fn format_datetime(secs: i64) -> String {
        let (days, seconds) = div_rem(secs, 86400);
        let (years, yday) = civil_from_days(days);
        let (month, day) = month_day(years, yday);
        let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
        format!("{years:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02}")
    }

    /// 秒 → "YYYY-MM-DD"。
    pub fn format_date(secs: i64) -> String {
        let days = div_rem(secs, 86400).0;
        let (years, yday) = civil_from_days(days);
        let (month, day) = month_day(years, yday);
        format!("{years:04}-{month:02}-{day:02}")
    }

    /// 自研的盐化散列：sha256 迭代 `rounds` 次，用于口令存储与会话令牌。
    ///
    /// 这是"能挡住字典撞库"的强度，不是替代 bcrypt 的正式 KDF，写在这里是因为
    /// 解释器不引入 C 依赖；换部署环境时可以用 std.hash_rounds 提高轮数。
    pub fn digest(text: &str, salt: &str, rounds: usize) -> String {
        let mut hash = String::new();
        for round in 0..rounds.max(1) {
            let mut hasher = Sha256::new();
            hasher.update(salt.as_bytes());
            hasher.update(hash.as_bytes());
            hasher.update(text.as_bytes());
            hasher.update(round.to_string().as_bytes());
            hash = hex_of(&hasher.finalize());
        }
        hash
    }
}

/// 内置函数表：语言自带的 print/exit/len/... 全部在这里。
fn build_builtins() -> HashMap<&'static str, Value> {
    let mut map: HashMap<&'static str, Value> = HashMap::new();
    let mut add = |name: &'static str, value: Value| { map.insert(name, value); };

    add("print", host_native(|_vm, args, kwargs| {
        let sep = kwargs.iter().find(|(key, _)| &**key == "sep")
            .map(|(_, value)| value.text_of()).unwrap_or_else(|| " ".to_string());
        let parts: Vec<String> = args.iter().map(Value::text_of).collect();
        println!("{}", parts.join(&sep));
        Ok(Value::None)
    }));

    add("exit", host_native(|_vm, args, _kwargs| {
        let code = match args.first() {
            None | Some(Value::None) => 0,
            Some(Value::Int(number)) => *number as i32,
            Some(Value::Str(text)) => text.parse::<i32>().unwrap_or(1),
            Some(other) => return Err(RunError::TypeError(format!("exit 的参数要整数，给的是 {}", other.type_name()))),
        };
        Err(RunError::Exit(code))
    }));

    add("len", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "len")?;
        Ok(Value::Int(length_of(value)?))
    }));

    add("str", host_native(|_vm, args, _kwargs| {
        Ok(Value::text(&args.first().map(Value::text_of).unwrap_or_else(|| "None".to_string())))
    }));

    add("repr", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "repr")?;
        Ok(Value::text(&value.quoted()))
    }));

    add("int", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "int")?;
        Ok(Value::Int(match value {
            Value::Int(number) => *number,
            Value::Bool(flag) => *flag as i64,
            Value::Float(float) => *float as i64,
            Value::Str(text) => text.trim().parse::<i64>()
                .map_err(|_| RunError::TypeError(format!("\"{text}\" 转不成整数")))?,
            other => return Err(RunError::TypeError(format!("{} 转不成整数", other.type_name()))),
        }))
    }));

    add("float", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "float")?;
        Ok(Value::Float(match value {
            Value::Float(float) => *float,
            Value::Int(number) => *number as f64,
            Value::Str(text) => text.trim().parse::<f64>()
                .map_err(|_| RunError::TypeError(format!("\"{text}\" 转不成浮点")))?,
            other => return Err(RunError::TypeError(format!("{} 转不成浮点", other.type_name()))),
        }))
    }));

    add("bool", host_native(|_vm, args, _kwargs| {
        Ok(Value::Bool(args.first().map(Value::truthy).unwrap_or(false)))
    }));

    add("range", host_native(|_vm, args, _kwargs| {
        let (start, stop, step) = match args.len() {
            1 => (0, to_int(want(&args, 0, "range")?)?, 1),
            2 => (to_int(want(&args, 0, "range")?)?, to_int(want(&args, 1, "range")?)?, 1),
            3 => (to_int(want(&args, 0, "range")?)?, to_int(want(&args, 1, "range")?)?,
                 to_int(want(&args, 2, "range")?)?),
            _ => return Err(RunError::TypeError("range 接 1 到 3 个参数".into())),
        };
        if step == 0 {
            return Err(RunError::RangeError("range 的步长不能是 0".into()));
        }
        let mut items = Vec::new();
        let mut current = start;
        while (step > 0 && current < stop) || (step < 0 && current > stop) {
            items.push(Value::Int(current));
            current += step;
            if items.len() > 1_000_000 {
                return Err(RunError::RangeError("range 太长，最多 100 万个元素".into()));
            }
        }
        Ok(Value::list(items))
    }));

    add("max", host_native(|_vm, args, _kwargs| fold_compare(&args, true)));
    add("min", host_native(|_vm, args, _kwargs| fold_compare(&args, false)));

    add("abs", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "abs")?;
        Ok(match value {
            Value::Int(number) => Value::Int(number.abs()),
            Value::Float(float) => Value::Float(float.abs()),
            other => return Err(RunError::TypeError(format!("abs 用不了 {}", other.type_name()))),
        })
    }));

    add("sum", host_native(|_vm, args, _kwargs| {
        let items = elements(want(&args, 0, "sum")?)?;
        let mut total = 0i64;
        for item in items {
            total += to_int(&item)?;
        }
        Ok(Value::Int(total))
    }));

    add("type", host_native(|_vm, args, _kwargs| {
        Ok(Value::text(want(&args, 0, "type")?.type_name()))
    }));

    add("list", host_native(|_vm, args, _kwargs| {
        match args.first() {
            None => Ok(Value::list(Vec::new())),
            Some(value) => Ok(Value::list(elements(value)?)),
        }
    }));

    add("keys", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "keys")?;
        Ok(match value {
            Value::Dict(map) => Value::list(map.borrow().pairs.iter().map(|(k, _)| k.clone()).collect()),
            other => return Err(RunError::TypeError(format!("keys 用不了 {}", other.type_name()))),
        })
    }));

    add("values", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "values")?;
        Ok(match value {
            Value::Dict(map) => Value::list(map.borrow().pairs.iter().map(|(_, v)| v.clone()).collect()),
            other => return Err(RunError::TypeError(format!("values 用不了 {}", other.type_name()))),
        })
    }));

    add("sorted", host_native(|vm, args, kwargs| {
        let source = want(&args, 0, "sorted")?;
        let mut items = elements(source)?;
        let key = kwargs.iter().find(|(name, _)| &**name == "key").map(|(_, value)| value.clone());
        let reverse = kwargs.iter().find(|(name, _)| &**name == "reverse")
            .map(|(_, value)| value.truthy()).unwrap_or(false);
        sort_values(vm, &mut items, key.as_ref(), reverse)?;
        Ok(Value::list(items))
    }));

    add("reverse", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "reverse")?;
        let mut items = elements(value)?;
        items.reverse();
        Ok(Value::list(items))
    }));

    add("enumerate", host_native(|_vm, args, _kwargs| {
        let value = want(&args, 0, "enumerate")?;
        let start = args.get(1).map(to_int).transpose()?.unwrap_or(0);
        let items = elements(value)?.into_iter().enumerate()
            .map(|(index, item)| Value::list(vec![Value::Int(start + index as i64), item]))
            .collect();
        Ok(Value::list(items))
    }));

    map
}

/// `std` 模块：文件、JSON、时间、散列、URL 编解码。
fn build_std(root: PathBuf, web_state: Rc<RefCell<WebState>>) -> Value {
    let host = root.clone();
    let mut members: Vec<(&'static str, Value)> = Vec::new();

    members.push(("read_file", host_native(move |_vm, args, kwargs| {
        let path = want(&args, 0, "read_file")?.text_of();
        let raw = kwargs.iter().any(|(name, _)| &**name == "raw");
        let file = pin_path(&host, &path)?;
        let bytes = std::fs::read(&file)
            .map_err(|error| RunError::Unsupported(format!("读取 {} 失败: {error}", file.display())))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(Value::text(if raw { &text } else { text.trim_start_matches('\u{feff}') }))
    })));

    members.push(("write_file", host_native(move |_vm, args, _kwargs| {
        let path = want(&args, 0, "write_file")?.text_of();
        let text = want(&args, 1, "write_file")?.text_of();
        let file = pin_path(&host, &path)?;
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut handle = std::fs::File::create(&file)
            .map_err(|error| RunError::Unsupported(format!("写入 {} 失败: {error}", file.display())))?;
        handle.write_all(text.as_bytes())
            .map_err(|error| RunError::Unsupported(format!("写入 {} 失败: {error}", file.display())))?;
        Ok(Value::Bool(true))
    })));

    members.push(("append_file", host_native(move |_vm, args, _kwargs| {
        let path = want(&args, 0, "append_file")?.text_of();
        let text = want(&args, 1, "append_file")?.text_of();
        let file = pin_path(&host, &path)?;
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut handle = std::fs::OpenOptions::new().create(true).append(true).open(&file)
            .map_err(|error| RunError::Unsupported(format!("追加 {} 失败: {error}", file.display())))?;
        handle.write_all(text.as_bytes())
            .map_err(|error| RunError::Unsupported(format!("追加 {} 失败: {error}", file.display())))?;
        Ok(Value::Bool(true))
    })));

    members.push(("delete_file", host_native(move |_vm, args, _kwargs| {
        let path = want(&args, 0, "delete_file")?.text_of();
        let file = pin_path(&host, &path)?;
        if file.is_dir() {
            std::fs::remove_dir_all(&file)
                .map_err(|error| RunError::Unsupported(format!("删除目录 {} 失败: {error}", file.display())))?;
        } else if file.exists() {
            std::fs::remove_file(&file)
                .map_err(|error| RunError::Unsupported(format!("删除 {} 失败: {error}", file.display())))?;
        }
        Ok(Value::Bool(true))
    })));

    members.push(("make_dir", host_native(move |_vm, args, _kwargs| {
        let path = want(&args, 0, "make_dir")?.text_of();
        let dir = pin_path(&host, &path)?;
        std::fs::create_dir_all(&dir)
            .map_err(|error| RunError::Unsupported(format!("建目录 {} 失败: {error}", dir.display())))?;
        Ok(Value::Bool(true))
    })));

    members.push(("file_exists", host_native(move |_vm, args, _kwargs| {
        let path = want(&args, 0, "file_exists")?.text_of();
        Ok(Value::Bool(pin_path(&host, &path).map(|file| file.exists()).unwrap_or(false)))
    })));

    members.push(("is_dir", host_native(move |_vm, args, _kwargs| {
        let path = want(&args, 0, "is_dir")?.text_of();
        Ok(Value::Bool(pin_path(&host, &path).map(|file| file.is_dir()).unwrap_or(false)))
    })));

    members.push(("list_dir", host_native(move |_vm, args, _kwargs| {
        let path = args.first().map(Value::text_of).unwrap_or_else(|| ".".to_string());
        let dir = pin_path(&host, &path)?;
        let read = std::fs::read_dir(&dir)
            .map_err(|error| RunError::Unsupported(format!("列目录 {} 失败: {error}", dir.display())))?;
        let mut names: Vec<Value> = Vec::new();
        for entry in read.flatten() {
            names.push(Value::text(&entry.file_name().to_string_lossy()));
        }
        names.sort_by(|a, b| a.text_of().cmp(&b.text_of()));
        Ok(Value::list(names))
    })));

    members.push(("json_parse", host_native(|_vm, args, _kwargs| {
        json::parse(&want(&args, 0, "json_parse")?.text_of())
    })));

    members.push(("json_dump", host_native(|_vm, args, kwargs| {
        let value = want(&args, 0, "json_dump")?;
        let pretty = kwargs.iter().any(|(name, value)| &**name == "indent" && value.truthy());
        Ok(Value::text(&json::dump(value, pretty)))
    })));

    members.push(("now", host_native(|_vm, _args, _kwargs| Ok(Value::Int(Host::now_secs())))));

    members.push(("datetime", host_native(|_vm, args, _kwargs| {
        let secs = args.first().map(to_int).transpose()?.unwrap_or_else(Host::now_secs);
        Ok(Value::text(&Host::format_datetime(secs)))
    })));

    members.push(("date", host_native(|_vm, args, _kwargs| {
        let secs = args.first().map(to_int).transpose()?.unwrap_or_else(Host::now_secs);
        Ok(Value::text(&Host::format_date(secs)))
    })));

    members.push(("hash", host_native(|_vm, args, kwargs| {
        let text = want(&args, 0, "hash")?.text_of();
        let salt = args.get(1).map(Value::text_of)
            .or_else(|| kwargs.iter().find(|(name, _)| &**name == "salt").map(|(_, v)| v.text_of()))
            .unwrap_or_default();
        let rounds = kwargs.iter().find(|(name, _)| &**name == "rounds")
            .map(|(_, value)| to_int(value).unwrap_or(256)).unwrap_or(256);
        // 轮数下限 1，避免脚本写 rounds=0 时得到未经迭代的裸摘要。
        Ok(Value::text(&Host::digest(&text, &salt, rounds.max(1) as usize)))
    })));

    members.push(("token", host_native(move |_vm, _args, kwargs| {
        let prefix = kwargs.iter().find(|(name, _)| &**name == "prefix")
            .map(|(_, value)| value.text_of()).unwrap_or_else(|| "s".to_string());
        let mut state = web_state.borrow_mut();
        state.counter += 1;
        let raw = format!("{}|{}|{}|{:?}", prefix, Host::now_secs(), state.counter, std::thread::current().id());
        Ok(Value::text(&Host::digest(&raw, "xorweb-token", 1)[..32].to_string()))
    })));

    members.push(("url_encode", host_native(|_vm, args, _kwargs| {
        Ok(Value::text(&web::url_encode(&want(&args, 0, "url_encode")?.text_of())))
    })));

    members.push(("url_decode", host_native(|_vm, args, _kwargs| {
        Ok(Value::text(&web::url_decode(&want(&args, 0, "url_decode")?.text_of())))
    })));

    members.push(("parse_form", host_native(|_vm, args, _kwargs| {
        Ok(web::decode_form(&want(&args, 0, "parse_form")?.text_of()))
    })));

    members.push(("path_join", host_native(move |_vm, args, _kwargs| {
        let mut joined = PathBuf::from(args.first().map(Value::text_of).unwrap_or_default());
        for part in &args[1..] {
            joined = joined.join(part.text_of());
        }
        let display = joined.to_string_lossy().replace('\\', "/");
        let _ = root;
        Ok(Value::text(&display))
    })));

    host_module("std", &members)
}

/// `os` 模块：平台、分隔符、环境变量与脚本参数。
fn build_os(root: PathBuf, args: &[String]) -> Value {
    let host = root.clone();
    let mut members: Vec<(&'static str, Value)> = Vec::new();
    members.push(("name", Value::text(if cfg!(windows) { "nt" } else { "posix" })));
    members.push(("sep", Value::text(if cfg!(windows) { "\\" } else { "/" })));
    members.push(("cwd", host_native(move |_vm, _args, _kwargs| {
        Ok(Value::text(&host.to_string_lossy()))
    })));
    members.push(("getenv", host_native(|_vm, args, kwargs| {
        let name = want(&args, 0, "getenv")?.text_of();
        let fallback = args.get(1).map(Value::text_of)
            .or_else(|| kwargs.iter().find(|(key, _)| &**key == "default").map(|(_, v)| v.text_of()))
            .unwrap_or_default();
        Ok(Value::text(&std::env::var(name).unwrap_or(fallback)))
    })));
    members.push(("args", Value::list(args.iter().map(|arg| Value::text(arg)).collect())));
    host_module("os", &members)
}

/// 内置表用的小工具：取第 index 个实参。
fn want<'a>(args: &'a [Value], index: usize, name: &str) -> Result<&'a Value, RunError> {
    args.get(index).ok_or_else(|| RunError::TypeError(format!("{name} 少了一个参数")))
}

/// 值转整数（布尔按 0/1）。
fn to_int(value: &Value) -> Result<i64, RunError> {
    match value {
        Value::Int(number) => Ok(*number),
        Value::Bool(flag) => Ok(*flag as i64),
        other => Err(RunError::TypeError(format!("这里要整数，给的是 {}", other.type_name()))),
    }
}

/// 长度的统一算法。
fn length_of(value: &Value) -> Result<i64, RunError> {
    Ok(match value {
        Value::Str(text) => text.chars().count() as i64,
        Value::List(items) => items.borrow().len() as i64,
        Value::Dict(map) => map.borrow().pairs.len() as i64,
        other => return Err(RunError::TypeError(format!("{} 没有长度", other.type_name()))),
    })
}

/// 可迭代值展开，复用 VM 的同一份实现。
fn elements(value: &Value) -> Result<Vec<Value>, RunError> {
    vm::elements_of(value)
}

/// max/min：既接 `max(a, b, c)` 也接 `max(list)`。
fn fold_compare(args: &[Value], want_max: bool) -> Result<Value, RunError> {
    let mut pool: Vec<Value> = Vec::new();
    for arg in args {
        match arg {
            Value::List(items) => pool.extend(items.borrow().iter().cloned()),
            Value::Dict(map) => pool.extend(map.borrow().pairs.iter().map(|(_, v)| v.clone())),
            other => pool.push(other.clone()),
        }
    }
    let name = if want_max { "max" } else { "min" };
    let mut best = pool.first().cloned()
        .ok_or_else(|| RunError::TypeError(format!("{name} 至少要一个值")))?;
    for candidate in &pool[1..] {
        let ordering = best.compare(candidate)
            .ok_or_else(|| RunError::TypeError(format!("{} 比较不了 {} 和 {}", name, best.type_name(), candidate.type_name())))?;
        use std::cmp::Ordering::*;
        let replace = if want_max { ordering == Less } else { ordering == Greater };
        if replace {
            best = candidate.clone();
        }
    }
    Ok(best)
}

/// 排序：可选 key 回调油饼函数，结果按 `compare` 决定，不可比的保持原序。
fn sort_values(vm: &mut Vm, items: &mut Vec<Value>, key: Option<&Value>, reverse: bool)
    -> Result<(), RunError> {
    let mut decorated: Vec<(Value, Value)> = Vec::new();
    for item in items.iter() {
        let rank = match key {
            Some(function) if function.is_callable() => vm.invoke(function, vec![item.clone()], Vec::new())?,
            Some(other) => return Err(RunError::TypeError(format!("sorted 的 key 不可调用：{}", other.type_name()))),
            None => item.clone(),
        };
        decorated.push((rank, item.clone()));
    }
    decorated.sort_by(|(left, _), (right, _)| {
        match left.compare(right) {
            Some(ordering) => ordering,
            None => std::cmp::Ordering::Equal,
        }
    });
    if reverse {
        decorated.reverse();
    }
    *items = decorated.into_iter().map(|(_, item)| item).collect();
    Ok(())
}

/// 宿主侧的路径围栏。
fn pin_path(root: &Path, raw: &str) -> Result<PathBuf, RunError> {
    let mut target = root.to_path_buf();
    let input = Path::new(raw);
    if input.is_absolute() {
        target = input.to_path_buf();
        if !target.starts_with(root) {
            return Err(RunError::RangeError(format!("路径 {raw} 越出了项目根目录")));
        }
    }
    for component in input.components() {
        match component {
            Component::Normal(part) => target.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                target.pop();
                if !target.starts_with(root) {
                    return Err(RunError::RangeError(format!("路径 {raw} 越出了项目根目录")));
                }
            }
            _ => return Err(RunError::RangeError(format!("路径 {raw} 里有不允许的写法"))),
        }
    }
    Ok(target)
}

/// 字节数组转小写十六进制。
fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// 欧氏除法与取余，负数时间戳也要落在正确的日历日。
fn div_rem(value: i64, divisor: i64) -> (i64, i64) {
    (value.div_euclid(divisor), value.rem_euclid(divisor))
}

/// Howard Hinnant 的 civil_from_days：把 1970-01-01 起的天数换成 (年, 年内的第几天)。
fn civil_from_days(days: i64) -> (i64, i64) {
    let shifted = days + 719468;
    let era = if shifted >= 0 { shifted } else { shifted - 146096 } / 146097;
    let day_of_era = shifted - era * 146097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    (if year <= 0 { year + 1 } else { year }, day_of_year)
}

/// 年内第几天 → (月, 日)，闰年按能被 400 整除或能被 4 整除且不能被 100 整除判定。
fn month_day(year: i64, day_of_year: i64) -> (i64, i64) {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let lengths: [i64; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut remainder = day_of_year;
    for (index, length) in lengths.iter().enumerate() {
        if remainder < *length {
            return ((index + 1) as i64, remainder + 1);
        }
        remainder -= length;
    }
    (12, remainder + 1)
}
