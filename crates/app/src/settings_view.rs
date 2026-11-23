//! The settings dialog's content: language, machines and the terminal font.

use gpui_cn::{Button, Input};
use gpui_kit::base::input::InputState;
use gpui_kit::{
    App, AppContext as _, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Window, div, px, rgb,
};
use rexterm::terminal::{TerminalAppearance, TerminalConfig};

use crate::config::{FontSettings, Language, MachineConfig, Settings, ShownHosts};
use crate::hosts::{shown_for, toggle_shown};
use crate::i18n::t;
use crate::machines::validate_machine;

/// The terminal's look from the settings; what isn't set follows the theme.
pub fn terminal_config(font: &FontSettings) -> TerminalConfig {
    let mut a = TerminalAppearance::default();
    if let Some(f) = &font.family {
        a = a.with_font_family(f.clone());
    }
    if let Some(s) = font.size.filter(|s| (6.0..=72.0).contains(s)) {
        a = a.with_font_size(px(s));
    }
    if !font.fallbacks.is_empty() {
        a = a.with_font_fallbacks(font.fallbacks.clone());
    }
    TerminalConfig::default().with_appearance(a)
}

/// The font fields as typed: empty means "the theme's"; the fallbacks are comma-separated.
pub fn parse_font(family: &str, size: &str, fallbacks: &str) -> Result<FontSettings, String> {
    let family = Some(family.trim().to_string()).filter(|f| !f.is_empty());
    let size = match size.trim() {
        "" => None,
        s => match s.parse::<f32>() {
            Ok(v) if (6.0..=72.0).contains(&v) => Some(v),
            _ => return Err(t("The font size must be a number from 6 to 72.", "字号必须是 6 到 72 之间的数字。").into()),
        },
    };
    let fallbacks = fallbacks.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    Ok(FontSettings { family, size, fallbacks })
}

/// One server's Rex hosts, for choosing which the sidebar shows.
pub struct HostGroup {
    pub server: String,
    pub name: String,
    pub labels: Vec<String>,
    pub default_visible: bool,
}

pub enum SettingsEvent {
    Save(Settings),
    Cancel,
}

