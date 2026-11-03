//! `xorweb` 宿主绑定：Flask 风格的服务对象、路由装饰器、模板渲染与请求上下文。
//!
//! HTTP 层用 tiny_http，其余（URL 编解码、表单解析、Cookie 解析、路由匹配、响应组装）
//! 都是本仓库自研实现，避免为一个 CMS 再拖进一串框架依赖。
//! 请求对象是一份活的字典：`xorweb.request` 与每次请求里填充的是同一个 Rc，
//! 这样脚本可以继续写 `xorweb.request.form.get("username", "")`。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;

use tiny_http::{Header, Response, Server};

use crate::error::RunError;
use crate::host::Host;
use crate::json;
use crate::template::TemplateSet;
use crate::value::{host_module, host_obj, with_method, DictData, Globals, Value};
use crate::Vm;

/// 路由表里的一条注册记录。
#[derive(Clone)]
pub struct Route {
    /// 原始写法，形如 `/post/<int:id>`。
    pub pattern: String,
    /// 允许的请求方法，空表示只允许 GET/HEAD。
    pub methods: Vec<String>,
    /// 处理函数（油饼函数或宿主函数）。
    pub handler: Value,
}

/// 一个 `Webservice(name)` 实例的私有状态。
pub struct AppState {
    pub name: String,
    pub routes: Vec<Route>,
}

/// 整个解释器共享的 Web 状态。
pub struct WebState {
    pub root: PathBuf,
    pub templates: Rc<TemplateSet>,
    /// 注册过的服务，`Webservice()` 每调一次多一个。
    pub apps: Vec<Rc<RefCell<AppState>>>,
    /// 活的请求上下文，`xorweb.request` 就是它。
    pub request: Value,
    /// 令牌计数与种子，`std.token` 用。
    pub counter: u64,
    pub seed: u64,
    /// 已处理请求数。
    pub served: u64,
}

impl WebState {
    /// 以项目根建 Web 状态，模板目录默认 `root/templates`。
    pub fn new(root: PathBuf) -> WebState {
        let templates = Rc::new(TemplateSet::new(root.join("templates")));
        WebState {
            root,
            templates,
            apps: Vec::new(),
            request: Value::Dict(Rc::new(RefCell::new(DictData::new()))),
            counter: 0,
            seed: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.subsec_nanos() as u64)
                .unwrap_or(0x9e37),
            served: 0,
        }
    }
}

