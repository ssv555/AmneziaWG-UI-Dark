//! Правая часть окна: сведения о выбранном туннеле (состояние, трафик, график, конфиг).

use eframe::egui::{self, RichText, Ui, Vec2};

use super::a11y;
use super::graph::{graph, stale_ping, GraphPause};
use super::theme::{dot, level_color, mono, palette};
use super::list::Primary;
use super::{menu, Action, ROW_H};
use crate::conf::{PeerInfo, TunnelInfo};
use crate::daemon::proto::Plan;
use crate::fmt;
use crate::health::Health;
use crate::i18n::{tr, trf};
use crate::monitor::{self, Live, Snapshot};
use crate::ping::PingState;
use crate::settings::{Mode, Settings};
use crate::stats::{self, Stats};

pub(super) struct Detail<'a> {
    pub(super) name: &'a str,
    pub(super) live: Option<&'a Live>,
    pub(super) health: Health,
    pub(super) busy: bool,
    pub(super) ping: &'a PingState,
    /// Агент (он ведёт пинг) не отвечает: вместо значения — «вторичная служба недоступна», полосы пинга нет.
    pub(super) ping_unavailable: bool,
    pub(super) stats: &'a Stats,
    /// Сведения о конфиге из источника или родного окна и откуда они.
    pub(super) info: Option<&'a (TunnelInfo, String)>,
    pub(super) info_loading: bool,
    /// Ядро не смогло подключить туннель за 10 минут и пробует раз в 10 минут: можно повторить сразу.
    pub(super) retry_slow: bool,
    /// Нет связи с ядром: состояние туннеля неизвестно, главное действие недоступно.
    pub(super) core_lost: bool,
    /// Снимок состояния: какие туннели снимет «Подключить» (`Primary::connect_hint`).
    pub(super) snap: &'a Snapshot,
}

/// Подключённый туннель: сведения из службы (UAPI), адреса/DNS/MTU — из конфига, если он известен.
fn info_from_status(st: &crate::uapi::Status, conf: Option<&TunnelInfo>) -> TunnelInfo {
    let base = conf.cloned().unwrap_or_default();
    TunnelInfo {
        public_key: st.public_key.clone(),
        listen_port: st.listen_port.to_string(),
        mtu: base.mtu,
        addresses: base.addresses,
        dns: base.dns,
        awg: st.awg_params.clone(),
        peers: st
            .peers
            .iter()
            .map(|p| PeerInfo {
                public_key: p.public_key.clone(),
                preshared: conf.and_then(|c| c.peers.iter().find(|cp| cp.public_key == p.public_key)).is_some_and(|cp| cp.preshared),
                endpoint: p.endpoint.clone(),
                allowed_ips: p.allowed_ips.clone(),
                keepalive: if p.keepalive == 0 { String::new() } else { p.keepalive.to_string() },
            })
            .collect(),
    }
}

/// Сетка из 4 колонок равной ширины: подпись, значение, подпись, значение. Ширина не зависит от текста.
/// Подписи — по левому краю, значения — по правому краю своей колонки.
fn fixed_grid(ui: &mut Ui, id: &str, rows: &[[RichText; 4]]) {
    let spacing = 24.0;
    let col = ((ui.available_width() - spacing * 3.0) / 4.0).max(60.0);
    egui::Grid::new(id).num_columns(4).spacing([spacing, 6.0]).min_col_width(col).max_col_width(col).show(ui, |ui| {
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                let layout = if i % 2 == 1 {
                    egui::Layout::right_to_left(egui::Align::Center)
                } else {
                    egui::Layout::left_to_right(egui::Align::Center)
                };
                ui.allocate_ui_with_layout(Vec2::new(col, ROW_H - 4.0), layout, |ui| {
                    let resp = ui.add(egui::Label::new(cell.clone()).truncate().sense(egui::Sense::click()));
                    // Значения (нечётные колонки) копируются правым щелчком; секретов в таблице состояния нет.
                    if i % 2 == 1 && !cell.text().is_empty() {
                        copy_menu(&resp, cell.text());
                    }
                });
            }
            ui.end_row();
        }
    });
}

