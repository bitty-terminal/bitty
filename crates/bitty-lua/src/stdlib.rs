//! Retained restricted-stdlib additions piccolo does not ship.
//!
//! The accepted [Lua Runtime RFC] baseline keeps a pure-computation base
//! (`string`, `table`, `math`, `utf8`, basic functions) for every VM. Piccolo's
//! `Lua::core()` ships a much smaller subset (for example `string.len/sub/upper/
//! lower/reverse` and `table.pack/unpack`), so this module installs the
//! missing, I/O-free functions the accepted baseline requires: `utf8.len/char/
//! codepoint`, `string.byte/char/format`, `table.concat`, and `table.sort`.
//! Nothing here grants ambient authority; every function only inspects or
//! builds bounded in-VM strings/tables, and `string.format` refuses unknown
//! conversions and caps its output.
//!
//! [Lua Runtime RFC]: ../../../../bitty-docs/docs/specifications/lua-runtime-rfc.md

use piccolo::{
    Callback, CallbackReturn, Closure, Context, Error, Executor, IntoValue, Lua, Table, Value,
    Variadic,
};

/// Maximum bytes produced by one `string.format` call.
pub const STRING_FORMAT_MAX_BYTES: usize = 64 * 1024;
/// Maximum width/precision accepted by `string.format`.
const FORMAT_MAX_WIDTH: usize = 4096;

/// Install the retained stdlib functions into `lua`.
pub(crate) fn install_retained_stdlib(lua: &mut Lua) {
    lua.enter(|ctx| {
        install_utf8(ctx);
        install_string(ctx);
        install_table(ctx);
        install_os(ctx);
    });
    install_table_sort(lua);
}

/// Restricted `os` retained by the accepted baseline: `time`/`clock`/`date`
/// only. `os.execute`, `os.getenv`, `os.remove`, `os.tmpname`, and the rest are
/// deliberately absent (no process, environment, or filesystem authority).
fn install_os<'gc>(ctx: Context<'gc>) {
    static PROCESS_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let os = Table::new(&ctx);
    os.set(
        ctx,
        "time",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0);
            stack.replace(ctx, Value::Integer(now));
            Ok(CallbackReturn::Return)
        }),
    )
    .expect("os accepts 'time'");
    os.set(
        ctx,
        "clock",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let start = PROCESS_START.get_or_init(std::time::Instant::now);
            stack.replace(ctx, Value::Number(start.elapsed().as_secs_f64()));
            Ok(CallbackReturn::Return)
        }),
    )
    .expect("os accepts 'clock'");
    os.set(
        ctx,
        "date",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let format = match stack.get(0) {
                Value::Nil => "%c".to_string(),
                Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                _ => {
                    return Err("bad argument #1 to 'date' (string expected)"
                        .into_value(ctx)
                        .into());
                }
            };
            let seconds = stack.get(1).to_integer().unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs() as i64)
                    .unwrap_or(0)
            });
            let fields = civil_from_unix(seconds);
            if format == "*t" || format == "!*t" {
                let table = Table::new(&ctx);
                let _ = table.set(ctx, "year", fields.year);
                let _ = table.set(ctx, "month", fields.month);
                let _ = table.set(ctx, "day", fields.day);
                let _ = table.set(ctx, "hour", fields.hour);
                let _ = table.set(ctx, "min", fields.minute);
                let _ = table.set(ctx, "sec", fields.second);
                let _ = table.set(ctx, "wday", fields.weekday);
                let _ = table.set(ctx, "yday", fields.year_day);
                let _ = table.set(ctx, "isdst", false);
                stack.replace(ctx, Value::Table(table));
            } else {
                stack.replace(ctx, ctx.intern(format_date(&format, &fields).as_bytes()));
            }
            Ok(CallbackReturn::Return)
        }),
    )
    .expect("os accepts 'date'");
    ctx.set_global("os", os).expect("globals accept 'os'");
}

struct DateFields {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    weekday: i64,
    year_day: i64,
}

