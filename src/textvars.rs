//! `{变量}` 的展开：OSD 自定义文字和托盘提示共用同一套写法。
//!
//! 语法（和托盘提示保持一致，用户在两边看到的是同一套东西）：
//! - `{名字}` 换成实时值，名字**大小写不敏感**（`{HH}` 和 `{hh}` 一样）；
//! - `{{` / `}}` 是字面的一个大括号；
//! - 认不出来的 `{xx}` 原样留着 —— 拼错了能一眼看出来，而不是悄悄消失。
//!
//! 时间变量一次展开只取一份时间（[`Clock`]）：同一张卡片上的
//! `{hh}:{mm}:{ss}` 永远是同一秒，不会出现「前半秒旧、后半秒新」的跳字。

use crate::i18n::Lang;

/// 把 `{变量}` 逐段换掉；`lookup` 返回 `None` 的名字原样保留。
pub fn expand(raw: &str, mut lookup: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                // `{{` 是字面大括号。
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                    continue;
                }
                let mut name = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    name.push(next);
                }
                if !closed {
                    // 没闭合：保留原样 —— 用户可能正打到一半。
                    out.push('{');
                    out.push_str(&name);
                    continue;
                }
                match lookup(&name) {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push('{');
                        out.push_str(&name);
                        out.push('}');
                    }
                }
            }
            '}' => {
                // `}}` 同理。
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                out.push('}');
            }
            _ => out.push(ch),
        }
    }
    out
}

/// 某一刻的本地时间。
///
/// 只存展开需要的字段，不引日期库：系统已经给了本地时间，自己算时区只会算错。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    /// 0 = 周日 … 6 = 周六。
    pub weekday: u8,
    /// 24 小时制（0..23）。
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Clock {
    /// 现在（本地时区，直接问系统）。
    pub fn local() -> Self {
        use windows::Win32::System::SystemInformation::GetLocalTime;
        let time = unsafe { GetLocalTime() };
        Self {
            year: time.wYear,
            month: time.wMonth as u8,
            day: time.wDay as u8,
            weekday: (time.wDayOfWeek % 7) as u8,
            hour: time.wHour as u8,
            minute: time.wMinute as u8,
            second: time.wSecond as u8,
        }
    }
}

/// OSD 自定义文字默认摆出来的那几个变量（编辑器里的按钮按这个列）。
///
/// 只列最常用的；全表见 [`time_value`]，其余变量照样能手打。
pub const TOKENS: &[&str] = &[
    "{hh}", "{mm}", "{ss}", "{ampm}", "{date}", "{week}", "{time}", "{time12}", "{yyyy}", "{mo}",
    "{dd}",
];

/// 这条文字里有没有认得出来的时间变量（`{{` 不算）。
pub fn has_variable(raw: &str) -> bool {
    if !raw.contains('{') {
        return false;
    }
    let mut found = false;
    // 借同一个解析器走一遍：返回 `None` 就不改动原文，只看认出了什么。
    let _ = expand(raw, |name| {
        if time_value(name, &Clock::default(), Lang::Zh).is_some() {
            found = true;
        }
        None
    });
    found
}

/// 把时间变量换成当前值，其余原样保留。
pub fn resolve(raw: &str, clock: &Clock, lang: Lang) -> String {
    expand(raw, |name| time_value(name, clock, lang))
}

/// 12 小时制的小时（0 点显示成 12）。
fn hour12(hour: u8) -> u8 {
    let hour = hour % 12;
    if hour == 0 { 12 } else { hour }
}

fn weekday_name(weekday: u8, lang: Lang) -> &'static str {
    const ZH: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
    const EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let table = if lang.is_en() { EN } else { ZH };
    table[(weekday as usize) % 7]
}