fn label(text: &str) -> RichText {
    RichText::new(text).weak()
}

pub(super) fn details(ui: &mut Ui, d: &Detail, s: &mut Settings, pause: &mut GraphPause, actions: &mut Vec<Action>) {
    let status = d.live.and_then(|l| l.status.as_ref());
    let running = d.live.is_some();
    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let name = ui.add(egui::Label::new(RichText::new(d.name).size(18.0).strong()).sense(egui::Sense::click()));
            copy_menu(&name, d.name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // То же решение, что у меню строки и трея: туннель, который ядро переподключает, — «Отключить».
                let primary = Primary::of(d.health.level, d.core_lost);
                let main = egui::Button::new(RichText::new(tr(primary.label())).size(16.0)).min_size(Vec2::new(140.0, 32.0));
                let plan = primary.plan().filter(|_| !d.busy);
                let button = ui.add_enabled(plan.is_some(), main);
                let button = match primary.hint() {
                    Some(hint) => button.on_disabled_hover_text(tr(hint)),
                    None => button,
                };
                let button = match primary.connect_hint(d.name, s.multiple, d.snap) {
                    Some(hint) => button.on_hover_text(hint),
                    None => button,
                };
                if let (true, Some(plan)) = (button.clicked(), plan) {
                    actions.push(Action::Switch(d.name.to_string(), plan));
                }
                if d.retry_slow {
                    let retry = egui::Button::new(RichText::new(tr("act.retry")).size(16.0)).min_size(Vec2::new(140.0, 32.0));
                    if ui.add_enabled(!d.busy, retry).clicked() {
                        actions.push(Action::Retry(d.name.to_string()));
                    }
                }
                if s.view.reconnect && running {
                    let re = egui::Button::new(RichText::new(tr("act.reconnect")).size(16.0)).min_size(Vec2::new(150.0, 32.0));
                    if ui.add_enabled(!d.busy, re).clicked() {
                        actions.push(Action::Switch(d.name.to_string(), Plan::Reconnect));
                    }
                }
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            dot(ui, level_color(d.health.level), 9.0, &a11y::state_word(Primary::of(d.health.level, d.core_lost)));
            ui.label(RichText::new(&d.health.text).size(22.0).strong().color(level_color(d.health.level)));
        });
        ui.add_space(8.0);

        let mut rows: Vec<[RichText; 4]> = Vec::new();
        if let (Some(live), Some(st)) = (d.live, status) {
            let (rx_rate, tx_rate) = live.rate(3.0);
            let hs = st.last_handshake_sec();
            let age = if hs == 0 { tr("st.never") } else { fmt::ago(monitor::unix_now().saturating_sub(hs)) };
            let endpoint = st.peers.first().map(|p| p.endpoint.clone()).unwrap_or_else(|| "—".into());
            rows.push([label(&tr("st.handshake")), mono(age, palette().text_strong), label(&tr("st.endpoint")), mono(endpoint, palette().text_strong)]);
            rows.push([
                label(&tr("st.received")),
                mono(fmt::bytes(st.rx_bytes() as f64), palette().graph_rx),
                label(&tr("st.sent")),
                mono(fmt::bytes(st.tx_bytes() as f64), palette().graph_tx),
            ]);
            rows.push([
                label(&tr("st.rx_rate")),
                mono(fmt::rate(rx_rate), palette().graph_rx),
                label(&tr("st.tx_rate")),
                mono(fmt::rate(tx_rate), palette().graph_tx),
            ]);
            if s.view.ping {
                // Нет ответа — прошлое значение и «н/д» красным; причина видна в состоянии и на полосе пинга.
                let value = match &d.ping.last {
                    _ if d.ping_unavailable => mono(tr("agent.unavailable"), palette().idle),
                    None => mono("…", palette().idle),
                    Some(Ok(ms)) => mono(trf("unit.ms", &[&ms.to_string()]), palette().graph_ping),
                    Some(Err(_)) => mono(stale_ping(d.ping), palette().error),
                };
                rows.push([label(&trf("st.ping_to", &[&d.ping.host])), value, label(""), label("")]);
            }
        }
        if s.view.totals {
            if let Some(t) = d.stats.get(d.name) {
                let share = stats::share(d.stats, d.name);
                rows.push([
                    label(&tr("tot.rx")),
                    mono(fmt::bytes(t.rx as f64), palette().graph_rx),
                    label(&tr("tot.tx")),
                    mono(fmt::bytes(t.tx as f64), palette().graph_tx),
                ]);
                rows.push([
                    label(&tr("tot.peak")),
                    mono(fmt::rate(t.peak_rx), palette().graph_rx),
                    label(&tr("tot.time")),
                    mono(format!("{} ({})", fmt::duration(t.seconds), fmt::percent(share)), palette().text_strong),
                ]);
                rows.push([label(&tr("tot.since")), mono(fmt::date_time(t.since), palette().idle), label(""), label("")]);
            }
        }
        fixed_grid(ui, "status-grid", &rows);

        if let (true, Some(live)) = (s.view.graph, d.live) {
            ui.add_space(8.0);
            graph(ui, d.name, live, (s.view.ping && !d.ping_unavailable).then_some(d.ping), s, pause);
        }
    });

    if !s.view.details {
        return;
    }
    ui.add_space(8.0);
    // Подключён — данные службы; нет — конфиг из источника или из родного окна.
    let (info, origin) = match (status, d.info) {
        (Some(st), conf) => (Some(info_from_status(st, conf.map(|c| &c.0))), None),
        (None, Some((info, origin))) => (Some(info.clone()), Some(origin.as_str())),
        (None, None) => (None, None),
    };
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match info {
        Some(info) => config_sections(ui, &info, origin),
        None if s.mode() == Mode::Engine => {
            ui.weak(tr("det.not_loaded"));
        }
        None => {
            ui.weak(tr("det.not_loaded"));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let read = egui::Button::new(tr("det.read_native"));
                if ui.add_enabled(!d.info_loading, read).on_hover_text(tr("det.read_native_hint")).clicked() {
                    actions.push(Action::ReadNative(d.name.to_string()));
                }
                if d.info_loading {
                    ui.spinner();
                }
            });
        }
    });
}