/// Convert Unix seconds (UTC) to calendar fields (Hinnant's civil algorithm).
fn civil_from_unix(seconds: i64) -> DateFields {
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let hour = remainder / 3_600;
    let minute = (remainder % 3_600) / 60;
    let second = remainder % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let weekday = (days.rem_euclid(7) + 4) % 7 + 1; // 1 = Sunday
    let year_day = days - days_from_civil(year, 1, 1) + 1;
    DateFields {
        year,
        month,
        day,
        hour,
        minute,
        second,
        weekday,
        year_day,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn format_date(format: &str, fields: &DateFields) -> String {
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{:04}", fields.year)),
            Some('m') => out.push_str(&format!("{:02}", fields.month)),
            Some('d') => out.push_str(&format!("{:02}", fields.day)),
            Some('H') => out.push_str(&format!("{:02}", fields.hour)),
            Some('M') => out.push_str(&format!("{:02}", fields.minute)),
            Some('S') => out.push_str(&format!("{:02}", fields.second)),
            Some('j') => out.push_str(&format!("{:03}", fields.year_day)),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

fn install_utf8<'gc>(ctx: Context<'gc>) {
    let utf8 = Table::new(&ctx);
    utf8.set(
        ctx,
        "len",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let Value::String(text) = stack.get(0) else {
                return Err("bad argument #1 to 'len' (string expected)"
                    .into_value(ctx)
                    .into());
            };
            if !stack.get(1).is_nil() || !stack.get(2).is_nil() {
                return Err("utf8.len i/j bounds are not supported"
                    .into_value(ctx)
                    .into());
            }
            match std::str::from_utf8(text.as_bytes()) {
                Ok(decoded) => {
                    stack.replace(ctx, Value::Integer(decoded.chars().count() as i64));
                }
                Err(error) => {
                    stack.replace(
                        ctx,
                        Variadic(vec![
                            Value::Nil,
                            Value::Integer(error.valid_up_to() as i64 + 1),
                        ]),
                    );
                }
            }
            Ok(CallbackReturn::Return)
        }),
    )
    .expect("utf8 accepts 'len'");
    utf8.set(
        ctx,
        "char",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let mut out = String::new();
            for index in 0..stack.len() {
                let Some(scalar) = stack.get(index).to_integer() else {
                    return Err("bad argument to 'char' (number expected)"
                        .into_value(ctx)
                        .into());
                };
                let Ok(scalar) = u32::try_from(scalar) else {
                    return Err("value out of range for 'char'".into_value(ctx).into());
                };
                let Some(ch) = char::from_u32(scalar) else {
                    return Err("value out of range for 'char'".into_value(ctx).into());
                };
                out.push(ch);
            }
            stack.replace(ctx, ctx.intern(out.as_bytes()));
            Ok(CallbackReturn::Return)
        }),
    )
    .expect("utf8 accepts 'char'");
    utf8.set(
        ctx,
        "codepoint",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let Value::String(text) = stack.get(0) else {
                return Err("bad argument #1 to 'codepoint' (string expected)"
                    .into_value(ctx)
                    .into());
            };
            let Ok(decoded) = std::str::from_utf8(text.as_bytes()) else {
                return Err("invalid UTF-8 code".into_value(ctx).into());
            };
            let values: Vec<Value> = decoded
                .chars()
                .map(|ch| Value::Integer(ch as i64))
                .collect();
            stack.replace(ctx, Variadic(values));
            Ok(CallbackReturn::Return)
        }),
    )
    .expect("utf8 accepts 'codepoint'");
    ctx.set_global("utf8", utf8).expect("globals accept 'utf8'");
}

fn install_string<'gc>(ctx: Context<'gc>) {
    let string = match ctx.get_global("string") {
        Value::Table(table) => table,
        _ => Table::new(&ctx),
    };
    string
        .set(
            ctx,
            "byte",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let Value::String(text) = stack.get(0) else {
                    return Err("bad argument #1 to 'byte' (string expected)"
                        .into_value(ctx)
                        .into());
                };
                let bytes = text.as_bytes();
                let length = bytes.len() as i64;
                let start = stack.get(1).to_integer().unwrap_or(1);
                let end = stack.get(2).to_integer().unwrap_or(start);
                let normalize = |index: i64| -> i64 {
                    if index < 0 {
                        length + index + 1
                    } else if index == 0 {
                        1
                    } else {
                        index
                    }
                };
                let start = normalize(start).clamp(1, length.max(1));
                let end = normalize(end).clamp(start, length.max(1));
                let values: Vec<Value> = if length == 0 || start > length {
                    Vec::new()
                } else {
                    (start..=end)
                        .map(|index| Value::Integer(bytes[(index - 1) as usize] as i64))
                        .collect()
                };
                stack.replace(ctx, Variadic(values));
                Ok(CallbackReturn::Return)
            }),
        )
        .expect("string accepts 'byte'");
    string
        .set(
            ctx,
            "char",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let mut bytes = Vec::with_capacity(stack.len());
                for index in 0..stack.len() {
                    let Some(value) = stack.get(index).to_integer() else {
                        return Err("bad argument to 'char' (number expected)"
                            .into_value(ctx)
                            .into());
                    };
                    if !(0..=255).contains(&value) {
                        return Err("value out of range for 'char'".into_value(ctx).into());
                    }
                    bytes.push(value as u8);
                }
                stack.replace(ctx, ctx.intern(&bytes));
                Ok(CallbackReturn::Return)
            }),
        )
        .expect("string accepts 'char'");
    string
        .set(
            ctx,
            "format",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let Value::String(format) = stack.get(0) else {
                    return Err("bad argument #1 to 'format' (string expected)"
                        .into_value(ctx)
                        .into());
                };
                let format = String::from_utf8_lossy(format.as_bytes()).into_owned();
                let rendered = match format_string(&format, &stack) {
                    Ok(rendered) => rendered,
                    Err(message) => {
                        let value: Value = message.into_value(ctx);
                        return Err(Error::from(value));
                    }
                };
                stack.replace(ctx, ctx.intern(rendered.as_bytes()));
                Ok(CallbackReturn::Return)
            }),
        )
        .expect("string accepts 'format'");
    ctx.set_global("string", string)
        .expect("globals accept 'string'");
}

