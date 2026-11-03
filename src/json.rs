//! JSON：自研的递归下降编解码，直接对着油饼值模型读写，不引第三方依赖。
//! 数字整值折成 Int、带小数折成 Float；对象保插入序，写回时顺序稳定。
//!
//! Copyright © 2026 Tinmc189623 All Rights Reserved

use crate::error::RunError;
use crate::value::{DictData, Value};

/// 解析 JSON 文本。
pub fn parse(text: &str) -> Result<Value, RunError> {
    let chars: Vec<char> = text.chars().collect();
    let mut reader = Reader { chars, at: 0 };
    reader.skip_ws();
    let value = reader.value()?;
    reader.skip_ws();
    if reader.at != reader.chars.len() {
        return Err(RunError::Unsupported(format!("JSON 第 {} 个字符之后还有多余内容", reader.at + 1)));
    }
    Ok(value)
}

/// 序列化；pretty 为真时每层缩进两个空格。
pub fn dump(value: &Value, pretty: bool) -> String {
    let mut out = String::new();
    write_value(&mut out, value, pretty, 0);
    out
}

struct Reader {
    chars: Vec<char>,
    at: usize,
}

impl Reader {
    /// 当前字符。
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    /// 跳空白。
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\n') | Some('\r')) {
            self.at += 1;
        }
    }

    /// 期望一个字符。
    fn expect(&mut self, ch: char) -> Result<(), RunError> {
        if self.peek() == Some(ch) {
            self.at += 1;
            Ok(())
        } else {
            Err(RunError::Unsupported(format!("JSON 第 {} 个字符该是 {ch}", self.at + 1)))
        }
    }

    /// 一个字面量词。
    fn literal(&mut self, word: &str) -> bool {
        if self.chars[self.at..].iter().copied().take(word.chars().count()).eq(word.chars()) {
            self.at += word.chars().count();
            return true;
        }
        false
    }

    /// 任意值。
    fn value(&mut self) -> Result<Value, RunError> {
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => Ok(Value::text(&self.string()?)),
            Some('t') if self.literal("true") => Ok(Value::Bool(true)),
            Some('f') if self.literal("false") => Ok(Value::Bool(false)),
            Some('n') if self.literal("null") => Ok(Value::None),
            Some(ch) if ch == '-' || ch.is_ascii_digit() => self.number(),
            other => Err(RunError::Unsupported(format!("JSON 第 {} 个字符不是合法值起点：{other:?}", self.at + 1))),
        }
    }

    /// 对象：键顺序即插入顺序。
    fn object(&mut self) -> Result<Value, RunError> {
        self.expect('{')?;
        let map = Value::dict();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.at += 1;
            return Ok(map);
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(':')?;
            self.skip_ws();
            let value = self.value()?;
            if let Value::Dict(inner) = &map {
                inner.borrow_mut().insert(Value::text(&key), value);
            }
            self.skip_ws();
            match self.peek() {
                Some(',') => self.at += 1,
                Some('}') => {
                    self.at += 1;
                    return Ok(map);
                }
                other => return Err(RunError::Unsupported(format!(
                    "JSON 第 {} 个字符 expected , 或 }}，实际 {other:?}", self.at + 1))),
            }
        }
    }

    /// 数组。
    fn array(&mut self) -> Result<Value, RunError> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.at += 1;
            return Ok(Value::list(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => self.at += 1,
                Some(']') => {
                    self.at += 1;
                    return Ok(Value::list(items));
                }
                other => return Err(RunError::Unsupported(format!(
                    "JSON 第 {} 个字符 expected , 或 ]，实际 {other:?}", self.at + 1))),
            }
        }
    }

    /// 字符串，支持 \u 转义。
    fn string(&mut self) -> Result<String, RunError> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            let ch = self.peek().ok_or_else(|| RunError::Unsupported(
                format!("JSON 第 {} 个字符之后字符串没有闭合", self.at + 1)))?;
            self.at += 1;
            match ch {
                '"' => return Ok(out),
                '\\' => {
                    let esc = self.peek().ok_or_else(|| RunError::Unsupported("JSON 反斜杠后面缺字符".into()))?;
                    self.at += 1;
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => out.push(self.unicode_escape()?),
                        other => return Err(RunError::Unsupported(format!("JSON 不认的转义 \\{other}"))),
                    }
                }
                _ => out.push(ch),
            }
        }
    }

    /// 4 位十六进制转义，遇到代理对时补上第二个码元。
    fn unicode_escape(&mut self) -> Result<char, RunError> {
        let first = self.hex4()?;
        if !(0xD800..0xDC00).contains(&first) {
            return char::from_u32(first).ok_or_else(|| RunError::Unsupported("JSON \\u 码点不合法".into()));
        }
        if self.peek() == Some('\\') {
            self.at += 1;
            self.expect('u')?;
            let second = self.hex4()?;
            let combined = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
            return char::from_u32(combined).ok_or_else(|| RunError::Unsupported("JSON 代理对不合法".into()));
        }
        Err(RunError::Unsupported("JSON 高位代理后面缺低位代理".into()))
    }

    /// 4 位十六进制。
    fn hex4(&mut self) -> Result<u32, RunError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let ch = self.peek().ok_or_else(|| RunError::Unsupported("JSON \\u 只有不到 4 位".into()))?;
            self.at += 1;
            let digit = ch.to_digit(16).ok_or_else(|| RunError::Unsupported(format!("JSON \\u 里出现 {ch}")))?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// 数字：整值走 Int，其余走 Float。
    fn number(&mut self) -> Result<Value, RunError> {
        let start = self.at;
        if self.peek() == Some('-') {
            self.at += 1;
        }
        let mut is_float = false;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                self.at += 1;
            } else if ch == '.' || ch == 'e' || ch == 'E' || ch == '+' || ch == '-' {
                is_float = is_float || ch == '.' || ch == 'e' || ch == 'E';
                self.at += 1;
            } else {
                break;
            }
        }
        let text: String = self.chars[start..self.at].iter().collect();
        if !is_float {
            if let Ok(whole) = text.parse::<i64>() {
                return Ok(Value::Int(whole));
            }
        }
        text.parse::<f64>().map(Value::Float)
            .map_err(|_| RunError::Unsupported(format!("JSON 数字 {text} 读不出来")))
    }
}

/// 递归写一个值。
fn write_value(out: &mut String, value: &Value, pretty: bool, depth: usize) {
    match value {
        Value::None => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(number) => out.push_str(&number.to_string()),
        Value::Float(float) => out.push_str(&float.to_string()),
        Value::Str(text) => write_string(out, text),
        Value::List(items) => {
            let items = items.borrow();
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline_indent(out, pretty, depth + 1);
                write_value(out, item, pretty, depth + 1);
            }
            newline_indent(out, pretty, depth);
            out.push(']');
        }
        Value::Dict(map) => {
            let map = map.borrow();
            if map.pairs.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (index, (key, item)) in map.pairs.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline_indent(out, pretty, depth + 1);
                write_string(out, &key.text_of());
                out.push(':');
                if pretty {
                    out.push(' ');
                }
                write_value(out, item, pretty, depth + 1);
            }
            newline_indent(out, pretty, depth);
            out.push('}');
        }
        other => write_string(out, &other.text_of()),
    }
}

/// pretty 模式下的换行与缩进。
fn newline_indent(out: &mut String, pretty: bool, depth: usize) {
    if !pretty {
        return;
    }
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// 带转义的字符串。
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// 字典数据的默认构造，供上层复用。
pub fn empty_dict_data() -> DictData {
    DictData::new()
}