/// Строка раздела сведений: подпись, значение и можно ли его скопировать (правый щелчок -> «Копировать»).
#[derive(Debug, PartialEq)]
struct InfoRow {
    label: String,
    value: String,
    copy: bool,
}

impl InfoRow {
    fn new(label: String, value: String, copy: bool) -> Self {
        InfoRow { label, value, copy }
    }
}

/// Раздел «Интерфейс». Приватного ключа в `TunnelInfo` нет (`conf::parse` хранит только открытый) — копировать нечего.
fn interface_rows(info: &TunnelInfo) -> Vec<InfoRow> {
    let awg: Vec<String> = info.awg.iter().map(|(k, v)| format!("{k}={v}")).collect();
    vec![
        InfoRow::new(tr("det.pubkey"), info.public_key.clone(), true),
        InfoRow::new(tr("det.addresses"), info.addresses.join(", "), true),
        InfoRow::new(tr("det.dns"), info.dns.join(", "), true),
        InfoRow::new(tr("det.port"), info.listen_port.clone(), true),
        InfoRow::new(tr("det.mtu"), info.mtu.clone(), false),
        InfoRow::new("AWG".to_string(), awg.join("  "), false),
    ]
}

/// Раздел «Пир». Общий ключ (PSK) — секрет: показывается только «есть / нет» и не копируется.
fn peer_rows(peer: &PeerInfo) -> Vec<InfoRow> {
    let keepalive = match peer.keepalive.parse::<f64>() {
        Ok(k) if k > 0.0 => fmt::duration(k),
        _ => tr("det.off"),
    };
    vec![
        InfoRow::new(tr("det.pubkey"), peer.public_key.clone(), true),
        InfoRow::new(tr("st.endpoint"), peer.endpoint.clone(), true),
        InfoRow::new(tr("det.keepalive"), keepalive, false),
        InfoRow::new(tr("det.preshared"), tr(if peer.preshared { "det.yes" } else { "det.no" }), false),
    ]
}