/// 造 `xorweb` 模块值。
pub fn module(state: Rc<RefCell<WebState>>) -> Value {
    let mut members: Vec<(&'static str, Value)> = Vec::new();

    // Webservice(name) → 服务对象，带 route/run 两个方法。
    members.push(("Webservice", {
        let state = state.clone();
        crate::host::host_native(move |_vm: &mut Vm, args, _kwargs| {
            let name = args.first().map(Value::text_of).unwrap_or_else(|| "xorweb".to_string());
            let app = Rc::new(RefCell::new(AppState { name: name.clone(), routes: Vec::new() }));
            let obj = host_obj("Webservice");
            if let Value::Obj(inner) = &obj {
                let mut inner = inner.borrow_mut();
                inner.fields.insert("name".into(), Value::text(&name));
                inner.opaque = Some(Rc::new(app.clone()) as Rc<dyn std::any::Any>);
            }
            state.borrow_mut().apps.push(app);
            let router = with_method(&obj, "route", route_decorator(app.clone()));
            Ok(with_method(&router, "run", run_server(app.clone(), state.clone())))
        })
    }));

    members.push(("render_template", {
        let state = state.clone();
        crate::host::host_native(move |vm: &mut Vm, args, kwargs| render_template(&state, vm, args, kwargs))
    }));

    members.push(("url_for", crate::host::host_native(|_vm, args, kwargs| Ok(url_for(args, kwargs)))));
    members.push(("redirect", crate::host::host_native(|_vm, args, kwargs| Ok(redirect(args, kwargs)))));
    members.push(("make_response", crate::host::host_native(|_vm, args, kwargs| Ok(make_response(args, kwargs)))));
    members.push(("json_response", crate::host::host_native(|_vm, args, kwargs| Ok(json_response(args, kwargs)))));
    members.push(("abort", crate::host::host_native(|_vm, args, kwargs| Ok(abort(args, kwargs)))));
    members.push(("escape", crate::host::host_native(|_vm, args, _kwargs| {
        let mut out = String::new();
        escape_html(&args.first().map(Value::text_of).unwrap_or_default(), &mut out);
        Ok(Value::text(&out))
    })));
    members.push(("parse_form", crate::host::host_native(|_vm, args, _kwargs| {
        Ok(decode_form(&args.first().map(Value::text_of).unwrap_or_default()))
    })));
    members.push(("url_encode", crate::host::host_native(|_vm, args, _kwargs| {
        Ok(Value::text(&url_encode(&args.first().map(Value::text_of).unwrap_or_default())))
    })));
    members.push(("url_decode", crate::host::host_native(|_vm, args, _kwargs| {
        Ok(Value::text(&url_decode(&args.first().map(Value::text_of).unwrap_or_default())))
    })));
    members.push(("now", crate::host::host_native(|_vm, _args, _kwargs| Ok(Value::Int(Host::now_secs())))));

    let request = state.borrow().request.clone();
    members.push(("request", request));
    host_module("xorweb", &members)
}

/// `@app.route('/path', methods=["POST"])`：先返回装饰器，装饰器把函数登记进路由表。
fn route_decorator(app: Rc<RefCell<AppState>>) -> crate::value::Native {
    Rc::new(move |_vm: &mut Vm, args, kwargs| {
        let path = args.get(1).map(Value::text_of)
            .ok_or_else(|| RunError::TypeError("route 需要路径参数".into()))?;
        let methods = args.get(2).or_else(|| kwargs.iter().find(|(name, _)| &**name == "methods")
            .map(|(_, value)| value))
            .map(|value| match value {
                Value::List(items) => items.borrow().iter().map(|m| m.text_of().to_uppercase()).collect(),
                Value::Str(text) => vec![text.to_uppercase()],
                _ => Vec::new(),
            })
            .unwrap_or_default();
        let app = app.clone();
        Ok(crate::host::host_native(move |_vm: &mut Vm, handler, _kwargs| {
            let function = handler.into_iter().next()
                .ok_or_else(|| RunError::TypeError("装饰器至少要接一个函数".into()))?;
            app.borrow_mut().routes.push(Route {
                pattern: normalize_pattern(&path),
                methods: methods.clone(),
                handler: function.clone(),
            });
            Ok(function)
        }))
    })
}

/// `app.run(host=, port=, debug=, requests=)`：起服务并阻塞处理请求。
fn run_server(app: Rc<RefCell<AppState>>, state: Rc<RefCell<WebState>>) -> crate::value::Native {
    Rc::new(move |vm: &mut Vm, _args, kwargs| {
        let host = kw_text(&kwargs, "host", "127.0.0.1");
        let port = kw_int(&kwargs, "port", 8080);
        let quiet = !kw_bool(&kwargs, "debug", true);
        let limit = kwargs.iter().find(|(name, _)| &**name == "requests")
            .and_then(|(_, value)| match value { Value::Int(number) => Some(*number as u64), _ => None });
        let addr = format!("{host}:{port}");
        let server = Server::http(addr.as_str())
            .map_err(|error| RunError::Unsupported(format!("监听 {addr} 失败: {error}")))?;
        let name = state.borrow().apps.len().to_string();
        let _ = name;
        println!("xorweb: 服务已启动 http://{addr}");
        let routes = app.borrow().routes.clone();
        let root = state.borrow().root.clone();
        let mut handled = 0u64;
        for request in server.incoming_requests() {
            let outcome = handle(&routes, &root, state.clone(), vm, request, quiet);
            handled += 1;
            match outcome {
                Ok(_) => {}
                Err(error) => eprintln!("xorweb: 请求处理失败: {error}"),
            }
            if limit.map(|max| handled >= max).unwrap_or(false) {
                break;
            }
        }
        Ok(Value::None)
    })
}

/// 处理一条请求：填上下文 → 匹配路由 → 调处理函数 → 组装响应。
fn handle(routes: &[Route], root: &PathBuf, state: Rc<RefCell<WebState>>, vm: &mut Vm,
    request: tiny_http::Request, quiet: bool) -> Result<(), RunError> {
    let method = format!("{:?}", request.method()).to_uppercase();
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((head, tail)) => (head.to_string(), tail.to_string()),
        None => (url.clone(), String::new()),
    };
    let mut body = Vec::new();
    let _ = request.as_reader().read_to_end(&mut body);
    let body_text = String::from_utf8_lossy(&body).into_owned();
    let mut headers: Vec<(String, String)> = Vec::new();
    for header in request.headers() {
        headers.push((header.field.as_str().to_string().to_lowercase(), header.value.as_str().to_string()));
    }
    let content_type = headers.iter().find(|(name, _)| name == "content-type")
        .map(|(_, value)| value.clone()).unwrap_or_default();
    let cookie = headers.iter().find(|(name, _)| name == "cookie")
        .map(|(_, value)| value.clone()).unwrap_or_default();
    let form = if content_type.contains("application/x-www-form-urlencoded") {
        decode_form(&body_text)
    } else if content_type.contains("application/json") {
        json::parse(&body_text).unwrap_or_else(|_| Value::dict())
    } else {
        Value::dict()
    };
    let remote = headers.iter().find(|(name, _)| name == "x-forwarded-for")
        .map(|(_, value)| value.clone()).unwrap_or_else(|| "127.0.0.1".to_string());

    fill_context(&state, &method, &path, &query, &form, &headers, &cookie, &body_text, &remote);
    state.borrow_mut().served += 1;

    // 静态目录优先：/static/** 直接落盘读取，不进路由表。
    if let Some(file) = static_file(root, &path) {
        let served = serve_file(&file);
        return respond(request, served);
    }

    let mut matched: Option<(&Route, Vec<(Box<str>, Value)>)> = None;
    for route in routes {
        let segments = split_path(&route.pattern);
        let Some(params) = match_path(&segments, &split_path(&path)) else { continue };
        let allowed = if route.methods.is_empty() {
            method == "GET" || method == "HEAD"
        } else {
            route.methods.iter().any(|allowed| allowed == &method)
        };
        if allowed {
            matched = Some((route, params.into_iter().map(|(k, v)| (Box::from(k), v)).collect()));
            break;
        }
    }

    let (status, reply) = match matched {
        Some((route, kwargs)) => match route.handler.clone() {
            Value::None => (404, body_of("404 未找到".into())),
            other => match vm.invoke(&other, Vec::new(), kwargs) {
                Ok(value) => render_value_to_reply(&value),
                Err(error) => {
                    eprintln!("xorweb: {} {path} 里出错: {error}", route.pattern);
                    (500, body_of(format!("<h1>500</h1><pre>{}</pre>", escape_once(&error.to_string()))))
                }
            },
        }
        None => (404, body_of("<h1>404</h1><p>这个地址没有对应的处理函数。</p>".to_string())),
    };
    if !quiet {
        println!("{method} {url} → {status}");
    }
    let response = build_response(status, reply);
    respond(request, response)
}

