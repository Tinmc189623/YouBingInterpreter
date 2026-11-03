//! 命令行入口：`check` 只做前端诊断，`run` 跑一个项目，`dump` 出字节码，`bench` 做解释/JIT 对照。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use youbing::{jit, Vm};

/// 一条命令解析后的结果：入口脚本、围栏根目录、脚本参数与是否启用 JIT。
struct Options {
    script: PathBuf,
    root: PathBuf,
    args: Vec<String>,
    jit: bool,
    repeat: u32,
    stats: bool,
}

/// 打印用法。
fn usage() {
    eprintln!("用法：youbing <命令> [选项]");
    eprintln!("  run   <入口.yb> [--root 目录] [--no-jit] [--stats] [-- 脚本参数...]   运行项目");
    eprintln!("  check <文件.yb>...                                                    只查词法与语法");
    eprintln!("  dump  <入口.yb> [--root 目录]                                         反汇编字节码");
    eprintln!("  bench <入口.yb> [--root 目录] [--repeat 次数]                         解释器与 JIT 对照计时");
}

/// 读取文件（自动去掉 UTF-8 BOM），失败时返回错误文本。
fn read_text(path: &str) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("{path}: 读取失败：{error}"))?;
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    String::from_utf8(body.to_vec()).map_err(|_| format!("{path}: 不是合法的 UTF-8 源码"))
}

/// 从命令行里挑出 `--root`，没给就用当前工作目录当围栏根。
fn take_root(iter: &mut Vec<String>) -> PathBuf {
    if let Some(index) = iter.iter().position(|arg| arg == "--root") {
        iter.remove(index);
        if index < iter.len() {
            let value = iter.remove(index);
            return PathBuf::from(value);
        }
        eprintln!("--root 后面要跟一个目录");
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 解析 run/dump/bench 共用的选项：脚本路径、`--root`、`--no-jit`、`--repeat`，
/// `--` 之后的内容原样作为脚本参数交给 `os.args`，不再参与选项解析。
fn parse_options(rest: &[String]) -> Option<Options> {
    let separator = rest.iter().position(|arg| arg == "--");
    let (flags, script_args) = match separator {
        Some(index) => (rest[..index].to_vec(), rest[index + 1..].to_vec()),
        None => (rest.clone(), Vec::new()),
    };
    let mut items = flags;
    let mut jit_on = true;
    if let Some(index) = items.iter().position(|arg| arg == "--no-jit") {
        items.remove(index);
        jit_on = false;
    }
    let mut stats = false;
    if let Some(index) = items.iter().position(|arg| arg == "--stats") {
        items.remove(index);
        stats = true;
    }
    let mut repeat = 1u32;
    if let Some(index) = items.iter().position(|arg| arg == "--repeat") {
        items.remove(index);
        if index < items.len() {
            let value = items.remove(index);
            repeat = value.parse::<u32>().unwrap_or(1);
        }
    }
    let root = take_root(&mut items);
    let script = items.first().cloned().map(PathBuf::from)?;
    Some(Options { script, root, args: script_args, jit: jit_on, repeat, stats })
}

/// 逐个文件跑前端检查，任一失败即返回失败退出码。
fn check_files(paths: &[String]) -> ExitCode {
    let mut failed = 0usize;
    for path in paths {
        let text = match read_text(path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("{error}");
                failed += 1;
                continue;
            }
        };
        match youbing::check(path, &text) {
            Ok(module) => println!("{path}: 语法通过（{} 条顶层语句）", module.stmts.len()),
            Err(error) => {
                eprintln!("{path}: {error}");
                failed += 1;
            }
        }
    }
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// 跑一个入口脚本并返回退出码；`--stats` 时顺带打印执行统计。
fn execute(options: &Options) -> ExitCode {
    let mut vm = Vm::new(options.root.clone(), options.args.clone());
    vm.jit_on = options.jit;
    let started = Instant::now();
    match vm.run_file(&options.script) {
        Ok(code) => {
            if options.stats {
                let elapsed = started.elapsed();
                let micros = elapsed.as_micros().max(1) as f64;
                println!("统计：{} 条指令，耗时 {:.1} ms（{:.0} 条/秒），JIT 编译 {} 个函数、本机调用 {} 次，本机码 {} 字节",
                    vm.executed, micros / 1000.0, vm.executed as f64 / (micros * 1e-6),
                    vm.jit_compiles, vm.jit_calls, jit::code_bytes());
            }
            ExitCode::from(code.clamp(0, 255) as u8)
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// 同一入口分别用解释器和 JIT 各跑若干遍，对比耗时并校验退出码一致，给出加速比。
fn bench(options: &Options) -> ExitCode {
    let repeat = options.repeat.max(1);
    let mut interpreted = 0u128;
    let mut compiled = 0u128;
    let mut jit_compiles = 0u32;
    let mut jit_calls = 0u64;
    let mut expected = 0i32;
    for _ in 0..repeat {
        let mut vm = Vm::new(options.root.clone(), options.args.clone());
        vm.jit_on = false;
        let started = Instant::now();
        match vm.run_file(&options.script) {
            Ok(code) => {
                expected = code;
                interpreted += started.elapsed().as_micros();
            }
            Err(error) => {
                eprintln!("解释执行失败：{error}");
                return ExitCode::FAILURE;
            }
        }
        let mut vm = Vm::new(options.root.clone(), options.args.clone());
        vm.jit_on = true;
        let started = Instant::now();
        match vm.run_file(&options.script) {
            Ok(code) => {
                if code != expected {
                    eprintln!("JIT 与解释器退出码不一致：{code} != {expected}");
                    return ExitCode::FAILURE;
                }
                compiled += started.elapsed().as_micros();
                jit_compiles += vm.jit_compiles;
                jit_calls += vm.jit_calls;
            }
            Err(error) => {
                eprintln!("JIT 执行失败：{error}");
                return ExitCode::FAILURE;
            }
        }
    }
    let times = repeat as u128;
    let avg_interp = interpreted / times;
    let avg_jit = compiled / times;
    let ratio = avg_interp as f64 / avg_jit.max(1) as f64;
    println!("基准（{repeat} 遍平均）：解释 {avg_interp} µs，JIT {avg_jit} µs，加速 {ratio:.2}×");
    println!("JIT 累计编译 {jit_compiles} 个函数、本机调用 {jit_calls} 次，本机码 {} 字节", jit::code_bytes());
    ExitCode::SUCCESS
}

/// 反汇编入口脚本的字节码。
fn dump_script(options: &Options) -> ExitCode {
    let path = options.script.display().to_string();
    match read_text(&path).and_then(|text| youbing::dump(&path, &text).map_err(|error| error.to_string())) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// 入口脚本必须真实存在，否则直接报路径。
fn script_exists(path: &Path) -> bool {
    path.is_file()
}

fn main() -> ExitCode {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = all.first().map(String::as_str) else {
        usage();
        return ExitCode::FAILURE;
    };
    let rest = &all[1..];
    match command {
        "check" if !rest.is_empty() => check_files(rest),
        "run" | "dump" | "bench" => {
            let Some(options) = parse_options(rest) else {
                usage();
                return ExitCode::FAILURE;
            };
            if !script_exists(&options.script) {
                eprintln!("找不到脚本：{}", options.script.display());
                return ExitCode::FAILURE;
            }
            match command {
                "run" => execute(&options),
                "dump" => dump_script(&options),
                _ => bench(&options),
            }
        }
        _ => {
            usage();
            ExitCode::FAILURE
        }
    }
}