/// Правый щелчок или Shift+F10 / клавиша меню на значении под фокусом (Tab) -> «Копировать» в буфер обмена.
fn copy_menu(resp: &egui::Response, value: &str) {
    menu::context_menu(resp, resp.has_focus(), |ui| {
        if ui.button(tr("det.copy")).clicked() {
            ui.ctx().copy_text(value.to_string());
            ui.close();
        }
    });
}

/// Значение моноширинным шрифтом с переносом; копируемое — с меню «Копировать».
fn value_label(ui: &mut Ui, value: &str, copy: bool) {
    let resp = ui.add(egui::Label::new(RichText::new(value).monospace()).wrap().sense(egui::Sense::click()));
    if copy {
        copy_menu(&resp, value);
    }
}

/// Разделы «Интерфейс» и «Пир»; `origin` — откуда сведения, если туннель не подключён.
fn config_sections(ui: &mut Ui, info: &TunnelInfo, origin: Option<&str>) {
    if let Some(o) = origin {
        ui.weak(o);
        ui.add_space(4.0);
    }
    let grid = |ui: &mut Ui, rows: Vec<InfoRow>| {
        for row in rows.into_iter().filter(|r| !r.value.is_empty()) {
            ui.weak(row.label);
            value_label(ui, &row.value, row.copy);
            ui.end_row();
        }
    };
    egui::CollapsingHeader::new(RichText::new(tr("det.interface")).strong()).default_open(true).show(ui, |ui| {
        egui::Grid::new("iface").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| grid(ui, interface_rows(info)));
    });
    for (i, peer) in info.peers.iter().enumerate() {
        egui::CollapsingHeader::new(RichText::new(tr("det.peer")).strong()).id_salt(("peer", i)).default_open(true).show(ui, |ui| {
            egui::Grid::new(("peer-grid", i)).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| grid(ui, peer_rows(peer)));
            egui::CollapsingHeader::new(trf("det.allowed", &[&peer.allowed_ips.len().to_string()]))
                .id_salt(("ips", i))
                .default_open(false)
                .show(ui, |ui| value_label(ui, &peer.allowed_ips.join(", "), true));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Пример из man wg(8) с общим ключом: ни приватный, ни общий ключ в строки сведений не попадают.
    const PRIVATE: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
    const PRESHARED: &str = "FpCyhws9cxwWoV4xELtfJvjJN+zQVRPISllRWgeopVE=";

    #[test]
    fn copyable_values_never_include_secrets() {
        let text = format!(
            "[Interface]\nPrivateKey = {PRIVATE}\nAddress = 10.0.0.2/32\nListenPort = 51820\n[Peer]\n\
             PublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nPresharedKey = {PRESHARED}\nEndpoint = 203.0.113.5:51820\n"
        );
        let info = crate::conf::parse(&text);
        let rows: Vec<InfoRow> = interface_rows(&info).into_iter().chain(info.peers.iter().flat_map(peer_rows)).collect();
        for row in &rows {
            assert!(!row.value.contains(PRIVATE) && !row.value.contains(PRESHARED), "secret in {row:?}");
        }
        let copyable: Vec<&str> = rows.iter().filter(|r| r.copy).map(|r| r.value.as_str()).collect();
        assert_eq!(copyable, vec!["HIgo9xNzJMWLKASShiTqIybxZ0U3wGLiUeJ1PKf8ykw=", "10.0.0.2/32", "", "51820", "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=", "203.0.113.5:51820"]);
        let psk = rows.iter().find(|r| r.label == tr("det.preshared")).expect("preshared row");
        assert_eq!((psk.value.as_str(), psk.copy), (tr("det.yes").as_str(), false));
    }
}