/// The dialog's fields, edited in place until Save.
pub struct SettingsView {
    language: Language,
    machines: Vec<MachineConfig>,
    /// The machine is pinned with -S: the list is shown but not edited here.
    pinned: bool,
    family: Entity<InputState>,
    size: Entity<InputState>,
    fallbacks: Entity<InputState>,
    new_name: Entity<InputState>,
    new_endpoint: Entity<InputState>,
    error: Option<String>,
    focus: FocusHandle,
    host_groups: Vec<HostGroup>,
    shown_hosts: Vec<ShownHosts>,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsView {
    pub fn new(settings: &Settings, pinned: bool, host_groups: Vec<HostGroup>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |value: String, placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let f = &settings.font;
        Self {
            language: settings.language,
            machines: settings.machines.clone(),
            pinned,
            family: input(f.family.clone().unwrap_or_default(), t("theme font", "主题字体"), window, cx),
            size: input(f.size.map(|s| s.to_string()).unwrap_or_default(), t("theme size", "主题字号"), window, cx),
            fallbacks: input(f.fallbacks.join(", "), "Noto Color Emoji, Symbols Nerd Font Mono", window, cx),
            new_name: input(String::new(), t("name, e.g. dev", "名称,如 dev"), window, cx),
            new_endpoint: input(String::new(), "ssh://user@host", window, cx),
            error: None,
            focus: cx.focus_handle(),
            host_groups,
            shown_hosts: settings.shown_hosts.clone(),
        }
    }

    fn host_visible(&self, group: &HostGroup, label: &str) -> bool {
        shown_for(&self.shown_hosts, &group.server).map_or(group.default_visible, |s| s.iter().any(|l| l == label))
    }

    fn toggle_host(&mut self, group: usize, label: &str, cx: &mut Context<Self>) {
        let Some(g) = self.host_groups.get(group) else { return };
        let visible: Vec<String> = g.labels.iter().filter(|l| self.host_visible(g, l)).cloned().collect();
        let server = g.server.clone();
        toggle_shown(&mut self.shown_hosts, &server, label, &visible);
        cx.notify();
    }

    pub fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error);
        cx.notify();
    }

    fn add_machine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_name.read(cx).value().to_string();
        let endpoint = self.new_endpoint.read(cx).value().to_string();
        match validate_machine(&name, &endpoint, &self.machines) {
            Ok(m) => {
                self.machines.push(m);
                self.error = None;
                self.new_name.update(cx, |i, cx| i.set_value("", window, cx));
                self.new_endpoint.update(cx, |i, cx| i.set_value("", window, cx));
            }
            Err(e) => self.error = Some(e.text()),
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let read = |e: &Entity<InputState>, cx: &Context<Self>| e.read(cx).value().to_string();
        match parse_font(&read(&self.family, cx), &read(&self.size, cx), &read(&self.fallbacks, cx)) {
            Ok(font) => {
                let s = Settings { language: self.language, machines: self.machines.clone(), font, shown_hosts: self.shown_hosts.clone() };
                self.error = None;
                cx.emit(SettingsEvent::Save(s));
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    fn section(title: &'static str) -> Div {
        div().pt_2().text_sm().text_color(rgb(0x9a9a9a)).child(title)
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang_button = |id: &'static str, label: &'static str, l: Language, cx: &mut Context<Self>| {
            let b = Button::new(id).label(label).on_click(cx.listener(move |this, _, _, cx| {
                this.language = l;
                cx.notify();
            }));
            if self.language == l { b.primary() } else { b.outline() }
        };
        let field = |label: &'static str, input: &Entity<InputState>| {
            div().flex().items_center().gap_2().child(div().w(px(110.)).text_sm().child(label)).child(div().flex_1().child(Input::new(input)))
        };
        let mut machines = div().flex().flex_col().gap_1();
        machines = machines.child(
            div().flex().gap_2().text_sm().child(div().w(px(110.)).child(t("This computer", "本机"))).child(div().text_color(rgb(0x9a9a9a)).child(t("always shown", "始终显示"))),
        );
        for (i, m) in self.machines.iter().enumerate() {
            let row = div()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child(div().w(px(110.)).child(m.name.clone()))
                .child(div().flex_1().text_color(rgb(0x9a9a9a)).child(m.endpoint.clone()));
            machines = machines.child(if self.pinned {
                row
            } else {
                row.child(
                    Button::new(SharedString::from(format!("remove-machine-{i}"))).ghost().label(t("Remove", "移除")).on_click(cx.listener(move |this, _, _, cx| {
                        if i < this.machines.len() {
                            this.machines.remove(i);
                        }
                        cx.notify();
                    })),
                )
            });
        }
        if self.pinned {
            machines = machines.child(div().text_sm().text_color(rgb(0x9a9a9a)).child(t(
                "Started with -S: only that machine is shown, and the list can't be changed here.",
                "用 -S 启动时只显示那一台机器,这里不能修改列表。",
            )));
        } else {
            machines = machines.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(110.)).child(Input::new(&self.new_name)))
                    .child(div().flex_1().child(Input::new(&self.new_endpoint)))
                    .child(Button::new("add-machine").outline().label(t("Add", "添加")).on_click(cx.listener(|this, _, window, cx| this.add_machine(window, cx)))),
            );
        }
        div()
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .gap_2()
            .min_w(px(560.))
            .child(Self::section(t("Language", "语言")))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(lang_button("lang-auto", t("Follow system", "跟随系统"), Language::Auto, cx))
                    .child(lang_button("lang-en", "English", Language::En, cx))
                    .child(lang_button("lang-zh", "中文", Language::Zh, cx)),
            )
            .child(Self::section(t("Machines", "机器")))
            .child(machines)
            .children((!self.host_groups.is_empty()).then(|| Self::section(t("Rex hosts shown in the sidebar", "侧栏显示的 Rex hosts"))))
            .children(self.host_groups.iter().enumerate().map(|(gi, g)| {
                let mut row = div().flex().flex_wrap().items_center().gap_2().child(div().w(px(110.)).text_sm().child(g.name.clone()));
                for (li, label) in g.labels.iter().enumerate() {
                    let on = self.host_visible(g, label);
                    let l = label.clone();
                    let b = Button::new(SharedString::from(format!("host-{gi}-{li}")))
                        .label(label.clone())
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_host(gi, &l, cx)));
                    row = row.child(if on { b.primary() } else { b.outline() });
                }
                row
            }))
            .child(Self::section(t("Terminal font", "终端字体")))
            .child(field(t("Family", "字体"), &self.family))
            .child(field(t("Size", "字号"), &self.size))
            .child(field(t("Fallbacks", "备用字体"), &self.fallbacks))
            .children(self.error.clone().map(|e| div().text_sm().text_color(rgb(0xe06c6c)).child(e)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .child(Button::new("settings-cancel").label(t("Cancel", "取消")).on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Cancel))))
                    .child(Button::new("settings-save").primary().label(t("Save", "保存")).on_click(cx.listener(|this, _, _, cx| this.save(cx)))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_config_uses_font_settings() {
        let font = FontSettings { family: Some("Iosevka".into()), size: Some(16.0), fallbacks: vec!["Noto Color Emoji".into()] };
        let c = terminal_config(&font);
        let a = c.appearance();
        assert_eq!(a.font_family().map(|f| f.to_string()), Some("Iosevka".to_string()));
        assert_eq!(a.font_size(), Some(px(16.0)));
        assert_eq!(a.font_fallbacks(), ["Noto Color Emoji".to_string()]);
        // Nothing set: the theme's font.
        let d = terminal_config(&FontSettings { family: None, size: None, fallbacks: vec![] });
        assert_eq!(d.appearance().font_family(), None);
        assert_eq!(d.appearance().font_size(), None);
        // A hand-edited size outside 6–72 is ignored.
        for bad in [0.0, -4.0, 500.0, f32::NAN] {
            assert_eq!(terminal_config(&FontSettings { family: None, size: Some(bad), fallbacks: vec![] }).appearance().font_size(), None);
        }
    }

    #[test]
    fn parse_font_fields() {
        let f = parse_font(" JetBrains Mono ", "14", "Noto Color Emoji, , Symbols Nerd Font Mono ").unwrap();
        assert_eq!(f.family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(f.size, Some(14.0));
        assert_eq!(f.fallbacks, ["Noto Color Emoji", "Symbols Nerd Font Mono"]);
        let e = parse_font("", "", "").unwrap();
        assert_eq!((e.family, e.size, e.fallbacks.len()), (None, None, 0));
        assert!(parse_font("", "abc", "").is_err());
        assert!(parse_font("", "3", "").is_err());
        assert!(parse_font("", "100", "").is_err());
    }
}
