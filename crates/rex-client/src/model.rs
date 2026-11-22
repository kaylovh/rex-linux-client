//! What a client knows about a server's sessions: the list, the current session's view (kept
//! at its newest revision) and what programs report about themselves (OSC 7501 status).

use std::collections::HashMap;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub label: String,
}

/// What a program reports it is doing (OSC 7501).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Nothing,
    /// The current session's view is out of date: call session.view for it.
    RefetchView(String),
    SessionsChanged,
    StatusChanged,
}

#[derive(Default)]
pub struct SessionModel {
    pub sessions: Vec<SessionInfo>,
    pub current: Option<String>,
    pub view: Option<Value>,
    /// The highest view revision announced for the current session.
    announced: u64,
    /// block id -> (session id, record id -> status)
    status: HashMap<String, (String, HashMap<String, Status>)>,
}

impl Status {
    /// Unknown values are Idle, as the API asks.
    pub fn parse(s: &str) -> Status {
        match s {
            "working" => Status::Working,
            "blocked" => Status::Blocked,
            "done" => Status::Done,
            "error" => Status::Error,
            _ => Status::Idle,
        }
    }

    /// How much it needs the user: waiting for them first, then failures.
    fn rank(self) -> u8 {
        match self {
            Status::Idle => 0,
            Status::Done => 1,
            Status::Working => 2,
            Status::Error => 3,
            Status::Blocked => 4,
        }
    }

    fn most_pressing(items: impl Iterator<Item = Status>) -> Status {
        items.max_by_key(|s| s.rank()).unwrap_or_default()
    }
}

fn revision(v: &Value) -> Option<u64> {
    v["revision"].as_u64()
}

impl SessionModel {
    pub fn apply_event(&mut self, ev: &Value) -> Effect {
        let p = &ev["payload"];
        match ev["type"].as_str().unwrap_or("") {
            "session_created" => {
                let s = if p["session"].is_object() { &p["session"] } else { p };
                let Some(id) = s["session_id"].as_str() else { return Effect::Nothing };
                let label = s["label"].as_str().unwrap_or("").to_string();
                match self.sessions.iter_mut().find(|x| x.id == id) {
                    Some(x) => x.label = label,
                    None => self.sessions.push(SessionInfo { id: id.to_string(), label }),
                }
                Effect::SessionsChanged
            }
            "session_destroyed" => {
                let Some(id) = p["session_id"].as_str().or(ev["session_id"].as_str()) else { return Effect::Nothing };
                self.sessions.retain(|x| x.id != id);
                self.status.retain(|_, (sid, _)| sid != id);
                Effect::SessionsChanged
            }
            "session_label_changed" => {
                let (Some(id), Some(label)) = (ev["session_id"].as_str(), p["label"].as_str()) else { return Effect::Nothing };
                if let Some(x) = self.sessions.iter_mut().find(|x| x.id == id) {
                    x.label = label.to_string();
                }
                Effect::SessionsChanged
            }
            "session_view_changed" => {
                let Some(sid) = ev["session_id"].as_str() else { return Effect::Nothing };
                if self.current.as_deref() != Some(sid) {
                    return Effect::Nothing;
                }
                let have = self.view.as_ref().and_then(revision);
                if let Some(r) = revision(p) {
                    self.announced = self.announced.max(r);
                }
                match (revision(p), have) {
                    (Some(new), Some(old)) if new <= old => Effect::Nothing,
                    _ => Effect::RefetchView(sid.to_string()),
                }
            }
            "block_closed" => {
                if let Some(bid) = p["block_id"].as_str() {
                    self.status.remove(bid);
                }
                Effect::StatusChanged
            }
            "block_event" => self.block_event(ev["session_id"].as_str().unwrap_or(""), p),
            _ => Effect::Nothing,
        }
    }

    fn block_event(&mut self, sid: &str, p: &Value) -> Effect {
        let Some(bid) = p["block_id"].as_str() else { return Effect::Nothing };
        let body = &p["payload"];
        match p["name"].as_str() {
            Some("program_status_changed") => {
                let id = body["id"].as_str().or(body["record"]["id"].as_str()).unwrap_or("").to_string();
                let state = Status::parse(body["record"]["state"].as_str().unwrap_or(""));
                self.set_status(sid, bid, id, state);
                Effect::StatusChanged
            }
            Some("program_status_removed") => {
                if let Some((_, records)) = self.status.get_mut(bid) {
                    for id in body["ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                        records.remove(id);
                    }
                }
                Effect::StatusChanged
            }
            _ => Effect::Nothing,
        }
    }

    /// Records one program status record (as from `program_status` at startup).
    pub fn set_status(&mut self, session_id: &str, block_id: &str, record_id: String, state: Status) {
        let entry = self.status.entry(block_id.to_string()).or_insert_with(|| (session_id.to_string(), HashMap::new()));
        entry.1.insert(record_id, state);
    }

    /// Whether a change was announced that the held view doesn't have yet (call after a fetch
    /// lands: a change announced while it was running may not be in it).
    pub fn needs_refetch(&self) -> bool {
        self.current.is_some() && self.view.as_ref().and_then(revision).is_none_or(|have| self.announced > have)
            && self.announced > 0
    }

