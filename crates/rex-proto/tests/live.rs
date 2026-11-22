//! Tests against a real, isolated Rex server (see rex_proto::testing).

use std::time::{Duration, Instant};

use rex_proto::control::{CallError, Control};
use rex_proto::testing::TestServer;
use serde_json::{Value, json};

fn next_event(rx: &async_channel::Receiver<Value>, name: &str, within: Duration) -> Option<Value> {
    let start = Instant::now();
    while start.elapsed() < within {
        match rx.try_recv() {
            Ok(ev) if ev["type"] == name => return Some(ev),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

#[test]
fn control_hello_and_call() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let v = ctl.call("server.version", json!({})).unwrap();
    assert!(v["rex_version"].is_string(), "{v}");
    ctl.close();
}

#[test]
fn events_arrive_after_attach() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let created = ctl.call("session.create", json!({})).unwrap();
    let sid = created["session_id"].as_str().unwrap().to_string();
    ctl.call("session.attach", json!({"session_id": sid})).unwrap();
    ctl.call("session.set_label", json!({"session_id": sid, "label": "renamed"})).unwrap();
    let ev = next_event(&ctl.events(), "session_label_changed", Duration::from_secs(10)).expect("no session_label_changed");
    assert_eq!(ev["payload"]["label"], "renamed");
}

#[test]
fn remote_error_maps_to_call_error() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    match ctl.call("session.view", json!({})) {
        Err(CallError::Remote { code, .. }) => assert_eq!(code, "bad_request"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn reconnect_resumes_same_client() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let sid = ctl.call("session.create", json!({})).unwrap()["session_id"].as_str().unwrap().to_string();
    ctl.call("session.attach", json!({"session_id": sid})).unwrap();
    let id = ctl.client_id();
    let rx = ctl.events();
    ctl.drop_connection_for_test();
    next_event(&rx, "rexa_reconnecting", Duration::from_secs(5)).expect("no rexa_reconnecting");
    let ev = next_event(&rx, "rexa_reconnected", Duration::from_secs(10)).expect("no rexa_reconnected");
    assert_eq!(ev["resumed"], true);
    assert_eq!(ctl.client_id(), id);
    assert!(ctl.call("server.version", json!({})).is_ok());
}