fn install_table<'gc>(ctx: Context<'gc>) {
    let table = match ctx.get_global("table") {
        Value::Table(table) => table,
        _ => Table::new(&ctx),
    };
    table
        .set(
            ctx,
            "concat",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let Value::Table(list) = stack.get(0) else {
                    return Err("bad argument #1 to 'concat' (table expected)"
                        .into_value(ctx)
                        .into());
                };
                let separator = match stack.get(1) {
                    Value::Nil => String::new(),
                    Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                    _ => {
                        return Err("bad argument #2 to 'concat' (string expected)"
                            .into_value(ctx)
                            .into());
                    }
                };
                let length = list.length();
                let start = stack.get(2).to_integer().unwrap_or(1);
                let end = stack.get(3).to_integer().unwrap_or(length);
                let mut out = String::new();
                let mut first = true;
                for index in start..=end {
                    if !first {
                        out.push_str(&separator);
                    }
                    first = false;
                    match list.get(ctx, index) {
                        Value::String(s) => out.push_str(&String::from_utf8_lossy(s.as_bytes())),
                        Value::Integer(i) => out.push_str(&i.to_string()),
                        Value::Number(n) => out.push_str(&n.to_string()),
                        _ => return Err("invalid value in 'concat' list".into_value(ctx).into()),
                    }
                    if out.len() > STRING_FORMAT_MAX_BYTES {
                        return Err("concat result exceeds the byte ceiling"
                            .into_value(ctx)
                            .into());
                    }
                }
                stack.replace(ctx, ctx.intern(out.as_bytes()));
                Ok(CallbackReturn::Return)
            }),
        )
        .expect("table accepts 'concat'");
    ctx.set_global("table", table)
        .expect("globals accept 'table'");
}