/// 变量表。大小写不敏感：`{MM}` 和 `{mm}` 都是**分钟**，月份写 `{mo}` / `{month}`。
fn time_value(name: &str, clock: &Clock, lang: Lang) -> Option<String> {
    let key = name.trim().to_ascii_lowercase();
    Some(match key.as_str() {
        "h" => clock.hour.to_string(),
        "hh" => format!("{:02}", clock.hour),
        "h12" => hour12(clock.hour).to_string(),
        "hh12" => format!("{:02}", hour12(clock.hour)),
        "m" => clock.minute.to_string(),
        "mm" => format!("{:02}", clock.minute),
        "s" => clock.second.to_string(),
        "ss" => format!("{:02}", clock.second),
        // 中文给「上午 / 下午」，英文给 AM / PM。
        "ampm" => match (lang.is_en(), clock.hour < 12) {
            (true, true) => "AM".to_string(),
            (true, false) => "PM".to_string(),
            (false, true) => "上午".to_string(),
            (false, false) => "下午".to_string(),
        },
        "date" => format!("{:04}-{:02}-{:02}", clock.year, clock.month, clock.day),
        "yy" => format!("{:02}", clock.year % 100),
        "yyyy" => format!("{:04}", clock.year),
        "mo" | "month" => format!("{:02}", clock.month),
        "d" => clock.day.to_string(),
        "dd" | "day" => format!("{:02}", clock.day),
        "week" | "weekday" => weekday_name(clock.weekday, lang).to_string(),
        "time" => format!("{:02}:{:02}", clock.hour, clock.minute),
        // 12 小时制那一档固定用 AM / PM：数码钟的样子，跟界面语言无关。
        "time12" => format!(
            "{}:{:02} {}",
            hour12(clock.hour),
            clock.minute,
            if clock.hour < 12 { "AM" } else { "PM" }
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> Clock {
        Clock {
            year: 2026,
            month: 3,
            day: 7,
            weekday: 6,
            hour: 9,
            minute: 5,
            second: 3,
        }
    }

    #[test]
    fn time_variables_are_padded() {
        let out = resolve("{hh}:{mm}:{ss}", &clock(), Lang::Zh);
        assert_eq!(out, "09:05:03");
        assert_eq!(resolve("{h}:{m}:{s}", &clock(), Lang::Zh), "9:5:3");
        assert_eq!(resolve("{time}", &clock(), Lang::Zh), "09:05");
        assert_eq!(resolve("{time12}", &clock(), Lang::Zh), "9:05 AM");
    }

    #[test]
    fn date_and_week_follow_the_language() {
        assert_eq!(resolve("{date}", &clock(), Lang::Zh), "2026-03-07");
        assert_eq!(
            resolve("{yyyy}/{mo}/{dd}", &clock(), Lang::Zh),
            "2026/03/07"
        );
        assert_eq!(resolve("{yy}", &clock(), Lang::Zh), "26");
        assert_eq!(resolve("{week}", &clock(), Lang::Zh), "周六");
        assert_eq!(resolve("{week}", &clock(), Lang::En), "Sat");
    }

    #[test]
    fn names_are_case_insensitive_and_mm_is_minutes() {
        // 用户常把「分钟」写成 {MM}，这里按分钟算；月份得写 {mo}。
        assert_eq!(resolve("{MM}", &clock(), Lang::Zh), "05");
        assert_eq!(resolve("{HH}-{SS}", &clock(), Lang::Zh), "09-03");
        assert_eq!(resolve("{mon}", &clock(), Lang::Zh), "{mon}");
        assert_eq!(resolve("{month}", &clock(), Lang::Zh), "03");
    }

    #[test]
    fn ampm_and_midnight() {
        let mut night = clock();
        night.hour = 0;
        assert_eq!(resolve("{ampm}", &night, Lang::Zh), "上午");
        assert_eq!(resolve("{ampm}", &night, Lang::En), "AM");
        assert_eq!(resolve("{time12}", &night, Lang::Zh), "12:05 AM");
        night.hour = 13;
        assert_eq!(resolve("{ampm}", &night, Lang::Zh), "下午");
        assert_eq!(resolve("{time12}", &night, Lang::Zh), "1:05 PM");
    }

    #[test]
    fn unknown_variables_survive() {
        assert_eq!(resolve("{hh} {nope}", &clock(), Lang::Zh), "09 {nope}");
        // 没闭合的（还在打字）一个字都不吞。
        assert_eq!(resolve("{hh", &clock(), Lang::Zh), "{hh");
    }

    #[test]
    fn braces_escape() {
        assert_eq!(resolve("{{hh}}", &clock(), Lang::Zh), "{hh}");
        assert_eq!(resolve("a }} b", &clock(), Lang::Zh), "a } b");
    }

    #[test]
    fn has_variable_only_sees_known_names() {
        assert!(!has_variable("音量"));
        assert!(!has_variable("{nope}"));
        assert!(!has_variable("{{hh}}"));
        assert!(has_variable("{hh}"));
        assert!(has_variable("现在是 {time}"));
    }
}