    /// Forgets announced revisions (when another session is shown).
    pub fn reset_announced(&mut self) {
        self.announced = 0;
    }

    /// Keeps `view` if it is newer than the one held; false if it was stale.
    pub fn set_view(&mut self, view: Value) -> bool {
        if let (Some(new), Some(old)) = (revision(&view), self.view.as_ref().and_then(revision))
            && new <= old
        {
            return false;
        }
        self.view = Some(view);
        true
    }

    pub fn block_status(&self, block_id: &str) -> Status {
        self.status.get(block_id).map_or(Status::Idle, |(_, r)| Status::most_pressing(r.values().copied()))
    }

    pub fn session_status(&self, session_id: &str) -> Status {
        Status::most_pressing(
            self.status.values().filter(|(sid, _)| sid == session_id).flat_map(|(_, r)| r.values().copied()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(rev: u64) -> Value {
        json!({"session_id": "s1", "revision": rev, "windows": []})
    }

    fn session_event(sid: &str, name: &str, payload: Value) -> Value {
        json!({"type": name, "session_id": sid, "payload": payload})
    }

    fn status_event(sid: &str, bid: &str, id: Option<&str>, state: &str) -> Value {
        let mut body = json!({"record": {"state": state}});
        if let Some(i) = id {
            body["id"] = json!(i);
            body["record"]["id"] = json!(i);
        }
        session_event(sid, "block_event", json!({"block_id": bid, "name": "program_status_changed", "payload": body}))
    }

    #[test]
    fn stale_revision_is_ignored() {
        let mut m = SessionModel { current: Some("s1".into()), ..Default::default() };
        assert!(m.set_view(view(5)));
        assert!(!m.set_view(view(4)));
        assert_eq!(m.view.as_ref().unwrap()["revision"], 5);
        assert_eq!(m.apply_event(&session_event("s1", "session_view_changed", json!({"revision": 3}))), Effect::Nothing);
        assert_eq!(
            m.apply_event(&session_event("s1", "session_view_changed", json!({"revision": 6}))),
            Effect::RefetchView("s1".into())
        );
        // Another session's view changes don't concern the current view.
        assert_eq!(m.apply_event(&session_event("s2", "session_view_changed", json!({"revision": 99}))), Effect::Nothing);
    }

    #[test]
    fn change_during_fetch_is_not_lost() {
        let mut m = SessionModel { current: Some("s1".into()), ..Default::default() };
        m.set_view(view(5));
        assert!(!m.needs_refetch());
        // Two changes announced; the fetch that was already running answers with rev 6.
        m.apply_event(&session_event("s1", "session_view_changed", json!({"revision": 6})));
        m.apply_event(&session_event("s1", "session_view_changed", json!({"revision": 7})));
        m.set_view(view(6));
        assert!(m.needs_refetch());
        m.set_view(view(7));
        assert!(!m.needs_refetch());
        // A new session starts over.
        m.current = Some("s2".into());
        m.view = None;
        m.reset_announced();
        assert!(!m.needs_refetch());
    }

    #[test]
    fn sessions_follow_events() {
        let mut m = SessionModel::default();
        assert_eq!(m.apply_event(&json!({"type": "session_created", "payload": {"session": {"session_id": "s1", "label": "a"}}})), Effect::SessionsChanged);
        m.apply_event(&json!({"type": "session_created", "payload": {"session_id": "s2", "label": "b"}}));
        m.apply_event(&session_event("s1", "session_label_changed", json!({"label": "work"})));
        assert_eq!(m.sessions, vec![SessionInfo { id: "s1".into(), label: "work".into() }, SessionInfo { id: "s2".into(), label: "b".into() }]);
        m.apply_event(&json!({"type": "session_destroyed", "payload": {"session_id": "s1"}}));
        assert_eq!(m.sessions.len(), 1);
        assert_eq!(m.sessions[0].id, "s2");
    }

    #[test]
    fn status_priority() {
        let mut m = SessionModel::default();
        assert_eq!(m.apply_event(&status_event("s1", "b1", None, "working")), Effect::StatusChanged);
        m.apply_event(&status_event("s1", "b1", Some("build"), "blocked"));
        assert_eq!(m.block_status("b1"), Status::Blocked);
        assert_eq!(m.session_status("s1"), Status::Blocked);
        m.apply_event(&session_event("s1", "block_event", json!({"block_id": "b1", "name": "program_status_removed", "payload": {"ids": ["build"]}})));
        assert_eq!(m.block_status("b1"), Status::Working);
        assert_eq!(m.session_status("s2"), Status::Idle);
        m.apply_event(&status_event("s1", "b1", None, "nonsense"));
        assert_eq!(m.block_status("b1"), Status::Idle);
    }

    #[test]
    fn closed_block_forgets_status() {
        let mut m = SessionModel::default();
        m.apply_event(&status_event("s1", "b1", None, "error"));
        m.apply_event(&session_event("s1", "block_closed", json!({"block_id": "b1"})));
        assert_eq!(m.session_status("s1"), Status::Idle);
    }
}
