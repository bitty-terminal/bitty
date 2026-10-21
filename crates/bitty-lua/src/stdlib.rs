//! Retained restricted-stdlib additions the in-core baseline does not ship.
//!
//! The accepted [Lua Runtime RFC] baseline keeps a pure-computation base
//! (`string`, `table`, `math`, `utf8`, basic functions) for every VM.
//! Phodopus's core ships `utf8` and `string.format` in-core with proportional
//! fuel charging, so the former bitty `utf8` and `string.format` duplicates
//! are retired; the old boundary tests now exercise the in-core
//! implementations as parity. This module keeps the bounded bitty variants it
//! still owns (`string.byte/char`, `table.concat`, `table.sort`) plus the
//! restricted `os.time/clock/date` core does not ship. The string/table
//! functions only inspect or build bounded in-VM values. The single host-state
//! read is `os.date`'s default path: it resolves the process timezone
//! environment (`TZ`) and the host timezone database through `chrono::Local`
//! to local calendar fields, which intentionally exposes the host timezone to
//! a plugin with no grant. `os.date("!...")` is deterministic UTC. Nothing
//! here grants process, filesystem, or network authority, and `os.getenv` /
//! `os.execute` stay absent.
//!
//! [Lua Runtime RFC]: https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/specifications/lua-runtime-rfc.md

use phodopus::{
    Callback, CallbackReturn, Closure, Context, Executor, IntoValue, Lua, Table, Value, Variadic,
};

/// Maximum bytes produced by one `table.concat` call.
const TABLE_CONCAT_MAX_BYTES: usize = 64 * 1024;

/// Install the retained stdlib functions into `lua`.
pub(crate) fn install_retained_stdlib(lua: &mut Lua) {
    lua.enter(|ctx| {
        install_string(ctx);
        install_table(ctx);
        install_os(ctx);
    });
    install_table_sort(lua);
}

/// Install only pure computation libraries for the dedicated configuration VM.
/// The base loader itself includes dynamic loading and debug helpers, so the
/// configuration surface is scrubbed after loading and before untrusted code
/// runs. No retained `os` helpers or host callbacks are installed here.
pub(crate) fn install_config_stdlib(lua: &mut Lua) {
    lua.enter(|ctx| {
        phodopus::stdlib::load_base(ctx);
        phodopus::stdlib::load_string(ctx);
        phodopus::stdlib::load_table(ctx);
        phodopus::stdlib::load_math(ctx);
        phodopus::stdlib::load_utf8(ctx);
        if let Ok(math) = ctx.get_global::<Table>("math") {
            math.set_field(ctx, "random", Value::Nil);
            math.set_field(ctx, "randomseed", Value::Nil);
        }
        for name in [
            "_G",
            "debug",
            "package",
            "require",
            "load",
            "loadfile",
            "dofile",
            "io",
            "os",
            "coroutine",
            "tonumber",
        ] {
            ctx.set_global(name, phodopus::Value::Nil);
        }
    });
}

/// Restricted `os` retained by the accepted baseline: `time`/`clock`/`date`
/// only. `os.execute`, `os.getenv`, `os.remove`, `os.tmpname`, and the rest are
/// deliberately absent (no process, filesystem, or network authority).
///
/// `os.date` without a leading `!` is the one host-state read in this module:
/// it resolves the process timezone environment (`TZ`) and the host timezone
/// database through `chrono::Local` to local calendar fields, so a plugin gains
/// the host timezone with no grant. `os.date("!...")` is deterministic UTC over
/// the same fields. The read returns calendar values only — it is not an
/// environment lookup, and no environment name/value is observable.
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
            let (utc, spec) = match format.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, format.as_str()),
            };
            let Some(fields) = date_fields_at(seconds, utc) else {
                // Reference Lua returns nil when the timestamp cannot be
                // represented by the host's calendar conversion.
                stack.replace(ctx, Value::Nil);
                return Ok(CallbackReturn::Return);
            };
            if spec == "*t" {
                let table = Table::new(&ctx);
                let _ = table.set(ctx, "year", fields.year);
                let _ = table.set(ctx, "month", fields.month);
                let _ = table.set(ctx, "day", fields.day);
                let _ = table.set(ctx, "hour", fields.hour);
                let _ = table.set(ctx, "min", fields.minute);
                let _ = table.set(ctx, "sec", fields.second);
                let _ = table.set(ctx, "wday", fields.weekday);
                let _ = table.set(ctx, "yday", fields.year_day);
                // Known deviation: reference Lua reports `isdst = true` while
                // daylight saving time is in effect. chrono's `Local` exposes
                // the UTC offset but not the DST flag, so this stays constant
                // `false` until a tz-aware source lands; tracked as a
                // follow-up (the bar clock consumes `%H:%M` only).
                let _ = table.set(ctx, "isdst", false);
                stack.replace(ctx, Value::Table(table));
            } else {
                stack.replace(ctx, ctx.intern(format_date(spec, &fields).as_bytes()));
            }
            Ok(CallbackReturn::Return)
        }),
    )
    .expect("os accepts 'date'");
    ctx.set_global("os", os);
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

