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
    ("dlg.dont_show", "Don't show again", "Больше не показывать"),
    ("dlg.remember_choice", "Remember my choice and do not ask again", "Запомнить выбор и больше не спрашивать"),
    ("exit.title", "Exit", "Выход"),
    ("exit.text", "Connected: {0}. The VPN runs in the core service and keeps working without the window. Disconnect the tunnels before exiting?", "Подключено: {0}. VPN работает в службе ядра и без окна программы. Отключить туннели перед выходом?"),
    ("exit.disconnect", "Disconnect and exit", "Отключить и выйти"),
    ("exit.keep", "Exit, keep the VPN", "Выйти, VPN остаётся"),
    ("exit.disconnecting", "Disconnecting tunnels…", "Отключение туннелей…"),
    ("set.show_hidden", "Show hidden dialogs again", "Снова показывать скрытые диалоги"),
    ("dlg.err_slash", "The name cannot contain “/” — it separates levels", "В названии не может быть «/» — это разделитель уровней"),
    ("dlg.err_exists", "A group with this name already exists here", "Группа с таким названием здесь уже есть"),
    ("dlg.err_empty", "The name cannot be empty", "Название не может быть пустым"),
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
    ("det.store_failed", "Could not get the tunnel details from the core: {0}. Retrying.", "Не удалось получить сведения о туннеле у ядра: {0}. Повтор через несколько секунд."),
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
    ("status.mode_engine", "Mode: built-in engine", "Режим: встроенный движок"),
    ("status.mode_overlay", "Mode: on top of AmneziaWG", "Режим: поверх AmneziaWG"),
    ("status.error_in_log", "Error — open the event log", "Ошибка — открыть журнал событий"),
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
    ("grp.err_missing", "Group “{0}” no longer exists — the tunnel was not moved", "Группы «{0}» больше нет — туннель не перенесён"),
    ("act.into_group", "Into “{0}”", "В «{0}»"),
    ("ed.unsaved_title", "Unsaved changes", "Несохранённые изменения"),
    ("ed.unsaved_text", "Save changes to {0} before closing?", "Сохранить изменения в {0} перед закрытием?"),
    ("ed.discard", "Don't save", "Не сохранять"),
    ("grp.rename", "Rename…", "Переименовать…"),
    ("grp.up", "Move up", "Выше"),
    ("grp.down", "Move down", "Ниже"),
    ("grp.delete", "Delete group (subgroups and tunnels move up a level)", "Удалить группу (подгруппы и туннели перейдут уровнем выше)"),
    ("act.connect", "Connect", "Подключить"),
    ("act.disconnect", "Disconnect", "Отключить"),
    ("act.reconnect", "Reconnect", "Переподключить"),
    ("act.retry", "Retry", "Повторить"),
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
    ("st.na", "n/a", "н/д"),
    ("eng.missing", "Engine file not found: {0}", "Не найден файл движка: {0}"),
    ("eng.busy", "disconnect the built-in engine tunnels to update the engine", "чтобы обновить движок, отключите туннели встроенного режима"),
    ("eng.bad_name", "Tunnel name \"{0}\" is not allowed: up to 32 Latin letters, digits and _ = + . -", "Имя туннеля «{0}» не подходит: до 32 латинских букв, цифр и _ = + . -"),
    ("eng.name_taken", "A Windows service named \"{0}\" already exists and does not belong to this app — rename the tunnel", "Служба Windows «{0}» уже есть и не принадлежит программе — переименуйте туннель"),
    ("eng.service_desc", "Tunnel of the built-in AmneziaWG engine (AmneziaWG UI Dark)", "Туннель встроенного движка AmneziaWG (AmneziaWG UI Dark)"),
    ("eng.start_failed", "Tunnel {0} did not start (code {1}, engine code {2})", "Туннель {0} не запустился (код {1}, код движка {2})"),
    ("eng.stop_failed", "Tunnel {0} did not stop in time", "Туннель {0} не остановился вовремя"),
    ("eng.no_engine_build", "this build has no built-in engine", "в этой сборке нет встроенного движка"),
    ("eng.untrusted", "{0} differs from the file this build was made with — it will not be given to the tunnel service", "{0} отличается от файла, с которым собрана программа, — службе туннеля он не передаётся"),
    ("core.menu", "Core (service)", "Ядро (служба)"),
    ("core.busy", "The core is busy, try again", "Ядро занято, повторите"),
    ("core.request_failed", "The core could not process the request (internal error, the core keeps running): {0}", "Ядро не смогло выполнить запрос (внутренняя ошибка, ядро продолжает работу): {0}"),
    ("core.thread_failed", "The core stopped after an internal error; Windows restarts it", "Ядро остановлено после внутренней ошибки; Windows перезапустит его"),
    ("crash.window", "The program stopped after an internal error: {0}. Details: {1}", "Программа остановлена после внутренней ошибки: {0}. Подробности: {1}"),
    ("crash.start", "The window could not start: {0}. On a virtual machine or in a remote desktop session the graphics adapter may lack DirectX 12 support. Details: {1}", "Не удалось открыть окно: {0}. На виртуальной машине или в сеансе удалённого рабочего стола у графического адаптера может не быть поддержки DirectX 12. Подробности: {1}"),
    ("core.pipe_taken", "Another program has taken the core's pipe name; waiting for it to be released", "Имя канала ядра заняла другая программа; жду, пока оно освободится"),
    ("core.pipe_recovered", "The core pipe works again", "Канал ядра снова работает"),
    ("core.accept_repeats", "Repeated {0} times since the last entry: {1}", "Повторилось {0} раз с прошлой записи: {1}"),
    ("core.no_scripts", "Configs with PreUp/PostUp/PreDown/PostDown commands are not accepted: the tunnel service would run them as SYSTEM", "Конфиги с командами PreUp/PostUp/PreDown/PostDown не принимаются: служба туннеля выполнила бы их от имени системы"),
    ("core.other_session", "Actions in the AmneziaWG window work only for the account signed in to this Windows session", "Действия в окне AmneziaWG доступны только учётной записи, которая вошла в этот сеанс Windows"),
    ("core.unavailable", "The core does not answer: {0}", "Ядро не отвечает: {0}"),
    ("core.foreign", "The core pipe belongs to another program ({0}) — not connecting", "Канал ядра занят чужой программой ({0}) — не подключаюсь"),
    ("core.helper_timeout", "The AmneziaWG window did not respond in time", "Окно AmneziaWG не ответило вовремя"),
    ("core.no_owner", "The core has no owner — install it again from the app", "У ядра нет владельца — установите его заново из программы"),
    ("stats.pruned", "Statistics of tunnel “{0}” removed: the tunnel has not existed for {1} days", "Статистика туннеля «{0}» удалена: туннеля нет уже {1} дней"),
    ("core.bad_owner", "The owner SID in the core config is invalid; the pipe is open to administrators and SYSTEM only: {0}", "SID владельца в настройках ядра некорректен; канал открыт только администраторам и SYSTEM: {0}"),
    ("core.only_engine", "Only in the built-in engine mode", "Только в режиме встроенного движка"),
    ("core.only_overlay", "Only in the mode on top of AmneziaWG", "Только в режиме поверх AmneziaWG"),
    ("core.mode_switched", "Working mode: {0}", "Режим работы: {0}"),
    ("core.retry_failed", "Reconnecting {0}: attempt {1}, error: {2}", "Переподключение {0}: попытка {1}, ошибка: {2}"),
    ("core.retry_slow", "Could not connect {0} within 10 minutes: {1}. Next — one attempt every 10 minutes", "Не удалось подключить {0} за 10 минут: {1}. Дальше — попытка раз в 10 минут"),
    ("core.retry_connected", "{0} connected after {1} attempts", "{0} подключён после {1} попыток"),
    ("core.retry_died", "the tunnel stopped right after start", "туннель остановился сразу после запуска"),
    ("core.retry_restarted", "Reconnecting {0}: started over by the user", "Переподключение {0}: заново по команде пользователя"),
    ("core.retry_not_desired", "{0} is not among the tunnels to keep connected", "{0} нет среди туннелей, которые нужно держать подключёнными"),
    ("core.retry_outside", "{0} was disconnected outside this program (its service is gone): it will not be reconnected", "{0} отключён вне программы (его службы нет): переподключаться не будет"),
    ("core.netwatch_failed", "Network changes are not tracked, reconnection goes by schedule only: {0}", "Смена сети не отслеживается, переподключение — только по расписанию: {0}"),
    ("core.failure_actions_left", "Windows may still restart this tunnel service on failure: {0}", "Windows может по-прежнему перезапускать службу туннеля при сбое: {0}"),
    ("core.desired_adopted", "Tunnels to bring back after a restart, taken from the running ones: {0}", "Туннели для восстановления после перезапуска взяты из работающих: {0}"),
    ("core.desired_unsaved", "The list of tunnels to bring back after a restart was not saved: {0}", "Список туннелей для восстановления после перезапуска не сохранён: {0}"),
    ("core.replaced", "Disconnected: cannot run together with {0} (same address or both route all traffic)", "Отключён: не может работать вместе с {0} (общий адрес или оба на весь трафик)"),
    ("core.uac_declined", "Administrator rights were not granted", "Права администратора не получены"),
    ("core.restore_needs_admin", "Rollback needs administrator rights (UAC confirmation)", "Возврат версии требует прав администратора (подтверждение UAC)"),
    ("core.restore_refused", "Rollback request without administrator rights refused (account {0})", "Отклонён запрос возврата версии без прав администратора (учётная запись {0})"),
    ("core.footprint_unread", "The tunnel config cannot be read, so its conflicts with other tunnels are not checked: {0}", "Конфиг туннеля не читается, конфликты с другими туннелями не проверены: {0}"),
    ("core.carry_failed", "Could not carry statistics or the event log over from the program folder: {0}", "Не удалось перенести статистику или журнал событий из папки программы: {0}"),
    ("eng.rejected", "Engine file {0} was not installed, so the built-in engine mode is unavailable: its SHA-256 is {1}, expected {2}", "Файл движка {0} не установлен, встроенный режим недоступен: его SHA-256 — {1}, ожидалась {2}"),
    ("ini.unreadable","Settings file {0} cannot be read ({1}); defaults are used. The original was kept as {2}", "Файл настроек {0} не читается ({1}); взяты значения по умолчанию. Исходный файл сохранён как {2}"),
    ("ini.unreadable_kept", "Settings file {0} cannot be read ({1}); defaults are used. The original could not be moved aside ({2}) and may be overwritten on the next save", "Файл настроек {0} не читается ({1}); взяты значения по умолчанию. Отодвинуть исходный файл не удалось ({2}), следующая запись может его затереть"),
    ("core.uac_hint", "One Windows prompt for administrator rights; after that the app works without them", "Один запрос Windows на права администратора; дальше программа работает без них"),
    ("core.stop_failed", "The core service did not stop in time", "Служба ядра не остановилась вовремя"),
    ("core.start_failed", "The core service did not start (code {0})", "Служба ядра не запустилась (код {0})"),
    ("core.service_desc", "AmneziaWG UI Dark core: tunnels, statistics and event log; keeps working when the window is closed", "Ядро AmneziaWG UI Dark: туннели, статистика и журнал; работает и при закрытом окне"),
    ("core.missing", "The core is not installed: the app cannot manage tunnels yet.", "Ядро не установлено: управлять туннелями программа пока не может."),
    ("core.install", "Install the core", "Установить ядро"),
    ("core.outdated", "The core is version {0}, the window is {1}.", "Ядро версии {0}, окно — {1}."),
    ("core.update", "Update the core", "Обновить ядро"),
    ("core.newer", "The core ({0}) is newer than the window ({1}); the window will update itself.", "Ядро ({0}) новее окна ({1}) — окно обновится само."),
    ("core.retry", "Retry", "Повторить"),
    ("core.down", "The core service is installed but does not answer: {0}", "Служба ядра установлена, но не отвечает: {0}"),
    ("core.reinstall", "Reinstall the core", "Переустановить ядро"),
    ("core.installing", "Installing the core…", "Устанавливаю ядро…"),
    ("core.installed", "The core is installed and running", "Ядро установлено и запущено"),
    ("core.uninstalled", "The core service is removed; tunnels and data are kept", "Служба ядра удалена; туннели и данные сохранены"),
    ("core.setup_failed", "Core setup failed (code {0})", "Установка ядра не удалась (код {0})"),
    ("core.uninstall_title", "Remove the core…", "Удалить ядро…"),
    ("core.uninstall_text", "Remove the core service? Without it the window cannot manage tunnels. Tunnels of the built-in engine, statistics and the event log are kept.", "Удалить службу ядра? Без неё окно не сможет управлять туннелями. Туннели встроенного движка, статистика и журнал сохраняются."),
    ("set.mode", "Working mode", "Режим работы"),
    ("mode.overlay", "On top of AmneziaWG (default)", "Поверх AmneziaWG (по умолчанию)"),
    ("mode.engine", "Built-in engine", "Встроенный движок"),
    ("mode.overlay_help", "The official AmneziaWG client runs alongside and keeps its own updates; this app shows and controls its tunnels. Import and editing go through the AmneziaWG window. Tunnels of the built-in engine stay in their folder and come back when you switch again.", "Официальный клиент AmneziaWG работает рядом и обновляется сам; программа показывает его туннели и управляет ими. Импорт и правка — через окно AmneziaWG. Туннели встроенного движка остаются в своей папке и вернутся при обратном переключении."),
    ("mode.engine_help", "The app works on its own, without an installed AmneziaWG: the tunnel engine (tunnel.dll, the same code as in the official client 3.1) and the wintun driver ship with it. Tunnels live in C:\\Program Files\\AmneziaWG UI Dark\\tunnels, encrypted by Windows (DPAPI) — a copied file cannot be read on another computer. Add them with File → Import, or take all of them from AmneziaWG in one click. For a new PC or a reinstalled Windows use File → Backup: a zip protected by your password. Switching does not restart the window, and connected tunnels come back by themselves after a reboot.", "Программа работает сама, без установленного AmneziaWG: движок туннелей (tunnel.dll — тот же код, что в официальном клиенте 3.1) и драйвер wintun идут вместе с ней. Туннели хранятся в C:\\Program Files\\AmneziaWG UI Dark\\tunnels, зашифрованные средствами Windows (DPAPI) — скопированный файл на другом компьютере не прочитать. Добавить их — «Файл → Импорт» или одной кнопкой забрать всё из AmneziaWG. Для нового ПК или после переустановки Windows — «Файл → Резервная копия»: zip под вашим паролем. Окно при переключении не перезапускается, а подключённые туннели сами поднимаются после перезагрузки."),
    ("mode.will_disconnect", "These tunnels will be disconnected: {0}", "Будут отключены туннели: {0}"),
    ("mode.missing", "The built-in engine is not available: {0}. Download the full zip from the Releases page and keep its files next to the program.", "Встроенный движок недоступен: {0}. Скачайте полный zip со страницы Releases и держите его файлы рядом с программой."),
    ("mode.switch", "Switch", "Переключить"),
    ("mode.switching", "Switching the mode…", "Переключаю режим…"),
    ("eng.new", "New tunnel…", "Новый туннель…"),
    ("eng.import", "Import tunnels (.conf, .zip)…", "Импорт туннелей (.conf, .zip)…"),
    ("eng.take_native", "Take all tunnels from AmneziaWG", "Забрать все туннели из AmneziaWG"),
    ("eng.taking", "Taking tunnels from AmneziaWG…", "Забираю туннели из AmneziaWG…"),
    ("eng.take_native_hint", "Copies the tunnels of the installed AmneziaWG into the app's own storage; tunnels that are already here are not changed", "Копирует туннели установленного AmneziaWG в собственное хранилище программы; туннели, которые здесь уже есть, не меняются"),
    ("eng.empty_title", "No tunnels in the built-in engine yet", "Во встроенном движке пока нет туннелей"),
    ("eng.empty_text", "The built-in engine keeps its own copy of tunnels, separate from AmneziaWG. Take them from AmneziaWG, import .conf / .zip files or restore a backup.", "У встроенного движка своя копия туннелей, отдельная от AmneziaWG. Заберите их из AmneziaWG, импортируйте файлы .conf / .zip или восстановите резервную копию."),
    ("eng.backup", "Backup…", "Резервная копия…"),
    ("eng.restore", "Restore from backup…", "Восстановить из резервной копии…"),
    ("eng.imported", "Imported: {0}", "Импортировано: {0}"),
    ("eng.import_existing", "already there, not changed: {0}", "уже есть, не тронуты: {0}"),
    ("eng.import_bad_name", "name not allowed: {0}", "имя не подходит: {0}"),
    ("eng.backup_title", "Backup of tunnels", "Резервная копия туннелей"),
    ("eng.backup_text", "All tunnels go into one zip encrypted with AES-256. Keep the password: without it the copy cannot be opened. The zip also opens in 7-Zip and WinRAR.", "Все туннели — в один zip, зашифрованный AES-256. Сохраните пароль: без него копию не открыть. Архив открывается и в 7-Zip, WinRAR."),
    ("eng.backup_save", "Save…", "Сохранить…"),
    ("eng.backup_done", "Backup: {0} tunnels → {1}", "Резервная копия: {0} туннелей → {1}"),
    ("eng.restore_title", "Password of the backup", "Пароль резервной копии"),
    ("eng.restore_text", "The archive is protected by a password.", "Архив защищён паролем."),
    ("eng.password", "Password", "Пароль"),
    ("eng.password_repeat", "Repeat the password", "Повторите пароль"),
    ("eng.password_short", "At least 12 characters — a phrase of several words is best", "Не короче 12 символов — лучше фраза из нескольких слов"),
    ("eng.password_mismatch", "The passwords differ", "Пароли не совпадают"),
    ("eng.wrong_password", "Wrong password", "Неверный пароль"),
    ("eng.new_title", "New tunnel", "Новый туннель"),
    ("eng.rename_title", "Rename tunnel {0}", "Переименовать туннель {0}"),
    ("eng.edit", "Edit…", "Изменить…"),
    ("eng.rename", "Rename…", "Переименовать…"),
    ("eng.rename_hint", "Disconnect the tunnel first", "Сначала отключите туннель"),
    ("eng.rename_running", "Disconnect {0} before renaming", "Перед переименованием отключите {0}"),
    ("eng.saved", "Saved {0}; a connected tunnel takes the changes after Reconnect", "Сохранено {0}; подключённый туннель возьмёт изменения после переподключения"),
    ("eng.delete_text", "Delete tunnel {0} from the app? Its encrypted file is removed; make a copy first if you need it.", "Удалить туннель {0} из программы? Его зашифрованный файл будет удалён; если нужна копия — сохраните её."),
    ("eng.delete_running", "The tunnel is connected — it will be disconnected.", "Туннель подключён — он будет отключён."),
    ("eng.deleting", "Deleting {0}…", "Удаление {0}…"),
    ("eng.deleted", "Tunnel {0} deleted from the app", "Туннель {0} удалён из программы"),
    ("eng.deleted_copy", "Tunnel {0} deleted from the app; copy saved: {1}", "Туннель {0} удалён из программы; копия сохранена: {1}"),
    ("ed.import_engine", "Import into the app", "Импорт в программу"),
    ("det.from_store", "Not connected — config from the app's storage", "Не подключён — конфиг из хранилища программы"),
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
    ("health.core_lost", "Unknown: no connection to the core", "Неизвестно: нет связи с ядром"),
    ("health.no_service", "Service not responding: {0}", "Служба не отвечает: {0}"),
    ("health.wait", "Waiting for handshake…", "Ждём рукопожатия…"),
    ("health.no_handshake", "No handshake — server does not respond", "Нет рукопожатия — сервер не отвечает"),
    ("health.stale", "Handshake is stale ({0})", "Рукопожатие устарело ({0})"),
    ("health.ping_fail", "Handshake OK, but ping to {0} fails", "Рукопожатие есть, но пинг до {0} не проходит"),
    ("health.ok_traffic", "Connected — packets flowing", "Подключён — пакеты идут"),
    ("health.ok_idle", "Connected — no traffic", "Подключён — трафика нет"),
    ("health.retry_wait", "Reconnecting… next attempt in {0} s", "Переподключение… попытка через {0} с"),
    ("health.retrying", "Reconnecting… attempt {0}, next in {1} s", "Переподключение… попытка {0}, следующая через {1} с"),
    ("health.retry_slow", "Could not connect: {0}. Next attempt in {1} min", "Не удалось подключить: {0}. Следующая попытка через {1} мин"),
    ("ev.connected", "Connected", "Подключён"),
    ("ev.disconnected", "Disconnected", "Отключён"),
    ("ev.dropped", "Tunnel went down", "Туннель отключился"),
    ("ev.restored", "Link restored", "Связь восстановлена"),
    ("ev.core_lost", "No connection to the core: {0}", "Нет связи с ядром: {0}"),
    ("ev.core_back", "Connection to the core restored", "Связь с ядром восстановлена"),
    ("ev.log_write_failed", "Cannot write the event log {0}: {1}", "Не удаётся писать журнал событий {0}: {1}"),
    ("ev.log_read_failed", "Cannot read the event log {0}, its history is not shown: {1}", "Не удаётся прочитать журнал событий {0}, история не показана: {1}"),
    ("ev.log_rotate_failed", "Cannot rotate the event log {0}: {1}", "Не удаётся сменить файл журнала событий {0}: {1}"),
    ("tray.open", "Open window", "Открыть окно"),
    ("tray.exit", "Exit", "Выход"),
    ("tray.none", "No connected tunnels", "Нет подключённых туннелей"),
    ("tray.core_lost", "No connection to the core — tunnel state unknown", "Нет связи с ядром — состояние туннелей неизвестно"),
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
    ("upd.menu", "Check for updates…", "Проверить обновления…"),
    ("upd.menu_new", "Check for updates… (new available)", "Проверить обновления… (есть новое)"),
    ("upd.title", "Updates and rollbacks", "Обновления и откаты"),
    ("upd.notice_title", "Updates", "Обновления"),
    ("upd.notice_text", "Updates available: {0}", "Доступны обновления: {0}"),
    ("upd.notice_open", "Open", "Открыть"),
    ("upd.notice_later", "Later", "Позже"),
    ("upd.col_component", "Component", "Компонент"),
    ("upd.col_installed", "Installed", "Установлено"),
    ("upd.col_available", "Available", "Доступно"),
    ("upd.col_status", "Status", "Состояние"),
    ("upd.c_native", "AmneziaWG (original client)", "AmneziaWG (оригинальный клиент)"),
    ("upd.c_engine", "Mode 2 engine (tunnel.dll, wintun.dll)", "Движок режима 2 (tunnel.dll, wintun.dll)"),
    ("upd.c_app", "AmneziaWG UI Dark program", "Программа AmneziaWG UI Dark"),
    ("upd.s_native", "AmneziaWG", "AmneziaWG"),
    ("upd.s_engine", "Mode 2 engine", "Движок режима 2"),
    ("upd.s_app", "AmneziaWG UI Dark", "AmneziaWG UI Dark"),
    ("upd.not_installed", "not installed", "не установлен"),
    ("upd.unknown", "unknown", "неизвестно"),
    ("upd.installed_on", "Installed on this computer: {0}", "Установлено на этом компьютере: {0}"),
    ("upd.st_ok", "Up to date", "Актуально"),
    ("upd.st_update", "Update available", "Есть обновление"),
    ("upd.st_missing", "Not installed", "Не установлен"),
    ("upd.st_unknown", "Not checked", "Не проверено"),
    ("upd.st_ok_engine", "Built from amneziawg-windows {0} — the newest Amnezia release", "Собрано из amneziawg-windows {0} — самый новый релиз Amnezia"),
    ("upd.st_upstream_newer", "Amnezia released {0} — comes with an app update", "Amnezia выпустила {0} — придёт с обновлением программы"),
    ("upd.st_upstream_unchecked", "Could not check", "Не удалось проверить"),
    ("upd.st_upstream_unchecked_tip", "Could not check the newest Amnezia release — details are in the event log", "Не удалось проверить новейший релиз Amnezia — подробности в журнале событий"),
    ("upd.st_manual", "Manual only (no manifest)", "Только вручную (релиз без манифеста)"),
    ("upd.notes", "What's new…", "Что нового…"),
    ("upd.notes_title", "What's new — {0} {1}", "Что нового — {0} {1}"),
    ("upd.check", "Check", "Проверить"),
    ("upd.apply_selected", "Update selected", "Обновить выбранное"),
    ("upd.apply_all", "Update all", "Обновить всё"),
    ("upd.checked_at", "Last check: {0}", "Последняя проверка: {0}"),
    ("upd.never_checked", "Not checked yet", "Проверки ещё не было"),
    ("upd.waiting", "Waiting for the core…", "Ожидание ответа ядра…"),
    ("upd.confirm_title", "Install updates", "Установка обновлений"),
    ("upd.update", "Update", "Обновить"),
    ("upd.warn_native", "Tunnels of the “on top of AmneziaWG” mode will be disconnected during installation", "Туннели режима «поверх AmneziaWG» на время установки отключатся"),
    ("upd.warn_app", "The program will restart", "Программа перезапустится"),
    ("upd.warn_backup", "A backup will be made before updating", "Перед обновлением будет сделана резервная копия"),
    ("upd.history", "History and rollback", "История и откат"),
    ("upd.history_empty", "History is empty", "История пуста"),
    ("upd.h_date", "Date", "Дата"),
    ("upd.h_action", "Action", "Действие"),
    ("upd.h_version", "Version", "Версия"),
    ("upd.h_backup", "Backup", "Копия"),
    ("upd.h_result", "Result", "Результат"),
    ("upd.a_backup", "Backup", "Резервная копия"),
    ("upd.a_update", "Update", "Обновление"),
    ("upd.a_restore", "Rollback", "Возврат"),
    ("upd.result_ok", "OK", "ОК"),
    ("upd.result_err", "Error", "Ошибка"),
    ("upd.restore", "Restore", "Вернуть"),
    ("upd.restore_to", "Restore {0}", "Вернуть {0}"),
    ("upd.no_backup", "There is no backup of the version this entry would restore (it was removed or never made)", "Резервной копии версии, к которой вернула бы эта строка, нет (удалена или не была сделана)"),
    ("upd.restore_installed", "Version {0} is already installed", "Версия {0} уже установлена"),
    ("upd.restore_title", "Roll back", "Возврат версии"),
    ("upd.warn_uac", "Windows will ask for administrator rights", "Windows запросит права администратора"),
    ("upd.restore_cancelled", "Rollback cancelled: administrator rights were not granted", "Возврат отменён: права администратора не предоставлены"),
    ("upd.restore_failed", "Rollback helper exited with code {0}", "Помощник возврата завершился с кодом {0}"),
    ("upd.restore_text","Roll back {0} to version {1}? The current version will be saved to a backup — you will be able to return to it as well.", "Вернуть {0} к версии {1}? Текущая версия будет сохранена в резервную копию — к ней тоже можно будет вернуться."),
    // Обновление и возврат наших компонентов (движок режима 2, сборка программы).
    ("updo.no_manifest", "Release {0} has no signed update manifest — it can only be installed manually", "В релизе {0} нет подписанного манифеста обновления — его можно поставить только вручную"),
    ("updo.manifest_changed", "The manifest of release {0} changed since the check — check for updates again", "Манифест релиза {0} изменился после проверки — проверьте обновления ещё раз"),
    ("updo.no_asset", "The release has no file {0}", "В релизе нет файла {0}"),
    ("updo.downloading", "Downloading {0}… {1} %", "Загрузка {0}… {1} %"),
    ("updo.installing", "Installing {0}…", "Установка {0}…"),
    ("updo.engine", "engine", "движка"),
    ("updo.no_engine", "The engine is not installed", "Движок не установлен"),
    ("updo.no_version", "The backup {0} has no version.txt", "В резервной копии {0} нет version.txt"),
    ("updo.self_update_failed", "Could not update the window to version {0}: {1}", "Не удалось обновить окно до версии {0}: {1}"),
    ("updo.window_no_manifest", "the new build has no valid signed manifest next to it", "рядом с новой сборкой нет верного подписанного манифеста"),
    ("updo.window_version", "the manifest is for version {0}, the core is {1}", "манифест для версии {0}, а ядро — {1}"),
    ("updo.window_mismatch", "{0} does not match the signed manifest", "{0} не совпадает с подписанным манифестом"),
    ("updo.rollback_kept", "Could not put {0} back ({1}); the previous file is kept as {2}", "Не удалось вернуть {0} на место ({1}); прежний файл сохранён как {2}"),
    ("updo.rollback_lost", "Could not put {0} back ({1}) nor keep the previous file {2} ({3}); it will be deleted when the core starts", "Не удалось ни вернуть {0} на место ({1}), ни сохранить прежний файл {2} ({3}); он будет удалён при старте ядра"),
    ("updo.restart_bad_arg", "{0} is not a previous core build — restarting without a way back", "{0} — не прежняя сборка ядра: перезапуск без возможности возврата"),
    ("updo.restart_failed", "The core of version {0} did not start after the update — returning the previous build", "Ядро версии {0} не запустилось после обновления — возвращается прежняя сборка"),
    ("updo.restart_no_way_back", "The core did not start after the update, and there is no previous build to return", "Ядро не запустилось после обновления, а прежней сборки для возврата нет"),
    ("updo.restart_back_failed", "Could not return the previous build of the core: {0}", "Не удалось вернуть прежнюю сборку ядра: {0}"),
    ("updo.restart_back", "The previous build of the core is back in place", "Прежняя сборка ядра возвращена на место"),
    ("updo.restart_back_not_started", "The previous build of the core is back but did not start either", "Прежняя сборка ядра возвращена, но тоже не запустилась"),
    ("updo.restart_pipe_taken", "The new core cannot open its pipe: another program has taken the name. The update is not at fault and is not rolled back; waiting up to 10 minutes for the name to be released", "Новое ядро не может открыть свой канал: имя заняла другая программа. Обновление тут ни при чём и не откатывается; жду до 10 минут, пока имя освободится"),
    ("updo.restart_pipe_gave_up", "The core is not running: its pipe name is still taken by another program. Close that program or restart the computer — the core will start with the updated version", "Ядро не запущено: имя его канала всё ещё занято другой программой. Закройте её или перезагрузите компьютер — ядро запустится уже обновлённым"),
    ("updo.restart_not_started", "The core of version {0} did not start after the update", "Ядро версии {0} не запустилось после обновления"),
    ("updo.start_refused", "The service control manager refused to start the core: {0}", "Диспетчер служб отказал в запуске ядра: {0}"),
    ("updo.restart_stop_failed", "The core service did not stop before the previous build was returned", "Служба ядра не остановилась перед возвратом прежней сборки"),
    ("updo.rollback_missing", "Could not put {0} back: the previous file {1} is missing", "Не удалось вернуть {0}: прежнего файла {1} нет"),
    ("updo.rollback_kept_file", "The previous {0} was not put back and is kept as {1}", "Прежний {0} не возвращён на место и сохранён как {1}"),
    ("updo.rollback_keep_failed", "The previous {0} was not put back nor kept: it stays as {1} ({2}) and will be deleted when the core starts", "Прежний {0} не возвращён на место и не сохранён: он остался как {1} ({2}) и будет удалён при старте ядра"),
    ("updm.native", "AmneziaWG", "AmneziaWG"),
    ("updm.engine", "Mode 2 engine", "Движок режима 2"),
    ("updm.app", "AmneziaWG UI Dark", "AmneziaWG UI Dark"),
    ("updm.busy", "Another update task is already running", "Уже идёт другая работа с обновлениями"),
    ("updm.upstream_failed", "Could not check the newest amneziawg-windows release: {0}", "Не удалось проверить новейший релиз amneziawg-windows: {0}"),
    ("updm.release_date_failed", "Could not get the release date of {0} {1}: {2}", "Не удалось узнать дату выхода {0} {1}: {2}"),
    ("updm.failed", "The update task stopped on an internal error: {0}", "Работа с обновлениями прервана внутренней ошибкой: {0}"),
    ("updm.history_repaired", "History entry #{0} was inconsistent and has been repaired: {1}", "Строка истории №{0} была противоречивой и исправлена: {1}"),
    ("updm.fix_ok_with_error", "marked successful but has an error text, now marked failed", "помечена успешной, но с текстом ошибки, теперь помечена неудачной"),
    ("updm.fix_backup_with_to", "a backup row had a target version, removed", "у строки копии была целевая версия, убрана"),
    ("updm.fix_backup_on_non_backup", "an update or restore row referred to a backup, reference removed", "строка обновления или возврата ссылалась на копию, ссылка убрана"),
    ("updm.checking", "Checking for updates…", "Проверка обновлений…"),
    ("updm.backing_up", "Backing up {0} {1}…", "Резервная копия {0} {1}…"),
    ("updm.downloading", "Downloading {0} {1}… {2}", "Загрузка {0} {1}… {2}"),
    ("updm.installing", "Installing {0} {1}…", "Установка {0} {1}…"),
    ("updm.restoring", "Restoring {0} {1}…", "Возврат {0} {1}…"),
    ("updm.available", "Update available: {0} {1}", "Доступно обновление: {0} {1}"),
    ("updm.updated", "{0} updated: {1} → {2}", "{0} обновлён: {1} → {2}"),
    ("updm.update_failed", "{0}: update failed: {1}", "{0}: обновление не удалось: {1}"),
    ("updm.backup_failed", "{0}: backup failed, nothing was changed: {1}", "{0}: резервная копия не удалась, ничего не изменено: {1}"),
    ("updm.restored", "{0} restored to {1}", "{0} возвращён к {1}"),
    ("updm.restore_failed", "{0}: restore failed: {1}", "{0}: возврат не удался: {1}"),
    ("updm.no_backup", "This history entry has no backup", "У этой строки истории нет резервной копии"),
    ("updm.already_installed", "The version this entry would restore is already installed", "Версия, к которой вернула бы эта строка, уже установлена"),
    ("updm.bad_backup", "Backup {0} is damaged: {1}", "Резервная копия {0} повреждена: {1}"),
    ("updm.no_asset", "Release {0} has no file {1}", "В релизе {0} нет файла {1}"),
    ("updm.no_digest", "The release does not give a SHA-256 for {0}", "Релиз не даёт SHA-256 для {0}"),
    ("updm.wrong_upgrade_code", "The installer is not AmneziaWG amd64 (UpgradeCode {0})", "Установщик — не AmneziaWG amd64 (UpgradeCode {0})"),
    ("updm.wrong_msi_version", "The installer has version {0}, the backup is for {1}", "У установщика версия {0}, а копия — для {1}"),
    ("updm.rolled_back", "installing the old version failed: {0}. The previous version was installed back with its tunnels", "установка старой версии не удалась: {0}. Прежняя версия установлена обратно вместе с туннелями"),
    ("updm.rollback_failed", "installing the old version failed: {0}. Installing the previous version back also failed: {1}. AmneziaWG may be missing — install it manually; the backup is in {2}", "установка старой версии не удалась: {0}. Вернуть прежнюю версию тоже не удалось: {1}. AmneziaWG может быть не установлен — поставьте его вручную; резервная копия — в {2}"),
    ("updm.interrupted", "interrupted: the core stopped before the restore finished — check that AmneziaWG is installed", "прервано: ядро остановилось до конца возврата — проверьте, что AmneziaWG установлен"),
    ("updm.amend_unknown", "update history has no entry {0}; the error is not recorded there: {1}", "в истории обновлений нет строки {0}; ошибка в ней не записана: {1}"),
    ("updm.reconnect_failed", "Could not connect again after the AmneziaWG restore: {0}", "Не удалось подключить снова после возврата AmneziaWG: {0}"),
    ("updm.version_changed", "The source released a different version ({0}) — check for updates again", "Источник выпустил другую версию ({0}) — проверьте обновления снова"),
    ("updm.msi_unexpected_version", "The downloaded installer has version {0}, expected {1}", "У загруженного установщика версия {0}, ожидалась {1}"),
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
         ; {0}, {1} are placeholders for values. Dates and times are not translated: they are always YYYY.MM.DD HH:MM.\r\n\r\n\
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