/// 一条待发送的响应。
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// 额外的响应头，Cookie 与 Location 走这里。
    pub headers: Vec<(String, String)>,
}

/// 组装成 tiny_http 响应并发出。
fn build_response(_status: u16, reply: Reply) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_data(reply.body).with_status_code(reply.status as u16);
    let typed = Header::from_bytes(b"Content-Type", reply.content_type.as_bytes());
    if let Ok(header) = typed {
        response = response.with_header(header);
    }
    for (name, value) in reply.headers {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response = response.with_header(header);
        }
    }
    response
}

/// 发出去；连接被浏览器提前掐断不算错。
fn respond(request: tiny_http::Request, response: Response<std::io::Cursor<Vec<u8>>>) -> Result<(), RunError> {
    let _ = request.respond(response);
    Ok(())
}

/// 只有一个文本 body 的简单响应。
fn body_of(text: String) -> Reply {
    Reply { status: 200, content_type: "text/html; charset=utf-8".into(), body: text.into_bytes(), headers: Vec::new() }
}

/// 把处理函数的返回值折成 (状态码, 响应体)。
fn render_value_to_reply(value: &Value) -> (u16, Reply) {
    match value {
        Value::Dict(map) => {
            let snapshot = map.borrow().pairs.clone();
            let lookup = |key: &str| snapshot.iter().find(|(k, _)| k.text_of() == key).map(|(_, v)| v.clone());
            let status = lookup("status").and_then(|v| match v { Value::Int(n) => Some(n as u16), _ => None })
                .unwrap_or(if lookup("redirect").is_some() { 302 } else { 200 });
            let content_type = lookup("content_type").map(|v| v.text_of())
                .unwrap_or_else(|| if lookup("redirect").is_some() { "text/html; charset=utf-8".into() } else { "text/html; charset=utf-8".into() });
            let body = match lookup("body").or_else(|| lookup("text")) {
                Some(value) => value.text_of().into_bytes(),
                None => String::new().into_bytes(),
            };
            let mut headers: Vec<(String, String)> = Vec::new();
            if let Some(target) = lookup("redirect") {
                headers.push(("Location".to_string(), target.text_of()));
            }
            if let Some(extra) = lookup("headers") {
                if let Value::Dict(map) = extra {
                    for (key, value) in map.borrow().pairs.iter() {
                        headers.push((key.text_of(), value.text_of()));
                    }
                }
            }
            if let Some(cookies) = lookup("cookies") {
                for line in crate::vm::elements_of(&cookies).unwrap_or_default() {
                    headers.push(("Set-Cookie".to_string(), line.text_of()));
                }
            }
            (status, Reply { status, content_type, body, headers })
        }
        Value::None => (500, body_of("<h1>500</h1><p>处理函数没有返回内容。</p>".to_string())),
        other => (200, body_of(other.text_of())),
    }
}

