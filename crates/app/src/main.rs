//! rexa-desktop: a desktop client for Rex.
//!
//!     rexa-desktop [-S ENDPOINT] [SESSION]
//!
//! Without -S it shows this computer and the machines in the settings; with -S only that one.

mod config;
mod file_panel;
mod fonts;
mod hosts;
mod i18n;
mod keys;
mod locate;
mod machines;
mod palette;
mod preview;
mod panes;
mod server_info;
mod settings_view;
mod workspace;

use gpui_kit::*;
use rex_proto::transport::Endpoint;
use rexterm::terminal::default_key_bindings;

use crate::i18n::t;

const USAGE: &str = "usage: rexa-desktop [-S ENDPOINT] [SESSION]\n";

struct Args {
    /// The -S text and what it names.
    endpoint: Option<(String, Endpoint)>,
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
                let ep = Endpoint::parse(&v).map_err(|e| e.to_string())?;
                endpoint = Some((v, ep));
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            _ if session.is_none() && !a.starts_with('-') => session = Some(a),
            _ => return Err(format!("unknown argument: {a}\n{USAGE}")),
        }
    }
    Ok(Args { endpoint, session })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprint!("rexa-desktop: {e}");
            std::process::exit(2);
        }
    };
    let dir = config::config_dir(|k| std::env::var(k).ok());
    let (settings, broken) = match &dir {
        Some(d) => config::load_settings(d),
        None => (config::Settings::default(), None),
    };
    i18n::set_language(settings.language);
    let state = dir.as_deref().map(config::load_state).unwrap_or_default();
    let pinned = args.endpoint.is_some();
    let (specs, mut notices) = machines::machine_specs(args.endpoint, &settings, Endpoint::default_local());
    if let Some(why) = &broken {
        notices.insert(
            0,
            format!(
                "{} ({why}). {}",
                t("The settings file can't be used", "设置文件无法使用"),
                t("Using defaults; saving the settings replaces it and keeps the old one as settings.json.bak.", "暂用默认设置;在设置里保存会替换它,旧文件留作 settings.json.bak。")
            ),
        );
    }
    let start = machines::start_target(&specs, &state, args.session.as_deref());
    let init = workspace::Init {
        specs,
        start,
        strict: args.session.is_some(),
        settings,
        settings_broken: broken.is_some(),
        config: dir,
        // A machine pinned with -S isn't where the user left off.
        save_state: !pinned,
        state: state.clone(),
        notices,
    };
    let mut init = Some(init);
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        gpui_cn::init(cx);
        fonts::register(cx);
        cx.bind_keys(default_key_bindings());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let (w, h) = state.window_size.unwrap_or((1200., 760.));
        let bounds = Bounds::centered(None, size(px(w.max(480.)), px(h.max(320.))), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("rexa-desktop".into()), ..Default::default() }),
            app_id: Some("dev.rexa.Desktop".into()),
            ..Default::default()
        };
        let init = init.take().expect("run once");
        gpui_kit::open_window(options, cx, |window, cx| {
            // The app draws a dark UI; the component theme (code views) matches it.
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, Some(window), cx);
            let ws = cx.new(|cx| workspace::Workspace::new(init, window, cx));
            let w2 = ws.clone();
            // Unsaved files are asked about first; the answer finishes the close.
            window.on_window_should_close(cx, move |window, cx| w2.update(cx, |w, cx| w.request_close(window, cx)));
            ws
        })
        .expect("failed to open window");
    });
}
