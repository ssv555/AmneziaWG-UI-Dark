//! Локализация. Английский — по умолчанию и запасной, русский встроен. Остальные языки — файлы
//! `lang\<код ISO 639-2>.lng` рядом с exe (deu.lng, fra.lng …): INI с секциями `[language]` и `[strings]`.
//! Нет ключа, пустой перевод, опечатка в ключе — берётся английский текст.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::ini::Ini;

pub const DEFAULT: &str = "eng";
pub const EXT: &str = "lng";

/// Ключ, английский, русский.
const STRINGS: &[(&str, &str, &str)] = &[
    ("app.ungrouped", "Ungrouped", "Без группы"),
    ("period.2m", "2 min", "2 мин"),
    ("period.10m", "10 min", "10 мин"),
    ("period.1h", "1 h", "1 ч"),
    ("busy.connect", "Connecting…", "Подключение…"),
    ("busy.disconnect", "Disconnecting…", "Отключение…"),
    ("busy.reconnect", "Reconnecting…", "Переподключение…"),
    ("dlg.new_group", "New group", "Новая группа"),
    ("dlg.rename_group", "Rename group", "Переименовать группу"),
    ("dlg.new_subgroup", "New subgroup in “{0}”", "Новая подгруппа в «{0}»"),
    ("dlg.name", "Name", "Название"),
    ("dlg.err_slash", "The name cannot contain “/” — it separates levels", "В названии не может быть «/» — это разделитель уровней"),
    ("dlg.err_exists", "A group with this name already exists here", "Группа с таким названием здесь уже есть"),
    ("btn.dismiss", "Dismiss", "Скрыть"),
    ("btn.ok", "OK", "ОК"),
    ("btn.cancel", "Cancel", "Отмена"),
    ("btn.close", "Close", "Закрыть"),
    ("empty.no_tunnels", "No tunnels. Import a config in the AmneziaWG window (Settings menu).", "Туннелей нет. Импортируйте конфиг в родном окне AmneziaWG (меню «Настройки»)."),
    ("service.running", "service {0} is running", "служба {0} работает"),
    ("service.started", "service {0} was stopped — started", "служба {0} была остановлена — запущена"),
    ("menu.view", "View", "Вид"),
    ("view.groups", "Groups", "Группы"),
    ("view.search", "Search", "Поиск"),
    ("view.columns", "List columns", "Колонки списка"),
    ("view.col_rx", "Downloaded total", "Скачано всего"),
    ("view.col_tx", "Uploaded total", "Отдано всего"),
    ("view.col_peak", "Peak speed", "Пиковая скорость"),
    ("view.col_share", "Time share", "Доля времени"),
    ("view.right", "Right side", "Справа"),
    ("view.totals", "Tunnel totals", "Итоги по туннелю"),
    ("view.graph", "Speed graph", "График скорости"),
    ("view.ping", "Ping through VPN", "Пинг через VPN"),
    ("view.reconnect", "“Reconnect” button", "Кнопка «Переподключить»"),
    ("view.details", "Interface and peer", "Интерфейс и пир"),
    ("view.log", "Event log", "Журнал событий"),
    ("view.scale", "Interface scale: {0} %", "Масштаб интерфейса: {0} %"),
    ("view.scale_keys", "Ctrl+Plus / Ctrl+Minus, Ctrl+0 = 100 %", "Ctrl+Плюс / Ctrl+Минус, Ctrl+0 = 100 %"),
    ("menu.settings", "Settings", "Настройки"),
    ("set.multiple", "Several tunnels at once", "Несколько туннелей одновременно"),
    ("set.tray", "Tray icon", "Значок в трее"),
    ("set.notify", "Windows notifications", "Уведомления Windows"),
    ("set.close_to_tray", "Closing the window hides it to the tray", "Закрытие окна сворачивает в трей"),
    ("set.taskbar", "Connection state on the taskbar icon", "Состояние подключения на значке в панели задач"),
    ("set.autostart", "Start at Windows sign-in", "Запускать при входе в Windows"),
    ("set.ping_to", "Ping host", "Пинг до"),
    ("set.original", "AmneziaWG window (import, edit)", "Родное окно AmneziaWG (импорт, правка)"),
    ("sync.title", "Synchronize with source", "Синхронизация с источником"),
    ("sync.to_source", "AmneziaWG → source", "AmneziaWG → источник"),
    ("sync.to_native", "Source → AmneziaWG", "Источник → AmneziaWG"),
    ("sync.to_source_hint", "Read the config from the AmneziaWG editor and overwrite the source file", "Прочитать конфиг из редактора AmneziaWG и перезаписать файл-источник"),
    ("sync.to_native_hint", "Put the source file text into the AmneziaWG editor and press Save", "Подставить текст источника в редактор AmneziaWG и нажать Save"),
    ("sync.other_file", "Choose another source file…", "Указать другой файл-источник…"),
    ("sync.confirm_to_source", "The source file {1} will be overwritten with the config of tunnel {0} from AmneziaWG. Comments in the source will be lost: AmneziaWG does not keep them. Continue?", "Файл-источник {1} будет перезаписан конфигом туннеля {0} из AmneziaWG. Комментарии в источнике пропадут: AmneziaWG их не хранит. Продолжить?"),
    ("sync.confirm_to_native", "The config of tunnel {0} in AmneziaWG will be replaced with the text of {1} and saved by AmneziaWG. A connected tunnel may reconnect. Continue?", "Конфиг туннеля {0} в AmneziaWG будет заменён текстом {1} и сохранён самим AmneziaWG. Подключённый туннель может переподключиться. Продолжить?"),
    ("sync.running", "Synchronizing {0} through the AmneziaWG window…", "Синхронизация {0} через окно AmneziaWG…"),
    ("sync.done_to_source", "Source updated from AmneziaWG: {0}", "Источник обновлён из AmneziaWG: {0}"),
    ("sync.done_to_native", "AmneziaWG updated from the source: {0}", "AmneziaWG обновлён из источника: {0}"),
    ("err.native_busy", "The AmneziaWG window is busy with its dialog “{0}” — close it and try again", "Окно AmneziaWG занято своим диалогом «{0}» — закройте его и повторите"),
    ("err.native_rejected", "AmneziaWG did not accept the config of {0} — its editor stays open with the reason", "AmneziaWG не принял конфиг {0} — его редактор остался открытым с причиной"),
    ("det.addresses", "Addresses", "Адреса"),
    ("det.dns", "DNS servers", "DNS-серверы"),
    ("det.mtu", "MTU", "MTU"),
    ("det.preshared", "Preshared key", "Общий ключ (PSK)"),
    ("det.yes", "yes", "есть"),
    ("det.no", "no", "нет"),
    ("det.from_source", "Not connected — config from the source file {0}", "Не подключён — конфиг из файла-источника {0}"),
    ("det.from_native", "Not connected — read from the AmneziaWG window at {0}", "Не подключён — прочитано из окна AmneziaWG в {0}"),
    ("det.not_loaded", "The tunnel is not connected. Its config is stored encrypted by AmneziaWG; link a source file or read the details from the AmneziaWG window.", "Туннель не подключён. Его конфиг AmneziaWG хранит зашифрованным: привяжите файл-источник или прочитайте сведения из окна AmneziaWG."),
    ("det.read_native", "Read from AmneziaWG", "Прочитать из AmneziaWG"),
    ("det.read_native_hint", "Opens the AmneziaWG window and reads the public details of this tunnel (no private key)", "Откроет окно AmneziaWG и прочитает открытые сведения туннеля (без приватного ключа)"),
    ("del.tunnel_menu", "Delete tunnel…", "Удалить туннель…"),
    ("del.tunnel_title", "Delete tunnel", "Удаление туннеля"),
    ("del.tunnel_text", "Tunnel {0} will be deleted from AmneziaWG. This cannot be undone.", "Туннель {0} будет удалён из AmneziaWG. Отменить это нельзя."),
    ("del.tunnel_running", "The tunnel is connected — it will be disconnected.", "Туннель подключён — он будет отключён."),
    ("del.tunnel_has_source", "The source file stays: {0} — the tunnel can be imported again.", "Файл-источник останется: {0} — туннель можно будет импортировать снова."),
    ("del.tunnel_no_source", "No source file is known — the config will be lost. Save a copy first.", "Файла-источника нет — конфиг будет потерян. Сначала сохраните копию."),
    ("del.save_copy", "Save a copy to a file and delete…", "Сохранить копию в файл и удалить…"),
    ("del.save_copy_hint", "Reads the full config from the AmneziaWG editor into a .conf file, then deletes the tunnel", "Читает полный конфиг из редактора AmneziaWG в файл .conf, затем удаляет туннель"),
    ("del.delete", "Delete", "Удалить"),
    ("del.running", "Deleting {0} in AmneziaWG…", "Удаление {0} в AmneziaWG…"),
    ("del.done", "Tunnel {0} deleted from AmneziaWG", "Туннель {0} удалён из AmneziaWG"),
    ("del.done_copy", "Tunnel {0} deleted from AmneziaWG; copy saved: {1}", "Туннель {0} удалён из AmneziaWG; копия сохранена: {1}"),
    ("del.group_title", "Delete group", "Удаление группы"),
    ("del.group_text", "Delete group “{0}”? Subgroups: {1}, tunnels: {2} — they move to “{3}”. Tunnels are not deleted from AmneziaWG.", "Удалить группу «{0}»? Подгрупп: {1}, туннелей: {2} — они перейдут в «{3}». Из AmneziaWG туннели не удаляются."),
    ("set.shortcut", "Create desktop shortcut", "Создать ярлык на рабочем столе"),
    ("set.shortcut_done", "Shortcut created: {0}", "Ярлык создан: {0}"),
    ("menu.language", "Language", "Язык"),
    ("lang.add", "Add language…", "Добавить язык…"),
    ("lang.folder", "Open languages folder", "Открыть папку языков"),
    ("menu.help", "Help", "Справка"),
    ("help.about", "About", "О программе"),
    ("about.text", "Unofficial dark interface for AmneziaWG tunnels on Windows. The original client is not modified: tunnels are started with its own amneziawg.exe, status is read from the tunnel service pipe.", "Неофициальный тёмный интерфейс для туннелей AmneziaWG в Windows. Родной клиент не меняется: туннели поднимаются его же amneziawg.exe, состояние читается из канала службы туннеля."),
    ("about.version", "Version {0}", "Версия {0}"),
    ("about.author", "Author: {0}", "Автор: {0}"),
    ("about.started", "Development started: {0}", "Начало разработки: {0}"),
    ("about.built", "Written in Rust, interface — egui", "Написано на Rust, интерфейс — egui"),
    ("about.repo", "Source code:", "Исходный код:"),
    ("about.files", "Settings, statistics and languages: {0}", "Настройки, статистика и языки: {0}"),
    ("status.poll_error", "poll: {0}", "опрос: {0}"),
    ("log.title", "Event log", "Журнал событий"),
    ("log.empty", "No events yet: connections, disconnections, lost and restored links will appear here.", "Событий пока нет: здесь появятся подключения, отключения, потеря и восстановление связи."),
    ("col.tunnel", "Tunnel", "Туннель"),
    ("col.rx", "Down", "Скачано"),
    ("col.tx", "Up", "Отдано"),
    ("col.peak", "Peak", "Пик"),
    ("col.share", "Share", "Доля"),
    ("list.add_group", "+ group", "+ группа"),
    ("list.search_hint", "Search tunnel… (Ctrl+F)", "Поиск туннеля… (Ctrl+F)"),
    ("list.add_group_tip", "New top-level group. Tunnels and groups can be dragged with the mouse into groups and out of them.", "Новая группа верхнего уровня. Туннели и группы перетаскиваются мышью в группы и из них."),
    ("col.sort_hint", "Click — sort; drag the left border — width", "Клик — сортировка; перетащите левую границу — ширина"),
    ("grp.new_sub", "New subgroup…", "Новая подгруппа…"),
    ("grp.to_top", "Move to top level", "На верхний уровень"),
    ("grp.to_top_taken", "A top-level group with this name already exists", "На верхнем уровне уже есть группа с таким названием"),
    ("act.into_group", "Into “{0}”", "В «{0}»"),
    ("ed.unsaved", "Changes are not saved. Close again to discard them, or save (Ctrl+S).", "Изменения не сохранены. Закройте ещё раз, чтобы отказаться от них, или сохраните (Ctrl+S)."),
    ("grp.rename", "Rename…", "Переименовать…"),
    ("grp.up", "Move up", "Выше"),
    ("grp.down", "Move down", "Ниже"),
    ("grp.delete", "Delete group (subgroups and tunnels move up a level)", "Удалить группу (подгруппы и туннели перейдут уровнем выше)"),
    ("act.connect", "Connect", "Подключить"),
    ("act.disconnect", "Disconnect", "Отключить"),
    ("act.reconnect", "Reconnect", "Переподключить"),
    ("act.to_group", "Move to group", "В группу"),
    ("act.new_group", "New group…", "Новая группа…"),
    ("st.handshake", "Handshake", "Рукопожатие"),
    ("st.never", "never", "не было"),
    ("st.endpoint", "Endpoint", "Endpoint"),
    ("st.received", "Received", "Принято"),
    ("st.sent", "Sent", "Отправлено"),
    ("st.rx_rate", "Download", "Приём"),
    ("st.tx_rate", "Upload", "Передача"),
    ("st.ping_to", "Ping {0}", "Пинг до {0}"),
    ("st.no_reply", "no reply", "нет ответа"),
    ("tot.rx", "Downloaded total", "Скачано всего"),
    ("tot.tx", "Uploaded total", "Отдано всего"),
    ("tot.peak", "Peak download", "Пик приёма"),
    ("tot.time", "Connected", "В подключении"),
    ("tot.since", "Counting since", "Учёт с"),
    ("det.interface", "Interface", "Интерфейс"),
    ("det.pubkey", "Public key", "Открытый ключ"),
    ("det.port", "Port", "Порт"),
    ("det.peer", "Peer", "Пир"),
    ("det.keepalive", "Keepalive", "Keepalive"),
    ("det.off", "off", "выкл"),
    ("det.allowed", "Allowed IPs — {0}", "Allowed IPs — {0}"),
    ("gr.speed", "Speed", "Скорость"),
    ("gr.peak", "peak {0}", "пик {0}"),
    ("gr.bucket", "maximum over {0}", "максимум за {0}"),
    ("gr.ping_legend", "ping, max {0}; red — no reply", "пинг, макс {0}; красное — нет ответа"),
    ("health.off", "Disconnected", "Отключён"),
    ("health.no_service", "Service not responding: {0}", "Служба не отвечает: {0}"),
    ("health.wait", "Waiting for handshake…", "Ждём рукопожатия…"),
    ("health.no_handshake", "No handshake — server does not respond", "Нет рукопожатия — сервер не отвечает"),
    ("health.stale", "Handshake is stale ({0})", "Рукопожатие устарело ({0})"),
    ("health.ping_fail", "Handshake OK, but ping to {0} fails", "Рукопожатие есть, но пинг до {0} не проходит"),
    ("health.ok_traffic", "Connected — packets flowing", "Подключён — пакеты идут"),
    ("health.ok_idle", "Connected — no traffic", "Подключён — трафика нет"),
    ("ev.connected", "Connected", "Подключён"),
    ("ev.disconnected", "Disconnected", "Отключён"),
    ("ev.dropped", "Tunnel went down", "Туннель отключился"),
    ("ev.restored", "Link restored", "Связь восстановлена"),
    ("tray.open", "Open window", "Открыть окно"),
    ("tray.exit", "Exit", "Выход"),
    ("tray.none", "No connected tunnels", "Нет подключённых туннелей"),
    ("act.edit_native", "Edit in AmneziaWG", "Редактировать в AmneziaWG"),
    ("act.add_conf", "Add config…", "Добавить конфиг…"),
    ("act.edit_source", "Edit source", "Редактировать источник"),
    ("act.set_source", "Set source file…", "Указать файл-источник…"),
    ("src.none", "No source file is known for this tunnel: use “Add config…” or “Set source file…”", "Для этого туннеля файл-источник не известен: «Добавить конфиг…» или «Указать файл-источник…»"),
    ("src.saved_title", "Source saved", "Источник сохранён"),
    ("src.ask", "{0} was saved. Add this config to the original AmneziaWG?", "Файл {0} сохранён. Добавить этот конфиг в оригинальный AmneziaWG?"),
    ("src.import_hint", "In the AmneziaWG import dialog the file is selected — press “Open”. Don’t forget to delete the old duplicate tunnel, if there is one.", "В окне импорта AmneziaWG файл уже выделен — нажмите «Открыть». Не забудьте удалить старый дубликат туннеля, если он есть."),
    ("src.watching", "Watching for saves: {0}", "Слежу за сохранением: {0}"),
    ("btn.yes", "Yes", "Да"),
    ("btn.no", "No", "Нет"),
    ("err.native_dialog", "The AmneziaWG import dialog did not appear", "Окно импорта AmneziaWG не появилось"),
    ("err.open_file", "Cannot open {0}: {1}", "Не удалось открыть {0}: {1}"),
    ("menu.file", "File", "Файл"),
    ("file.open_conf", "Open .conf file…", "Открыть файл .conf…"),
    ("file.exit", "Exit", "Выход"),
    ("file.exit_native", "Exit with AmneziaWG", "Выход вместе с AmneziaWG"),
    ("ed.title", "Config editor", "Редактор конфига"),
    ("ed.save", "Save", "Сохранить"),
    ("ed.save_as", "Save as…", "Сохранить как…"),
    ("ed.import", "Import into AmneziaWG", "Импортировать в AmneziaWG"),
    ("ed.modified", "modified", "изменён"),
    ("ed.saved", "Saved: {0}", "Сохранено: {0}"),
    ("ed.no_interface", "No [Interface] section — AmneziaWG will not accept this file", "Нет секции [Interface] — AmneziaWG не примет этот файл"),
    ("ed.import_hint", "In the AmneziaWG import dialog choose this file (the path is in the clipboard): {0}", "В окне импорта AmneziaWG выберите этот файл (путь в буфере обмена): {0}"),
    ("err.native_window", "The AmneziaWG window did not appear", "Окно AmneziaWG не появилось"),
    ("err.native_item", "Tunnel {0} not found in the AmneziaWG window", "Туннель {0} не найден в окне AmneziaWG"),
    ("err.native_button", "Button “{0}” not found in the AmneziaWG window", "Кнопка «{0}» не найдена в окне AmneziaWG"),
    ("unit.per_sec", "{0}/s", "{0}/с"),
    ("unit.ms", "{0} ms", "{0} мс"),
    ("time.s_ago", "{0} s ago", "{0} с назад"),
    ("time.m_ago", "{0} min {1} s ago", "{0} мин {1} с назад"),
    ("time.h_ago", "{0} h {1} min ago", "{0} ч {1} мин назад"),
    ("dur.s", "{0} s", "{0} с"),
    ("dur.m", "{0} min", "{0} мин"),
    ("dur.h", "{0} h {1} min", "{0} ч {1} мин"),
    ("fmt.date_time", "%Y-%m-%d %H:%M", "%d.%m.%Y %H:%M"),
    ("fmt.log_time", "%m-%d %H:%M:%S", "%d.%m %H:%M:%S"),
];