/// 渲染模板：上下文是关键字参，另外注入 request、url_for、escape。
fn render_template(state: &Rc<RefCell<WebState>>, vm: &mut Vm, args: Vec<Value>,
    kwargs: Vec<(Box<str>, Value)>) -> Result<Value, RunError> {
    let name = args.first().map(Value::text_of)
        .ok_or_else(|| RunError::TypeError("render_template 需要模板文件名".into()))?;
    let templates = { state.borrow().templates.clone() };
    let request = { state.borrow().request.clone() };
    let state = state.clone();
    let globals: Globals = Rc::new(RefCell::new(HashMap::new()));
    for (key, value) in kwargs {
        globals.borrow_mut().insert(key, value);
    }
    globals.borrow_mut().insert("request".into(), request);
    globals.borrow_mut().insert("escape".into(), module_escape());
    globals.borrow_mut().insert("url_for".into(), crate::host::host_native(move |_vm, args, kwargs| {
        Ok(url_for_with(&state, args, kwargs))
    }));
    let html = templates.render(vm, &name, globals)?;
    Ok(Value::text(&html))
}

/// 给模板用的 escape 函数值。
fn module_escape() -> Value {
    crate::host::host_native(|_vm, args, _kwargs| {
        let mut out = String::new();
        escape_html(&args.first().map(Value::text_of).unwrap_or_default(), &mut out);
        Ok(Value::text(&out))
    })
}

