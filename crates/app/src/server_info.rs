//! The server info dialog's content: what a Rex server says about itself.

use serde_json::Value;

use crate::i18n::t;

/// "2026-11-23T01:07:30.9Z" → "2026-11-23 01:07 UTC"; anything else as given.
fn short_time(s: &str) -> String {
    match (s.get(0..10), s.get(11..16)) {
        (Some(d), Some(hm)) if s.ends_with('Z') => format!("{d} {hm} UTC"),
        (Some(d), Some(hm)) => format!("{d} {hm}"),
        _ => s.to_string(),
    }
}

/// (label, value) rows: version, process, sessions, config, Tailscale, clients.
pub fn info_rows(version: &Value, status: &Value, clients: &Value) -> Vec<(String, String)> {
    let s = |v: &Value| v.as_str().unwrap_or("?").to_string();
    let version_line = format!(
        "{} (api {}, {})",
        s(&version["rex_version"]),
        version["api_revision"].as_u64().unwrap_or(0),
        s(&version["platform"])
    );
    let process = format!(
        "pid {}, {} {}",
        status["pid"].as_u64().unwrap_or(0),
        t("started", "启动于"),
        short_time(status["started_at"].as_str().unwrap_or("?"))
    );
    let mut sessions = status["session_count"].as_u64().unwrap_or(0).to_string();
    if status["exit_empty"] == true {
        sessions.push_str(t(" (stops when empty)", "(没有 session 时自动停止)"));
    }
    let config = if status["config"]["hash"].as_str().unwrap_or("").is_empty() {
        t("no config file", "没有配置文件").to_string()
    } else {
        format!("{} {} {}", t("loaded", "已加载"), status["config"]["generation"].as_u64().unwrap_or(0), t("times", "次"))
    };
    let tailscale = status["tailscale"]["state"].as_str().map_or_else(|| t("not available", "不可用").to_string(), str::to_string);
    // Connected clients by name and platform, in first-seen order, with a count.
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut lost = 0;
    for c in clients["clients"].as_array().into_iter().flatten() {
        if c["connection_state"].as_str() != Some("connected") {
            lost += 1;
            continue;
        }
        let name = format!("{} ({})", s(&c["info"]["name"]), s(&c["info"]["platform"]));
        match seen.iter_mut().find(|(n, _)| *n == name) {
            Some((_, n)) => *n += 1,
            None => seen.push((name, 1)),
        }
    }
    let mut clients_line = seen.iter().map(|(n, k)| if *k > 1 { format!("{n} ×{k}") } else { n.clone() }).collect::<Vec<_>>().join(", ");
    if clients_line.is_empty() {
        clients_line = t("none", "无").to_string();
    }
    if lost > 0 {
        clients_line.push_str(&format!("; {lost} {}", t("disconnected", "已断开")));
    }
    vec![
        (t("Version", "版本").into(), version_line),
        (t("Process", "进程").into(), process),
        (t("Sessions", "Session").into(), sessions),
        (t("Config", "配置").into(), config),
        ("Tailscale".into(), tailscale),
        (t("Clients", "客户端").into(), clients_line),
    ]
}

/// What `server.config_reload` answered, in one line.
pub fn reload_text(result: &Value) -> String {
    let n = result["generation"].as_u64().unwrap_or(0);
    let load = if crate::i18n::zh() { format!("第 {n} 次") } else { format!("load {n}") };
    let problems: Vec<String> = result["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            let sev = d["severity"].as_str().unwrap_or("warning");
            match d["source"].as_str() {
                Some(src) => format!("{sev} {src}: {}", d["message"].as_str().unwrap_or("")),
                None => format!("{sev}: {}", d["message"].as_str().unwrap_or("")),
            }
        })
        .collect();
    if problems.is_empty() {
        format!("{} ({load}).", t("Config reloaded", "配置已重新加载"))
    } else {
        format!(
            "{} ({load}) {} {} {}: {}",
            t("Config reloaded", "配置已重新加载"),
            t("with", "有"),
            problems.len(),
            t("problems", "个问题"),
            problems.join(" · ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn version() -> Value {
        json!({"rex_version":"0.1.0-snapshot.2506","platform":"linux_amd64","commit":"7635007b","built_at":"2026-11-21T15:05:34Z","dirty":false,"api_revision":19})
    }

    fn status() -> Value {
        json!({"pid":486249,"instance_id":"x","host_id":"h","started_at":"2026-11-23T01:07:30.946179584Z","exit_empty":true,"session_count":3,
               "tailscale":{"state":"disabled","allow_tailnet_members":false},"api_revision":19,"config":{"generation":2,"hash":"abc"}})
    }

    fn clients() -> Value {
        json!({"clients":[
            {"client_id":"c1","info":{"kind":"app","platform":"linux","name":"rexa-desktop","version":"1.0.0-dev"},"connection_state":"lost"},
            {"client_id":"c2","info":{"kind":"app","platform":"macos","name":"Rex","version":"1.2"},"connection_state":"connected"},
            {"client_id":"c3","info":{"kind":"cli","platform":"linux","name":"rex","version":"0.1"},"connection_state":"connected"},
            {"client_id":"c4","info":{"kind":"cli","platform":"linux","name":"rex","version":"0.1"},"connection_state":"connected"}
        ]})
    }

    fn row<'a>(rows: &'a [(String, String)], i: usize) -> &'a str {
        &rows[i].1
    }

    #[test]
    fn info_rows_from_real_shapes() {
        let _lang = crate::i18n::TEST_LANG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::i18n::set_language(crate::config::Language::En);
        let rows = info_rows(&version(), &status(), &clients());
        assert_eq!(rows.len(), 6);
        assert_eq!(row(&rows, 0), "0.1.0-snapshot.2506 (api 19, linux_amd64)");
        assert_eq!(row(&rows, 1), "pid 486249, started 2026-11-23 01:07 UTC");
        assert_eq!(row(&rows, 2), "3 (stops when empty)");
        assert_eq!(row(&rows, 3), "loaded 2 times");
        assert_eq!(row(&rows, 4), "disabled");
        assert_eq!(row(&rows, 5), "Rex (macos), rex (linux) ×2; 1 disconnected");
        // Missing pieces don't break it.
        let empty = info_rows(&json!({}), &json!({"config":{"generation":1,"hash":""}}), &json!({}));
        assert_eq!(empty[3].1, "no config file");
        assert_eq!(empty[5].1, "none");
    }

    #[test]
    fn reload_reports_diagnostics() {
        let _lang = crate::i18n::TEST_LANG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::i18n::set_language(crate::config::Language::En);
        assert_eq!(reload_text(&json!({"generation": 3, "diagnostics": []})), "Config reloaded (load 3).");
        assert_eq!(
            reload_text(&json!({"generation": 4, "diagnostics": [{"severity":"error","source":"init.lua:22","message":"Unknown action."},{"severity":"warning","message":"Odd."}]})),
            "Config reloaded (load 4) with 2 problems: error init.lua:22: Unknown action. · warning: Odd."
        );
    }
}