/// Calendar fields for a Unix timestamp in UTC (`utc = true`) or the host's
/// local timezone (Lua `os.date` default). The local path reads the process
/// `TZ` environment and the host timezone database; `utc = true` is
/// deterministic UTC. `chrono` resolves and caches the zone once per process,
/// so a later `TZ` change is ignored (plugins cannot set environment variables
/// anyway).
///
/// Returns `None` when the timestamp lies outside the representable calendar
/// range, matching reference Lua's `nil` result.
fn date_fields_at(seconds: i64, utc: bool) -> Option<DateFields> {
    use chrono::{DateTime, Local, Utc};
    let timestamp = DateTime::<Utc>::from_timestamp(seconds, 0)?;
    Some(if utc {
        fields_from_datetime(&timestamp)
    } else {
        fields_from_datetime(&timestamp.with_timezone(&Local))
    })
}

fn fields_from_datetime<Tz: chrono::TimeZone>(dt: &chrono::DateTime<Tz>) -> DateFields {
    use chrono::{Datelike, Timelike};
    DateFields {
        year: i64::from(dt.year()),
        month: i64::from(dt.month()),
        day: i64::from(dt.day()),
        hour: i64::from(dt.hour()),
        minute: i64::from(dt.minute()),
        second: i64::from(dt.second()),
        weekday: i64::from(dt.weekday().num_days_from_sunday()) + 1,
        year_day: i64::from(dt.ordinal()),
    }
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

fn install_string<'gc>(ctx: Context<'gc>) {
    let string = match ctx.get_global("string") {
        Ok(Value::Table(table)) => table,
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

    ctx.set_global("string", string);
}

fn install_table<'gc>(ctx: Context<'gc>) {
    let table = match ctx.get_global("table") {
        Ok(Value::Table(table)) => table,
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
                    match list.get_value(ctx, index) {
                        Value::String(s) => out.push_str(&String::from_utf8_lossy(s.as_bytes())),
                        Value::Integer(i) => out.push_str(&i.to_string()),
                        Value::Number(n) => out.push_str(&n.to_string()),
                        _ => return Err("invalid value in 'concat' list".into_value(ctx).into()),
                    }
                    if out.len() > TABLE_CONCAT_MAX_BYTES {
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
    ctx.set_global("table", table);
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
    // The sort chunk runs once on a fresh single-owner executor, so a step
    // refusal is unreachable; a trusted-chunk failure panics like the compile
    // immediately above rather than shipping a half-installed stdlib.
    lua.finish(&stashed)
        .expect("trusted stdlib sort chunk finishes");
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]
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
                local values = { utf8.codepoint(text, 1, #text) }
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
    fn os_date_defaults_to_local_time_and_bang_forces_utc() {
        let local = chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
            .expect("epoch")
            .with_timezone(&chrono::Local);
        let local_text = local.format("%Y-%m-%d %H:%M").to_string();
        let local_hour: i64 = local.format("%H").to_string().parse().expect("hour");
        let mut vm = LuaVm::new("osdate");
        let source = format!(
            r#"
            utc_ok = os.date("!%Y-%m-%d %H:%M", 0) == "1970-01-01 00:00"
            local_ok = os.date("%Y-%m-%d %H:%M", 0) == "{local_text}"
            utc_table = os.date("!*t", 0).hour == 0
            local_table = os.date("*t", 0).hour == {local_hour}
            "#
        );
        let outcome = vm.execute(&source).expect("execute");
        assert!(
            matches!(outcome, ExecuteOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(vm.test_global("utc_ok"), Some(1.0));
        assert_eq!(vm.test_global("local_ok"), Some(1.0));
        assert_eq!(vm.test_global("utc_table"), Some(1.0));
        assert_eq!(vm.test_global("local_table"), Some(1.0));
    }

    /// Child-process witness for `os.date`'s local/UTC split. The parent
    /// re-executes this test binary with `TZ` pinned so the assertion does not
    /// depend on the developer's or CI runner's timezone (GitHub runners run
    /// UTC, which would make an in-process test pass for a UTC-only `os.date`).
    /// Unix only: `TZ` is not an override chrono honors on Windows.
    #[cfg(unix)]
    const WITNESS_TZ_ENV: &str = "BITTY_LUA_OS_DATE_WITNESS_TZ";

    #[test]
    #[cfg(unix)]
    fn os_date_local_matches_tz_in_child_process() {
        for tz in ["Asia/Shanghai", "UTC", "America/New_York"] {
            let output =
                std::process::Command::new(std::env::current_exe().expect("test binary path"))
                    .args([
                        "--exact",
                        "stdlib::tests::os_date_timezone_witness_child",
                        "--ignored",
                        "--test-threads=1",
                    ])
                    .env("TZ", tz)
                    .env(WITNESS_TZ_ENV, tz)
                    .output()
                    .expect("spawn witness");
            assert!(
                output.status.success(),
                "TZ={tz} witness failed\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    #[ignore = "spawned by os_date_local_matches_tz_in_child_process with TZ pinned"]
    #[cfg(unix)]
    fn os_date_timezone_witness_child() {
        let tz = std::env::var(WITNESS_TZ_ENV).expect("witness TZ env");
        // 2026-07-01T12:00:00Z (northern-hemisphere DST), 2026-01-15T12:00:00Z
        // (standard time), and the New York spring-forward second pair
        // 2026-03-08T06:59:59Z / 07:00:00Z. Every expectation below was
        // checked against `date(1)` for the same instant.
        let summer = 1_782_907_200;
        let winter = 1_768_478_400;
        let before_spring_forward = 1_772_953_199;
        let after_spring_forward = 1_772_953_200;
        let (summer_local, winter_local, epoch_local, before_dst, after_dst) = match tz.as_str() {
            "Asia/Shanghai" => (
                "2026-07-01 20:00",
                "2026-01-15 20:00",
                "1970-01-01 08:00",
                "2026-03-08 14:59",
                "2026-03-08 15:00",
            ),
            "UTC" => (
                "2026-07-01 12:00",
                "2026-01-15 12:00",
                "1970-01-01 00:00",
                "2026-03-08 06:59",
                "2026-03-08 07:00",
            ),
            "America/New_York" => (
                "2026-07-01 08:00",
                "2026-01-15 07:00",
                "1969-12-31 19:00",
                "2026-03-08 01:59",
                "2026-03-08 03:00",
            ),
            other => panic!("unexpected witness TZ {other}"),
        };
        let mut vm = LuaVm::new("osdate-witness");
        let source = format!(
            r#"
            summer = os.date("%Y-%m-%d %H:%M", {summer}) == "{summer_local}"
            winter = os.date("%Y-%m-%d %H:%M", {winter}) == "{winter_local}"
            epoch = os.date("%Y-%m-%d %H:%M", 0) == "{epoch_local}"
            before_dst = os.date("%Y-%m-%d %H:%M", {before_spring_forward}) == "{before_dst}"
            after_dst = os.date("%Y-%m-%d %H:%M", {after_spring_forward}) == "{after_dst}"
            utc_epoch = os.date("!%Y-%m-%d %H:%M", 0) == "1970-01-01 00:00"

            -- Full table field set for the local path (2026-07-01 is yday 182,
            -- wday 4 = Wednesday with 1 = Sunday in every witness zone).
            local summer_fields = os.date("*t", {summer})
            summer_year = summer_fields.year == 2026
            summer_month = summer_fields.month == 7
            summer_day = summer_fields.day == 1
            summer_min = summer_fields.min == 0
            summer_sec = summer_fields.sec == 0
            summer_wday = summer_fields.wday == 4
            summer_yday = summer_fields.yday == 182
            local before_fields = os.date("*t", {before_spring_forward})
            before_min = before_fields.min == 59
            before_sec = before_fields.sec == 59
            before_wday = before_fields.wday == 1
            before_yday = before_fields.yday == 67

            -- UTC tables keep the same field set on the UTC side.
            local utc_fields = os.date("!*t", {summer})
            utc_hour = utc_fields.hour == 12
            utc_min = utc_fields.min == 0
            utc_sec = utc_fields.sec == 0
            utc_wday = utc_fields.wday == 4
            utc_yday = utc_fields.yday == 182
            overflow_is_nil = os.date("%Y", 9223372036854775807) == nil
            "#
        );
        vm.execute(&source).expect("execute");

        for name in [
            "summer",
            "winter",
            "epoch",
            "before_dst",
            "after_dst",
            "utc_epoch",
            "summer_year",
            "summer_month",
            "summer_day",
            "summer_min",
            "summer_sec",
            "summer_wday",
            "summer_yday",
            "before_min",
            "before_sec",
            "before_wday",
            "before_yday",
            "utc_hour",
            "utc_min",
            "utc_sec",
            "utc_wday",
            "utc_yday",
            "overflow_is_nil",
        ] {
            assert_eq!(vm.test_global(name), Some(1.0), "TZ={tz}: {name} failed");
        }
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
