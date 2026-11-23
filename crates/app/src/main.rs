//! rexa-desktop: a desktop client for Rex.
//!
//!     rexa-desktop [-S ENDPOINT] [SESSION]

mod config;
mod i18n;
mod keys;
mod machines;
mod palette;
mod panes;
mod workspace;

use gpui_kit::*;
use rex_proto::transport::Endpoint;
use rexterm::terminal::default_key_bindings;

const USAGE: &str = "usage: rexa-desktop [-S ENDPOINT] [SESSION]\n";

struct Args {
    endpoint: Endpoint,
    session: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut endpoint = None;
    let mut session = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-S" | "--server" => {
                let v = it.next().ok_or("-S needs an endpoint")?;
                endpoint = Some(Endpoint::parse(&v).map_err(|e| e.to_string())?);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            _ if session.is_none() && !a.starts_with('-') => session = Some(a),
            _ => return Err(format!("unknown argument: {a}\n{USAGE}")),
        }
    }
    Ok(Args { endpoint: endpoint.unwrap_or_else(Endpoint::default_local), session })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprint!("rexa-desktop: {e}");
            std::process::exit(2);
        }
    };
    let mut args = Some(args);
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        gpui_cn::init(cx);
        cx.bind_keys(default_key_bindings());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("rexa-desktop".into()), ..Default::default() }),
            app_id: Some("dev.rexa.Desktop".into()),
            ..Default::default()
        };
        let Args { endpoint, session } = args.take().expect("run once");
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| workspace::Workspace::new(endpoint, session, window, cx)))
            .expect("failed to open window");
    });
}
