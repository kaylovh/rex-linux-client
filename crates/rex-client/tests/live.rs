//! The session model against an isolated Rex server.

use std::time::{Duration, Instant};

use rex_client::model::{SessionModel, Status};
use rex_proto::control::Control;
use rex_proto::testing::TestServer;
use serde_json::json;

#[test]
fn status_from_osc_7501() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let created = ctl
        .call("session.create", json!({"initial_windows": [{"layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}}]}))
        .unwrap();
    let sid = created["session_id"].as_str().unwrap().to_string();
    let att = ctl.call("session.attach", json!({"session_id": sid})).unwrap();
    let bid = att["view"]["windows"][0]["focused_block_id"].as_str().unwrap().to_string();
    let mut model = SessionModel::default();
    let rx = ctl.events();
    // OSC 7501 reports a status: ESC ] 7501 ; state=<state>[:id=<id>][:kind=<kind>] ESC \
    ctl.call(
        "com.superlogical.terminal.write",
        json!({"session_id": sid, "block_id": bid, "args": {"data": "printf '\\033]7501;state=working\\033\\\\'\n"}}),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while model.block_status(&bid) != Status::Working {
        assert!(Instant::now() < deadline, "never saw the working status");
        while let Ok(ev) = rx.try_recv() {
            model.apply_event(&ev);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(model.session_status(&sid), Status::Working);
}