/// `xorweb.url_for('/post/<int:id>', id=3)`：按参数名回填占位，剩余参数转成查询串。
fn url_for(args: &[Value], kwargs: Vec<(Box<str>, Value)>) -> Value {
    let pattern = args.first().map(Value::text_of).unwrap_or_else(|| "/".to_string());
    let mut path = String::new();
    let mut used: Vec<String> = Vec::new();
    for segment in pattern.split('/') {
        if segment.starts_with('<') && segment.ends_with('>') {
            let key = segment.trim_matches(|ch| ch == '<' || ch == '>');
            let key = key.rsplit(':').next_back().unwrap_or(key).to_string();
            let value = kwargs.iter().find(|(name, _)| name.as_ref() == key.as_str())
                .map(|(_, value)| value.text_of()).unwrap_or_default();
            used.push(key);
            path.push_str(&url_encode(&value));
        } else {
            path.push_str(segment);
        }
        if !segment.is_empty() {
            path.push('/');
        }
    }
    while path.ends_with('/') && path.len() > 1 {
        path.pop();
    }
    let extra: Vec<String> = kwargs.iter()
        .filter(|(name, _)| !used.contains(&name.to_string()))
        .map(|(name, value)| format!("{}={}", url_encode(name), url_encode(&value.text_of())))
        .collect();
    if extra.is_empty() {
        Value::text(if path.is_empty() { "/" } else { &path })
    } else {
        Value::text(&format!("{}?{}", if path.is_empty() { "/" } else { &path }, extra.join("&")))
    }
}

/// run 里注入的 url_for 需要访问状态，走同一个实现。
fn url_for_with(_state: &Rc<RefCell<WebState>>, args: Vec<Value>, kwargs: Vec<(Box<str>, Value)>) -> Value {
    url_for(&args, kwargs)
}

/// `redirect(url)` → 带 Location 的响应字典。
fn redirect(args: &[Value], kwargs: Vec<(Box<str>, Value)>) -> Value {
    let target = args.first().map(Value::text_of).unwrap_or_default();
    let status = args.get(1).and_then(|v| match v { Value::Int(n) => Some(*n), _ => None })
        .or_else(|| kw_int_opt(&kwargs, "status"))
        .unwrap_or(302);
    let value = Value::dict();
    if let Value::Dict(map) = &value {
        map.borrow_mut().insert(Value::text("redirect"), Value::text(&target));
        map.borrow_mut().insert(Value::text("status"), Value::Int(status));
    }
    value
}

/// `make_response(body, status=, content_type=, headers=)`。
fn make_response(args: &[Value], kwargs: Vec<(Box<str>, Value)>) -> Value {
    let body = args.first().map(Value::text_of).unwrap_or_default();
    let status = args.get(1).and_then(|v| match v { Value::Int(n) => Some(*n), _ => None })
        .or_else(|| kw_int_opt(&kwargs, "status")).unwrap_or(200);
    let content_type = kwargs.iter().find(|(name, _)| &**name == "content_type")
        .map(|(_, value)| value.text_of()).unwrap_or_else(|| "text/html; charset=utf-8".to_string());
    let value = Value::dict();
    if let Value::Dict(map) = &value {
        let mut map = map.borrow_mut();
        map.insert(Value::text("body"), Value::text(&body));
        map.insert(Value::text("status"), Value::Int(status));
        map.insert(Value::text("content_type"), Value::text(&content_type));
    }
    value
}

/// `json_response(值)`：Content-Type 直接给 application/json。
fn json_response(args: &[Value], kwargs: Vec<(Box<str>, Value)>) -> Value {
    let text = json::dump(args.first().unwrap_or(&Value::None), false);
    let mut response = make_response(&[Value::text(&text)], Vec::new());
    if let Value::Dict(map) = &response {
        map.borrow_mut().insert(Value::text("content_type"), Value::text("application/json; charset=utf-8"));
        if let Some(status) = kwargs.iter().find(|(name, _)| &**name == "status").map(|(_, v)| v.clone()) {
            map.borrow_mut().insert(Value::text("status"), status);
        }
    }
    response
}

