//! 自研模板引擎：`{{ 表达式 }}`、`{% if %} / {% for %} / {% include %}`、`{# 注释 #}`。
//!
//! 表达式不另写一套解析器：直接复用油饼的词法与语法前端，把表达式编译成字节码，
//! 再交给同一台虚拟机在模板命名空间里求值——模板里的 `a.b`、`a[0]`、`x + y`、`a == b`
//! 与 .yb 源码里的行为完全一致，不存在第二套语义。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::ast::{Module, Stmt};
use crate::bytecode::Proto;
use crate::compile::compile_module;
use crate::error::{CompileError, Pos, RunError};
use crate::lex::{scan, Tok, Token};
use crate::parse::parse_interpolation;
use crate::value::{Globals, Value};
use crate::vm::Vm;

/// 一个模板节点。
enum Node {
    /// 原样输出的文本。
    Text(String),
    /// `{{ 表达式 }}`：默认做 HTML 转义，`{{{ 表达式 }}}` 原样输出。
    Print { proto: Rc<Proto>, raw: bool },
    /// `{% if %}` 链：arms 是 (条件, body)，orelse 是 `{% else %}` 的 body。
    If { arms: Vec<(Rc<Proto>, Vec<Node>)>, orelse: Option<Vec<Node>> },
    /// `{% for 变量 in 表达式 %}`。
    For { proto: Rc<Proto>, var: Box<str>, body: Vec<Node> },
    /// `{% include "片段.html" %}`。
    Include { proto: Rc<Proto> },
}

/// 解析后的模板。
pub struct Template {
    nodes: Vec<Node>,
}

/// 模板集合：一个目录加一份编译缓存。
pub struct TemplateSet {
    dir: PathBuf,
    cache: RefCell<HashMap<String, Rc<Template>>>,
}

impl TemplateSet {
    /// 指定模板目录建集合。
    pub fn new(dir: PathBuf) -> TemplateSet {
        TemplateSet { dir, cache: RefCell::new(HashMap::new()) }
    }

    /// 模板根目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 按文件名取模板，命中缓存就不再读盘。
    pub fn load(&self, name: &str) -> Result<Rc<Template>, RunError> {
        if let Some(found) = self.cache.borrow().get(name) {
            return Ok(found.clone());
        }
        let path = self.dir.join(name);
        if !path.is_file() {
            return Err(RunError::ModuleNotFound(format!("模板 {name}")));
        }
        let text = read_text(&path)?;
        let template = Rc::new(Template::parse(name, &text)?);
        self.cache.borrow_mut().insert(name.to_string(), template.clone());
        Ok(template)
    }

    /// 渲染一个模板文件，返回 HTML 文本。
    pub fn render(&self, vm: &mut Vm, name: &str, globals: Globals) -> Result<String, RunError> {
        let template = self.load(name)?;
        let mut out = String::new();
        render_nodes(&template.nodes, vm, &globals, self, &mut out)?;
        Ok(out)
    }
}

/// 渲染一组节点。
fn render_nodes(nodes: &[Node], vm: &mut Vm, globals: &Globals, set: &TemplateSet, out: &mut String)
    -> Result<(), RunError> {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Print { proto, raw } => {
                let value = vm.eval_proto(proto.clone(), globals.clone())?;
                if *raw {
                    out.push_str(&value.text_of());
                } else {
                    escape_html(&value.text_of(), out);
                }
            }
            Node::If { arms, orelse } => {
                let mut taken = false;
                for (cond, body) in arms {
                    if vm.eval_proto(cond.clone(), globals.clone())?.truthy() {
                        render_nodes(body, vm, globals, set, out)?;
                        taken = true;
                        break;
                    }
                }
                if !taken {
                    if let Some(body) = orelse {
                        render_nodes(body, vm, globals, set, out)?;
                    }
                }
            }
            Node::For { proto, var, body } => {
                let items = crate::vm::elements_of(&vm.eval_proto(proto.clone(), globals.clone())?)?;
                let total = items.len();
                let previous = globals.borrow().get(&**var).cloned();
                for (index, item) in items.into_iter().enumerate() {
                    let loop_info = Value::dict();
                    if let Value::Dict(map) = &loop_info {
                        let mut map = map.borrow_mut();
                        map.insert(Value::text("index"), Value::Int(index as i64 + 1));
                        map.insert(Value::text("index0"), Value::Int(index as i64));
                        map.insert(Value::text("first"), Value::Bool(index == 0));
                        map.insert(Value::text("last"), Value::Bool(index + 1 == total));
                        map.insert(Value::text("length"), Value::Int(total as i64));
                    }
                    let mut bound = globals.borrow_mut();
                    bound.insert(var.clone(), item);
                    bound.insert("loop".into(), loop_info);
                    drop(bound);
                    render_nodes(body, vm, globals, set, out)?;
                }
                // 循环变量出作用域：恢复外层同名绑定，没有就删掉，不泄漏到后续节点。
                let mut bound = globals.borrow_mut();
                match previous {
                    Some(value) => { bound.insert(var.clone(), value); }
                    None => { bound.remove(&**var); }
                }
                bound.remove("loop");
            }
            Node::Include { proto } => {
                let name = vm.eval_proto(proto.clone(), globals.clone())?.text_of();
                let piece = set.load(&name)?;
                render_nodes(&piece.nodes, vm, globals, set, out)?;
            }
        }
    }
    Ok(())
}