/// Inject `table.sort` as trusted Lua so a Lua comparator can be invoked
/// directly (a Rust callback cannot synchronously call back into the VM).
fn install_table_sort(lua: &mut Lua) {
    const TABLE_SORT_LUA: &str = r#"
table.sort = function(list, comp)
  local n = #list
  if comp == nil then
    comp = function(a, b) return a < b end
  end
  for i = 2, n do
    local value = list[i]
    local j = i - 1
    while j >= 1 and comp(value, list[j]) do
      list[j + 1] = list[j]
      j = j - 1
    end
    list[j + 1] = value
  end
end
"#;
    let stashed = lua.enter(|ctx| {
        let closure = Closure::load(ctx, Some("=bitty-stdlib"), TABLE_SORT_LUA.as_bytes())
            .expect("trusted stdlib chunk compiles");
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    lua.finish(&stashed);
}

fn format_string<'gc>(
    format: &str,
    stack: &piccolo::Stack<'gc, '_>,
) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    let mut argument = 1usize;
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some(conversion) if conversion.is_ascii_alphabetic() => {
                let mut flags = String::new();
                while let Some(&flag) = chars.peek() {
                    if matches!(flag, '-' | '+' | ' ' | '#' | '0') {
                        flags.push(flag);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let mut width = String::new();
                while let Some(&digit) = chars.peek() {
                    if digit.is_ascii_digit() && width.len() < 6 {
                        width.push(digit);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let mut precision: Option<usize> = None;
                if chars.peek() == Some(&'.') {
                    chars.next();
                    let mut digits = String::new();
                    while let Some(&digit) = chars.peek() {
                        if digit.is_ascii_digit() && digits.len() < 6 {
                            digits.push(digit);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    precision = digits.parse::<usize>().ok();
                }
                let value = stack.get(argument);
                argument += 1;
                let rendered = render_conversion(conversion, &flags, precision, value)
                    .ok_or("unsupported format conversion or argument")?;
                let width = width.parse::<usize>().unwrap_or(0).min(FORMAT_MAX_WIDTH);
                out.push_str(&pad(&rendered, width, flags.contains('-')));
            }
            _ => return Err("invalid format string"),
        }
        if out.len() > STRING_FORMAT_MAX_BYTES {
            return Err("format result exceeds the byte ceiling");
        }
    }
    Ok(out)
}

fn render_conversion(
    conversion: char,
    _flags: &str,
    precision: Option<usize>,
    value: Value<'_>,
) -> Option<String> {
    match conversion {
        'd' | 'i' | 'u' => Some(value.to_integer()?.to_string()),
        'o' => Some(format!("{:o}", value.to_integer()?)),
        'x' => Some(format!("{:x}", value.to_integer()?)),
        'X' => Some(format!("{:X}", value.to_integer()?)),
        'f' | 'F' => {
            let number = numeric(value)?;
            let precision = precision.unwrap_or(6);
            Some(format!("{number:.precision$}"))
        }
        'e' => Some(format!("{:e}", numeric(value)?)),
        'E' => Some(format!("{:E}", numeric(value)?)),
        'g' | 'G' => Some(format!("{}", numeric(value)?)),
        'c' => {
            let byte = value.to_integer()?;
            if !(0..=255).contains(&byte) {
                return None;
            }
            Some((byte as u8 as char).to_string())
        }
        's' => {
            let text = match value {
                Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                Value::Integer(i) => i.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Boolean(b) => b.to_string(),
                Value::Nil => "nil".to_string(),
                _ => return None,
            };
            let text = match precision {
                Some(limit) => text.chars().take(limit).collect(),
                None => text,
            };
            Some(text)
        }
        _ => None,
    }
}

fn numeric(value: Value<'_>) -> Option<f64> {
    match value {
        Value::Integer(i) => Some(i as f64),
        Value::Number(n) => Some(n),
        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).parse::<f64>().ok(),
        _ => None,
    }
}

fn pad(rendered: &str, width: usize, left_align: bool) -> String {
    let length = rendered.chars().count();
    if length >= width {
        return rendered.to_string();
    }
    let padding: String = std::iter::repeat_n(' ', width - length).collect();
    if left_align {
        format!("{rendered}{padding}")
    } else {
        format!("{padding}{rendered}")
    }
}

#[cfg(test)]
mod tests {
    use crate::{ExecuteOutcome, LuaVm};

    #[test]
    fn utf8_len_counts_codepoints() {
        let mut vm = LuaVm::new("utf8");
        let outcome = vm
            .execute(
                r#"
                ascii = utf8.len("abc")
                multibyte = utf8.len("aé€😀")
            "#,
            )
            .expect("execute");
        assert!(
            matches!(outcome, ExecuteOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(vm.test_global("ascii"), Some(3.0));
        assert_eq!(vm.test_global("multibyte"), Some(4.0));
    }

    #[test]
    fn utf8_char_and_codepoint_round_trip() {
        let mut vm = LuaVm::new("utf8");
        let outcome = vm
            .execute(
                r#"
                local text = utf8.char(0x61, 0x1F600)
                local values = { utf8.codepoint(text) }
                round_trip = (#text == 5) and (values[2] == 0x1F600)
            "#,
            )
            .expect("execute");
        assert!(
            matches!(outcome, ExecuteOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(vm.test_global("round_trip"), Some(1.0));
    }

    #[test]
    fn utf8_len_rejects_invalid_bytes() {
        let mut vm = LuaVm::new("utf8");
        let outcome = vm
            .execute(
                r#"
                local ok, position = utf8.len(string.char(0xFF))
                invalid_flagged = (not ok) and position >= 1
            "#,
            )
            .expect("execute");
        assert!(
            matches!(outcome, ExecuteOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(vm.test_global("invalid_flagged"), Some(1.0));
    }

    #[test]
    fn string_byte_char_and_format() {
        let mut vm = LuaVm::new("string");
        let outcome = vm
            .execute(
                r#"
                local a, b = string.byte("AB", 1, 2)
                local text = string.char(72, 105)
                formatted = string.format("%s (%d)", text, a + b)
            "#,
            )
            .expect("execute");
        assert!(
            matches!(outcome, ExecuteOutcome::Completed { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn table_concat_and_sort_with_comparator() {
        let mut vm = LuaVm::new("table");
        let outcome = vm
            .execute(
                r#"
                local ranked = {
                  { label = "a", count = 2 },
                  { label = "b", count = 5 },
                  { label = "c", count = 1 },
                }
                table.sort(ranked, function(left, right) return left.count > right.count end)
                ordered = table.concat({ ranked[1].label, ranked[2].label, ranked[3].label }, "")
            "#,
            )
            .expect("execute");
        assert!(
            matches!(outcome, ExecuteOutcome::Completed { .. }),
            "{outcome:?}"
        );
    }
}