/// `abort(404, "说明")`：直接给一个错误页。
fn abort(args: &[Value], kwargs: Vec<(Box<str>, Value)>) -> Value {
    let status = match args.first() {
        Some(Value::Int(number)) => *number,
        _ => 400,
    };
    let message = args.get(1).map(Value::text_of)
        .or_else(|| kwargs.iter().find(|(name, _)| &**name == "message").map(|(_, v)| v.text_of()))
        .unwrap_or_else(|| default_error_text(status));
    let value = Value::dict();
    if let Value::Dict(map) = &value {
        let mut map = map.borrow_mut();
        map.insert(Value::text("status"), Value::Int(status));
        map.insert(Value::text("body"), Value::text(&format!("<h1>{status}</h1><p>{}</p>", escape_once(&message))));
    }
    value
}

/// 状态码的默认说明文字。
fn default_error_text(status: i64) -> String {
    match status {
        400 => "请求不对".to_string(),
        401 => "需要先登录".to_string(),
        403 => "没有权限".to_string(),
        404 => "内容不存在".to_string(),
        500 => "服务内部出错".to_string(),
        other => format!("错误 {other}"),
    }
}

/// 把本次请求的各个部分写进共享的请求字典。
fn fill_context(state: &Rc<RefCell<WebState>>, method: &str, path: &str, query: &str,
    form: &Value, headers: &[(String, String)], cookie: &str, body: &str, remote: &str) {
    let request = state.borrow().request.clone();
    let header_map = Value::dict();
    if let Value::Dict(map) = &header_map {
        let mut map = map.borrow_mut();
        for (name, value) in headers {
            map.insert(Value::text(name), Value::text(value));
        }
    }
    if let Value::Dict(inner) = &request {
        let mut inner = inner.borrow_mut();
        *inner = DictData::new();
        inner.insert(Value::text("method"), Value::text(method));
        inner.insert(Value::text("path"), Value::text(path));
        inner.insert(Value::text("query_string"), Value::text(query));
        inner.insert(Value::text("args"), decode_query(query));
        inner.insert(Value::text("form"), form.clone());
        inner.insert(Value::text("data"), Value::text(body));
        inner.insert(Value::text("headers"), header_map);
        inner.insert(Value::text("cookies"), parse_cookie(cookie));
        inner.insert(Value::text("remote_addr"), Value::text(remote));
    }
}

/// 路由与请求路径的分段。
fn split_path(path: &str) -> Vec<String> {
    path.split('/').filter(|segment| !segment.is_empty()).map(|segment| segment.to_string()).collect()
}

/// 归一化注册写法：补前导斜杠、去尾斜杠。
fn normalize_pattern(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.starts_with('/') { trimmed.to_string() } else { format!("/{trimmed}") }
}

/// 路径匹配：`<name>` 取任意一段，`<int:name>` 只接受整数。
fn match_path(pattern: &[String], target: &[String]) -> Option<Vec<(String, Value)>> {
    if pattern.len() != target.len() {
        return None;
    }
    let mut params = Vec::new();
    for (slot, actual) in pattern.iter().zip(target) {
        if slot.starts_with('<') && slot.ends_with('>') {
            let inner = &slot[1..slot.len() - 1];
            let (kind, name) = match inner.split_once(':') {
                Some((kind, name)) => (kind, name),
                None => ("str", inner),
            };
            let value = match kind {
                "int" => match actual.parse::<i64>() {
                    Ok(number) => Value::Int(number),
                    Err(_) => return None,
                },
                _ => Value::text(&url_decode(actual)),
            };
            params.push((name.to_string(), value));
        } else if slot != actual {
            return None;
        }
    }
    Some(params)
}