pub struct Lang {
    pub code: String,
    strings: HashMap<String, String>,
}

impl Lang {
    fn builtin(code: &str) -> Lang {
        let ru = code == "rus";
        Lang {
            code: code.to_string(),
            strings: STRINGS.iter().map(|(k, en, r)| (k.to_string(), if ru { r } else { en }.to_string())).collect(),
        }
    }

    fn from_ini(code: &str, ini: &Ini) -> Lang {
        Lang {
            code: code.to_string(),
            strings: ini
                .section("strings")
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| (k.clone(), v.replace("\\n", "\n")))
                .collect(),
        }
    }

    /// Перевод; нет или пустой — английский; нет и его — сам ключ.
    pub fn get(&self, key: &str) -> String {
        if let Some(v) = self.strings.get(key) {
            return v.clone();
        }
        STRINGS.iter().find(|(k, ..)| *k == key).map(|(_, en, _)| en.to_string()).unwrap_or_else(|| key.to_string())
    }
}

static CURRENT: RwLock<Option<Arc<Lang>>> = RwLock::new(None);

/// Код языка — 3 латинские буквы (ISO 639-2): eng, rus, deu.
pub fn is_code(s: &str) -> bool {
    s.len() == 3 && s.chars().all(|c| c.is_ascii_lowercase())
}

