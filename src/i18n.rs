use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[repr(u8)]
pub enum Language {
    #[serde(rename = "zh-CN")]
    SimplifiedChinese = 1,
    #[serde(rename = "en")]
    English = 2,
    #[default]
    #[serde(rename = "system", other)]
    System = 0,
}

impl Language {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => tr("跟随系统"),
            Self::SimplifiedChinese => "简体中文",
            Self::English => "English",
        }
    }
}

static LANGUAGE: AtomicU8 = AtomicU8::new(Language::System as u8);

pub fn language() -> Language {
    match LANGUAGE.load(Ordering::Relaxed) {
        1 => Language::SimplifiedChinese,
        2 => Language::English,
        _ => Language::System,
    }
}

pub fn set_language(language: Language) {
    LANGUAGE.store(language as u8, Ordering::Relaxed);
}

pub fn english() -> bool {
    match language() {
        Language::SimplifiedChinese => false,
        Language::English => true,
        Language::System => system_english(),
    }
}

fn system_english() -> bool {
    static ENGLISH: OnceLock<bool> = OnceLock::new();
    *ENGLISH.get_or_init(|| {
        #[cfg(windows)]
        {
            // Use the Windows display language, not the date/number formatting locale.
            let language = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
            language & 0x03ff != 0x04
        }
        #[cfg(not(windows))]
        {
            let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
                .into_iter()
                .filter_map(|key| std::env::var(key).ok())
                .find(|value| !value.is_empty())
                .unwrap_or_default();
            !locale.to_ascii_lowercase().starts_with("zh")
        }
    })
}

pub fn tr(value: &str) -> &str {
    if !english() {
        return value;
    }
    static ENGLISH: OnceLock<HashMap<String, String>> = OnceLock::new();
    ENGLISH
        .get_or_init(|| {
            serde_json::from_str(include_str!("../assets/locales/en.json"))
                .expect("Invalid bundled English translations")
        })
        .get(value)
        .map_or(value, String::as_str)
}

pub fn count(template: &str, count: usize) -> String {
    tr(template).replace("{count}", &count.to_string())
}

// Runtime errors can carry a path or a third-party error. Translate our message,
// keeping the diagnostic payload and all project names/paths untouched.
pub fn message(value: &str) -> String {
    let translated = tr(value);
    if translated != value || !english() {
        return translated.to_owned();
    }
    for template in [
        "本地服务未能就绪，请查看 {value}",
        "无法复制 {value}",
        "启动进程已退出（{value}）",
        "WebView2 截图进程已退出（{value}）",
    ] {
        let (prefix, suffix) = template.split_once("{value}").unwrap();
        if let Some(detail) = value
            .strip_prefix(prefix)
            .and_then(|v| v.strip_suffix(suffix))
        {
            return tr(template).replace("{value}", detail);
        }
    }
    if let Some((manager, rest)) = value.split_once(" 安装失败（退出码 ") {
        if let Some(code) = rest.strip_suffix("），请查看日志后重试。") {
            return format!(
                "{manager} installation failed (exit code {code}). Check the logs and try again."
            );
        }
    }
    for separator in ["：", ": "] {
        if let Some((prefix, detail)) = value.split_once(separator) {
            let translated = tr(prefix);
            let translated_detail = message(detail);
            if translated != prefix || translated_detail != detail {
                return format!("{translated}: {translated_detail}");
            }
        }
    }
    value.to_owned()
}