/// `/static/**` 的落盘解析，禁止越出静态目录。
fn static_file(root: &PathBuf, path: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix("/static/")?;
    let mut target = root.join("static");
    for segment in rest.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
        target.push(segment);
    }
    if target.is_file() { Some(target) } else { None }
}

/// 读一个静态文件，附带 Content-Type。
fn serve_file(path: &PathBuf) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = std::fs::read(path).unwrap_or_default();
    let type_name = mime_of(&path.extension().map(|name| name.to_string_lossy().to_string()).unwrap_or_default());
    let reply = Reply {
        status: if body.is_empty() { 404 } else { 200 },
        content_type: type_name.to_string(),
        body,
        headers: Vec::new(),
    };
    build_response(reply.status, reply)
}

/// 按扩展名给 Content-Type，只覆盖 CMS 用得上的几种。
fn mime_of(extension: &str) -> &'static str {
    match extension {
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// `application/x-www-form-urlencoded` → 字典，同名键后者覆盖前者。
pub fn decode_form(body: &str) -> Value {
    decode_query(body)
}

/// 查询串解析：`a=1&b=%E4%B8%AD`，值做 URL 解码，`+` 当空格。
pub fn decode_query(text: &str) -> Value {
    let value = Value::dict();
    if let Value::Dict(map) = &value {
        let mut map = map.borrow_mut();
        for pair in text.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
            map.insert(Value::text(&url_decode(key)), Value::text(&url_decode(raw)));
        }
    }
    value
}

/// Cookie 头解析：`a=1; b=2`。
pub fn parse_cookie(text: &str) -> Value {
    let value = Value::dict();
    if let Value::Dict(map) = &value {
        let mut map = map.borrow_mut();
        for pair in text.split(';') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
            map.insert(Value::text(key.trim()), Value::text(raw.trim()));
        }
    }
    value
}

/// 百分号编码，保留 RFC 3986 的非保留字符集。
pub fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 百分号解码，非法转义按原样保留，`+` 还原成空格。
pub fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => { out.push(b' '); index += 1; }
            b'%' if index + 2 < bytes.len() => {
                let hex = &text[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => { out.push(byte); index += 3; }
                    Err(_) => { out.push(bytes[index]); index += 1; }
                }
            }
            byte => { out.push(byte); index += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// HTML 转义。
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

/// 单行转义，错误页用。
fn escape_once(text: &str) -> String {
    let mut out = String::new();
    escape_html(text, &mut out);
    out
}

/// 关键字参取文本。
fn kw_text(kwargs: &[(Box<str>, Value)], name: &str, fallback: &str) -> String {
    kwargs.iter().find(|(key, _)| key.as_ref() == name)
        .map(|(_, value)| value.text_of()).unwrap_or_else(|| fallback.to_string())
}

/// 关键字参取整数。
fn kw_int(kwargs: &[(Box<str>, Value)], name: &str, fallback: i64) -> i64 {
    kwargs.iter().find(|(key, _)| key.as_ref() == name)
        .and_then(|(_, value)| match value { Value::Int(number) => Some(*number), _ => None })
        .unwrap_or(fallback)
}

/// 关键字参取可选整数。
fn kw_int_opt(kwargs: &[(Box<str>, Value)], name: &str) -> Option<i64> {
    kwargs.iter().find(|(key, _)| key.as_ref() == name)
        .and_then(|(_, value)| match value { Value::Int(number) => Some(*number), _ => None })
}

/// 关键字参取布尔，缺省用 fallback。
fn kw_bool(kwargs: &[(Box<str>, Value)], name: &str, fallback: bool) -> bool {
    kwargs.iter().find(|(key, _)| key.as_ref() == name)
        .map(|(_, value)| value.truthy()).unwrap_or(fallback)
}
