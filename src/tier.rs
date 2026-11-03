//! 分层升级：解释执行累计到阈值，且类型反馈仍是纯整数时，才允许进 JIT。
//! 这里只做便宜的静态白名单判定（指令集合、常量表）；需要栈形与定值分析的严格判定放在 jit 里，
//! 那边编不出来就把函数拉黑、永久退回解释，保证 JIT 与 VM 的结果永远一致。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use crate::bytecode::{BinOp, Op, Proto, UnOp};
use crate::value::{tag, Value};

/// 触发编译的调用次数：太小会把一次性函数也编了，太大基准看不出差别。
pub const CALL_THRESHOLD: u32 = 48;

/// 类型反馈里"这个槽位见过不止一种类型"的标记，与任何 tag 都不相等。
pub const MIXED: u8 = 255;

/// 纯整数子集白名单判定：只允许不产生堆对象、不抛异常、不跨块携带栈值的指令。
///
/// 除法与取余被排除：VM 里除零是运行期错误，本机码没有等价的报错通道。
pub fn eligible(proto: &Proto) -> bool {
    !proto.code.is_empty()
        && proto.protos.is_empty()
        && proto.code.iter().all(|instr| match instr.op {
            Op::Const(index) => matches!(proto.consts.get(index as usize), Some(Value::Int(_))),
            Op::Pop | Op::Dup | Op::Swap | Op::LoadLocal(_) | Op::StoreLocal(_) => true,
            Op::Unary(UnOp::Neg) => true,
            Op::Binary(op) => {
                matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul
                    | BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
            }
            Op::Jump(_) | Op::JumpIfFalse(_) | Op::JumpIfTrue(_)
            | Op::JumpIfFalsePeek(_) | Op::JumpIfTruePeek(_) | Op::Return => true,
            _ => false,
        })
}

/// 不达标原因，`youbing jitinfo` 与调试用；达标返回 None。
pub fn reject_reason(proto: &Proto) -> Option<&'static str> {
    if proto.code.is_empty() {
        return Some("空函数体");
    }
    if !proto.protos.is_empty() {
        return Some("有嵌套函数定义");
    }
    for instr in &proto.code {
        let reason = match instr.op {
            Op::Const(index) => match proto.consts.get(index as usize) {
                Some(Value::Int(_)) => continue,
                Some(_) => "常量不是整数",
                None => "常量下标越界",
            },
            Op::Binary(BinOp::Div) | Op::Binary(BinOp::Mod) => "整数子集不含除法和取余",
            Op::Unary(UnOp::Not) => "整数子集不含布尔取非",
            Op::LoadName(_) | Op::StoreName(_) => "有全局名访问",
            Op::Call { .. } | Op::CallMethod { .. } => "有函数调用",
            Op::GetAttr(_) | Op::SetAttr(_) | Op::GetIndex | Op::SetIndex => "有属性或下标访问",
            Op::BuildList(_) | Op::JoinStr(_) => "有容器或字符串构造",
            Op::IterStart | Op::IterNext { .. } | Op::IterPop => "有 for 迭代",
            Op::MakeFunc(_) | Op::ImportModule(_) | Op::ImportFrom { .. } => "有函数构造或导入",
            _ => continue,
        };
        return Some(reason);
    }
    None
}

/// 实参是否全为整数，VM 的破供判定与 JIT 入参打包共用。
pub fn all_int(args: &[Value]) -> bool {
    args.iter().all(|value| value.tag() == tag::INT)
}
