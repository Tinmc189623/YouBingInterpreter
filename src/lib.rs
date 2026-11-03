//! 油饼语言（YouBing / .yb）解释器的库入口。
//! 分层：error 诊断 → lex 词法 → ast 语法树 → parse 语法分析 →
//! value 值模型 → bytecode 指令 → compile 字节码生成 → vm 栈式虚拟机 →
//! host 宿主绑定（std / os / xorweb）→ template 模板 → web 服务 →
//! tier 分层升级 → jit Cranelift 后端。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

pub mod error;
pub mod ast;
pub mod lex;
pub mod parse;
pub mod value;
pub mod bytecode;
pub mod compile;
pub mod json;
pub mod vm;
pub mod host;
pub mod template;
pub mod web;
pub mod tier;
pub mod jit;

pub use bytecode::{Instr, Op, Proto};
pub use error::{CompileError, Pos, RunError};
pub use value::Value;
pub use vm::Vm;

/// 只跑前端（词法 + 语法），把诊断文本原样返回，供 `youbing check` 使用。
///
/// 输入是文件路径与源码文本；返回解析出的模块或首个诊断错误。
pub fn check(path: &str, text: &str) -> Result<ast::Module, CompileError> {
    parse::parse_file(path, text)
}

/// 前端 + 字节码生成，返回顶层原型（含嵌套函数）的反汇编文本。
///
/// 供 `youbing dump` 使用，语法或语义错误按诊断文本返回。
pub fn dump(path: &str, text: &str) -> Result<String, CompileError> {
    let module = check(path, text)?;
    let proto = compile::compile_module(&module)?;
    Ok(proto.disassemble())
}

/// 跑一个入口脚本：`root` 是项目根（模块导入与文件访问的围栏），`script` 是入口文件，
/// `args` 原样交给 `os.args`，`use_jit` 关掉时只走解释器，用于 VM/JIT 差分对比。
///
/// 返回进程的退出码（`exit(n)` 或 0）。
pub fn run_file(root: &std::path::Path, script: &std::path::Path, args: &[String],
    use_jit: bool) -> Result<i32, RunError> {
    let mut vm = Vm::new(root.to_path_buf(), args.to_vec());
    vm.jit_on = use_jit;
    vm.run_file(script)
}