/// 模板解析游标，`at` 始终是字符边界。
struct Cursor<'t> {
    src: &'t str,
    at: usize,
    name: String,
    /// 最近读到的结构标签，body 递归返回后由上层读它判定闭合方式。
    last_tag: (String, String),
}

impl<'t> Cursor<'t> {
    /// 当前所在的行号（1 起），报错用。
    fn line(&self) -> u32 {
        1 + self.src[..self.at].matches('\n').count() as u32
    }

    /// 造一条带模板名和行号的错误。
    fn fail(&self, msg: impl Into<String>) -> RunError {
        RunError::Unsupported(format!("{}:{}: 模板 {}", self.name, self.line(), msg.into()))
    }

    /// 读一组节点，遇到 `stops` 里的结构标签就把它消费掉并返回，由上层决定怎么收尾。
    fn nodes(&mut self, stops: &[&str]) -> Result<Vec<Node>, RunError> {
        let mut nodes: Vec<Node> = Vec::new();
        let mut text = String::new();
        loop {
            let rest = &self.src[self.at..];
            let Some(offset) = find_marker(rest) else {
                text.push_str(rest);
                self.at = self.src.len();
                break;
            };
            text.push_str(&rest[..offset]);
            self.at += offset;
            let marker = &rest[..2];
            if marker == "{#" {
                if !text.is_empty() {
                    nodes.push(Node::Text(std::mem::take(&mut text)));
                }
                self.skip_comment()?;
                continue;
            }
            if marker == "{{" {
                if !text.is_empty() {
                    nodes.push(Node::Text(std::mem::take(&mut text)));
                }
                nodes.push(self.print()?);
                continue;
            }
            if !text.is_empty() {
                nodes.push(Node::Text(std::mem::take(&mut text)));
            }
            let (tag, expr) = self.block_tag()?;
            if stops.contains(&tag.as_str()) {
                return Ok(nodes);
            }
            match tag.as_str() {
                "if" => nodes.push(self.if_chain(&expr)?),
                "for" => nodes.push(self.for_loop(&expr)?),
                "include" => nodes.push(self.include(&expr)?),
                other => return Err(self.fail(format!("结构标签 {other} 没有对应的开始标签"))),
            }
        }
        if !text.is_empty() {
            nodes.push(Node::Text(text));
        }
        Ok(nodes)
    }

    /// `{% if %}` / `{% elif %}` / `{% else %}` / `{% endif %}`。
    fn if_chain(&mut self, cond_text: &str) -> Result<Node, RunError> {
        let mut arms = Vec::new();
        let mut orelse = None;
        let mut cond = compile_expr(cond_text, &self.name)?;
        loop {
            let body = self.nodes(&["elif", "else", "endif"])?;
            arms.push((cond.clone(), body));
            match self.last_tag.0.as_str() {
                "elif" => { cond = compile_expr(&self.last_tag.1, &self.name)?; }
                "else" => {
                    orelse = Some(self.nodes(&["endif"])?);
                    if self.last_tag.0 != "endif" {
                        return Err(self.fail("{% else %} 后面必须有 {% endif %}"));
                    }
                    break;
                }
                "endif" => break,
                other => return Err(self.fail(format!("if 里遇到了未期待的标签 {other}"))),
            }
        }
        Ok(Node::If { arms, orelse })
    }

    /// `{% for x in items %}` / `{% endfor %}`。
    fn for_loop(&mut self, expr: &str) -> Result<Node, RunError> {
        let (var, iterable) = split_for(expr).ok_or_else(|| {
            self.fail(format!("`{expr}` 不是 `变量 in 集合` 形式"))
        })?;
        let proto = compile_expr(&iterable, &self.name)?;
        let body = self.nodes(&["endfor"])?;
        if self.last_tag.0 != "endfor" {
            return Err(self.fail("{% for %} 没有对应的 {% endfor %}"));
        }
        Ok(Node::For { proto, var: var.into(), body })
    }

    /// `{% include "片段.html" %}`：不带引号时按裸名补成字符串字面量。
    fn include(&mut self, expr: &str) -> Result<Node, RunError> {
        let literal = if expr.starts_with('"') || expr.starts_with('\'') {
            expr.to_string()
        } else {
            format!("\"{expr}\"")
        };
        Ok(Node::Include { proto: compile_expr(&literal, &self.name)? })
    }