/// Встроенные и файловые языки: (код, название). Файл с кодом встроенного перекрывает его.
pub fn available(dir: &Path) -> Vec<(String, String)> {
    let mut list = vec![("eng".to_string(), "English".to_string()), ("rus".to_string(), "Русский".to_string())];
    for (code, path) in files(dir) {
        let name = Ini::load(&path).get("language", "name").filter(|n| !n.is_empty()).unwrap_or(&code).to_string();
        match list.iter_mut().find(|(c, _)| *c == code) {
            Some(item) => item.1 = name,
            None => list.push((code, name)),
        }
    }
    list
}

fn files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(EXT)))
        .filter_map(|p| {
            let code = p.file_stem()?.to_string_lossy().to_lowercase();
            is_code(&code).then_some((code, p))
        })
        .collect();
    out.sort();
    out
}

/// Включить язык: файл `dir\<code>.lng`, иначе встроенный, иначе английский.
pub fn set(dir: &Path, code: &str) {
    let path = dir.join(format!("{code}.{EXT}"));
    let lang = if path.exists() {
        Lang::from_ini(code, &Ini::load(&path))
    } else if code == "rus" {
        Lang::builtin("rus")
    } else {
        Lang::builtin(DEFAULT)
    };
    *CURRENT.write().unwrap() = Some(Arc::new(lang));
}