    /// 跳过 `{# 注释 #}`。
    fn skip_comment(&mut self) -> Result<(), RunError> {
        let rest = &self.src[self.at..];
        match rest.find("#}") {
            Some(offset) => {
                self.at += offset + 2;
                Ok(())
            }
            None => Err(self.fail("{# 注释没有 #} 收尾")),
        }
    }

    /// 读一个 `{{ }}`；`{{{ }}}` 表示原样输出。
    fn print(&mut self) -> Result<Node, RunError> {
        let rest = &self.src[self.at..];
        let raw = rest.starts_with("{{{");
        let close = if raw { "}}}" } else { "}}" };
        let open = if raw { 3 } else { 2 };
        let expr = tagged_body(rest, open, close).ok_or_else(|| self.fail("{{ 没有 }} 收尾"))?;
        self.at += open + expr.len() + close.len();
        Ok(Node::Print { proto: compile_expr(expr.trim(), &self.name)?, raw })
    }

    /// 读一个 `{% %}`，同时把它记进 last_tag。
    fn block_tag(&mut self) -> Result<(String, String), RunError> {
        let rest = &self.src[self.at..];
        let body = tagged_body(rest, 2, "%}").ok_or_else(|| self.fail("{% 没有 %} 收尾"))?;
        self.at += 2 + body.len() + 2;
        let trimmed = body.trim();
        let head = trimmed.split_whitespace().next().unwrap_or("").to_string();
        let tail = trimmed[head.len()..].trim().to_string();
        self.last_tag = (head.clone(), tail.clone());
        Ok((head, tail))
    }
}

/// 解析入口：模板文本 → 节点树。
impl Template {
    /// 把模板文本解析成节点树。
    pub fn parse(name: &str, text: &str) -> Result<Template, RunError> {
        let body = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut cursor = Cursor {
            src: body,
            at: 0,
            name: name.to_string(),
            last_tag: (String::new(), String::new()),
        };
        let nodes = cursor.nodes(&[])?;
        Ok(Template { nodes })
    }
}

/// 取出 `rest` 从 `open` 之后到 `close` 之前的内容。
fn tagged_body<'s>(rest: &'s str, open: usize, close: &str) -> Option<&'s str> {
    let tail = rest.get(open..)?;
    let offset = tail.find(close)?;
    Some(&tail[..offset])
}

/// 找到下一个 `{{`、`{%` 或 `{#` 的字节下标。
fn find_marker(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut index = 1;
    while index + 1 < bytes.len() {
        if bytes[index] == b'{' && matches!(bytes[index + 1], b'{' | b'%' | b'#') {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// 拆 `for` 的变量名与迭代表达式，按 ` in ` 的第一个出现位置切。
fn split_for(expr: &str) -> Option<(String, String)> {
    let (offset, _) = expr.match_indices(" in ").next()?;
    let var = expr[..offset].trim();
    let iterable = expr[offset + " in ".len()..].trim();
    if var.is_empty() || iterable.is_empty() {
        return None;
    }
    Some((var.to_string(), iterable.to_string()))
}

/// 把一段表达式文本编成可求值的原型。
fn compile_expr(text: &str, template: &str) -> Result<Rc<Proto>, RunError> {
    let tokens = scan(text)
        .map_err(|error| RunError::Unsupported(format!("{template} 词法: {error}")))?;
    let cleaned: Vec<Token> = tokens.into_iter()
        .filter(|token| !matches!(token.tok, Tok::Newline | Tok::Indent | Tok::Dedent | Tok::Eof))
        .collect();
    if cleaned.is_empty() {
        return Err(RunError::Unsupported(format!("{template} 里有空的模板表达式")));
    }
    let expr = parse_interpolation(&cleaned)
        .map_err(|error: CompileError| RunError::Unsupported(format!("{template} 语法: {error}")))?;
    // 用一个只含 return 的模块包住表达式，模板求值走的正是 VM 的正常返回路径。
    let module = Module {
        path: template.to_string(),
        stmts: vec![Stmt::Return { value: Some(expr), pos: Pos::new(1, 1) }],
    };
    compile_module(&module)
        .map_err(|error| RunError::Unsupported(format!("{template} 编译: {error}")))
}

/// 默认转义，防 XSS：模板里输出的值一律过这里。
fn escape_html(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

/// 读文本文件，剥掉 BOM。
fn read_text(path: &Path) -> Result<String, RunError> {
    let bytes = std::fs::read(path)
        .map_err(|error| RunError::Unsupported(format!("读取 {} 失败: {error}", path.display())))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned().trim_start_matches('\u{feff}').to_string())
}