pub fn current_code() -> String {
    CURRENT.read().unwrap().as_ref().map(|l| l.code.clone()).unwrap_or_else(|| DEFAULT.to_string())
}

pub fn tr(key: &str) -> String {
    match CURRENT.read().unwrap().as_ref() {
        Some(lang) => lang.get(key),
        None => Lang::builtin(DEFAULT).get(key),
    }
}

/// Перевод с подстановкой `{0}`, `{1}` …
pub fn trf(key: &str, args: &[&str]) -> String {
    let mut s = tr(key);
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("{{{i}}}"), a);
    }
    s
}

/// Шаблон нового языка: все ключи с английским текстом. Возвращает путь созданного файла.
pub fn create_template(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = (1..)
        .map(|i| dir.join(if i == 1 { format!("new.{EXT}") } else { format!("new{i}.{EXT}") }))
        .find(|p| !p.exists())
        .expect("свободное имя файла");
    let mut text = String::from(
        "; AmneziaWG UI Dark language file.\r\n\
         ; Translate the text after \"=\", keep the keys. Missing or empty lines fall back to English.\r\n\
         ; Save as <ISO 639-2 code>.lng in this folder: deu.lng, fra.lng, spa.lng …\r\n\
         ; {0}, {1} are placeholders for values; %Y %m %d %H %M %S are date/time parts.\r\n\r\n\
         [language]\r\nname=English\r\n\r\n[strings]\r\n",
    );
    for (k, en, _) in STRINGS {
        text.push_str(&format!("{k}={en}\r\n"));
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_unique_and_filled() {
        let mut keys: Vec<&str> = STRINGS.iter().map(|(k, ..)| *k).collect();
        keys.sort();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "повтор ключа");
        assert!(STRINGS.iter().all(|(_, en, ru)| !en.is_empty() && !ru.is_empty()));
    }

    #[test]
    fn missing_empty_or_misspelled_falls_back_to_english() {
        let ini = Ini::parse("[language]\nname=Deutsch\n[strings]\nact.connect=Verbinden\nact.disconnect=\nact.recconect=Neu\n");
        let de = Lang::from_ini("deu", &ini);
        assert_eq!(de.get("act.connect"), "Verbinden");
        assert_eq!(de.get("act.disconnect"), "Disconnect");
        assert_eq!(de.get("act.reconnect"), "Reconnect");
        assert_eq!(de.get("no.such.key"), "no.such.key");
    }

    #[test]
    fn template_roundtrip_and_listing() {
        let dir = std::env::temp_dir().join(format!("awg-ui-lang-{}", std::process::id()));
        let tpl = create_template(&dir).unwrap();
        let eng = Lang::from_ini("deu", &Ini::load(&tpl));
        assert!(STRINGS.iter().all(|(k, en, _)| eng.get(k) == *en));
        std::fs::rename(&tpl, dir.join("deu.lng")).unwrap();
        std::fs::write(dir.join("readme.lng"), "").unwrap(); // не код языка — пропускается
        let list = available(&dir);
        assert_eq!(list.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(), vec!["eng", "rus", "deu"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn russian_builtin_and_placeholders() {
        assert_eq!(Lang::builtin("rus").get("act.connect"), "Подключить");
        let s = Lang::builtin("eng").get("time.m_ago").replace("{0}", "1").replace("{1}", "36");
        assert_eq!(s, "1 min 36 s ago");
    }
}
