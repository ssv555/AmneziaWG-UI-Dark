<p align="right"><a href="development.md">English</a> | <b>Русский</b></p>

# Разработка

Как собрать, проверить и выпустить проект. Правила участия (issues, pull request, стиль кода, сообщения коммитов,
переводы) - в [CONTRIBUTING.md](../CONTRIBUTING.md), здесь они не повторяются.

## Инструменты

1. Windows 10/11 x64.
2. Rust 1.95 или новее (минимум eframe 0.36), канал stable, цель MSVC (`x86_64-pc-windows-msvc`),
   edition 2021. В CI ставится последний stable.
3. Visual Studio Build Tools и Windows SDK: `build.rs` вызывает `rc.exe`, чтобы вшить значок и сведения о версии.
4. `.cargo/config.toml` линкует среду выполнения C статически (`+crt-static`), поэтому exe не требует VC++
   Redistributable. Переменная `RUSTFLAGS` заменяет эти флаги; для релизной сборки её не задавайте.
5. Для сборки движка (ниже) нужны ещё PowerShell и `git`; Go, llvm-mingw и wintun скрипт скачивает сам.

## Сборка

```powershell
cargo build --release
```

Результат: `target\release\awg-ui.exe`. Релизный профиль: `opt-level = 3`, thin LTO, без символов. Сборка должна идти
без предупреждений (CI падает на любой строке, начинающейся с `warning`).

`awg-ui.exe --demo` запускает окно без AmneziaWG и без прав администратора, на выдуманных туннелях.
`--demo --snapshot out.png` сохраняет снимок окна; все параметры командной строки - в [README](../README.ru.md).

## Сборка движка

Встроенный движок (режим 2) - это `tunnel.dll` (из зафиксированного тега amneziawg-windows) и `wintun.dll`:

```powershell
.\engine\build.ps1     # до cargo build
```

1. `engine/build.ps1` скачивает Go, llvm-mingw и wintun в `engine\.deps` с проверкой каждого архива по SHA-256,
   клонирует зафиксированный тег amneziawg-windows в `engine\.src`, сверяет хэш коммита и собирает DLL в
   `engine\out`. Все закрепления (`$EngineTag`, `$EngineCommit`, адреса и хэши загрузок) - в начале скрипта.
2. `build.rs` вшивает в `awg-ui.exe` SHA-256 обеих DLL из `engine\out`. Приложение копирует DLL для службы туннеля,
   только если хэши совпали. Без `engine\out` программа собирается, но без встроенного движка.
3. Чтобы запустить режим 2 из локальной сборки, положите `tunnel.dll` и `wintun.dll` рядом с `awg-ui.exe`.
4. `engine/check-upstream.ps1` сравнивает закрепления с новейшими версиями и печатает JSON-отчёт. Код выхода:
   0 - всё свежее, 10 - что-то устарело, 2 - сама проверка не удалась. Как обновить закрепление - раздел
   *Upstream updates* в [CONTRIBUTING.md](../CONTRIBUTING.md).

## Тесты

```powershell
cargo test --release
```

CI запускает `cargo test --release --locked`. Тесты - модульные, рядом с кодом (модули `#[cfg(test)]` в `src/`);
данные - в `tests/fixtures/`. Они работают на моках и временных папках и не трогают службы, чужие учётные записи и
настоящие туннели.

Тесты с `#[ignore]` по умолчанию не запускаются:

| Тест | Что нужно |
|------|-----------|
| `live_latest_native` (`src/update/feed.rs`) | Сеть: у последнего релиза оригинального клиента AmneziaWG есть MSI для amd64 с SHA-256 |
| `live_msi_identity` (`src/update/native.rs`) | Сеть: скачивает настоящий MSI и проверяет его подлинность |
| `live_installed` (`src/update/native.rs`) | Установленный на этой машине клиент AmneziaWG |
| `live_download_small`, `live_max_enforced` (`src/update/net.rs`) | Сеть: ход загрузки и предел размера |
| `fake_agent_process` (`src/daemon/server.rs`) | Не тест: точка входа поддельного процесса-помощника, который тесты изоляции ядра запускают сами |

Живые тесты запускайте осознанно, с выводом:

```powershell
cargo test --release -- --ignored --nocapture live_
```

Не запускайте `--ignored` вслепую: `fake_agent_process` без своего аргумента ничего не делает, но тесты `live_*`
ходят в сеть и смотрят на состояние настоящей машины. Они ничего не устанавливают, не удаляют и не перезапускают.

Логика, которой нужна живая система (установка службы, обновление, MSI, туннели), проверяется вручную и описывается
в pull request.

### Журналы нестабильных тестов

Тесты изоляции ядра (`isolation_*` в `src/daemon/server.rs`) отмечают время каждой фазы каждого Switch: подключение
клиента, приём на канале, запуск потока запроса, блокировки ядра, запись настроек, вызовы движка
(`src/daemon/phase_trace.rs`). Если Switch не прошёл или занял 1 с и больше, тест пишет запись со сведениями о машине
(число процессоров, их загрузка, число потоков, возраст процесса и exe) в `target/test-logs/<тест>-<unix-время>.log`
и указывает путь в сообщении о падении. Только для разработчиков: запись есть лишь в тестовой сборке, в exe выпуска её нет.

## Рабочие процессы CI

Все в `.github/workflows/`:

| Процесс | Запуск | Что делает |
|---------|--------|------------|
| `ci.yml` | push в `main`, pull request | На Windows: `cargo test --release --locked`, затем релизная сборка, падающая на любом предупреждении |
| `release.yml` | тег `v*` | Собирает и публикует релиз (см. ниже) |
| `upstream.yml` | ежедневно, по понедельникам, вручную | Ежедневно: запускает `engine/check-upstream.ps1` и открывает по одному issue на устаревшее закрепление (`Upstream update: <что> <версия>`). По понедельникам: `cargo audit` по `Cargo.lock`, падает на уязвимостях |

`.github/dependabot.yml` раз в неделю предлагает обновления зависимостей Cargo и GitHub Actions.

## Выпуск релиза

1. Поднимите `version` в `Cargo.toml` (и один раз соберите, чтобы подтянулся `Cargo.lock`).
2. Добавьте строку сверху таблицы в `CHANGELOG.md` и `CHANGELOG.ru.md`: версия, дата, изменения через `<br>`.
   Текст строки становится описанием релиза.
3. Отправьте тег `vX.Y.Z`. Дальше `release.yml`:
   1. проверяет, что тег совпадает с версией в `Cargo.toml` и что задан секрет `UPDATE_SIGNING_KEY`;
   2. собирает движок, запускает тесты, собирает `awg-ui.exe`;
   3. пакует zip (exe, лицензия, README и CHANGELOG на двух языках, DLL движка);
   4. пишет `update-manifest.json` (версии, размеры и SHA-256 `awg-ui.exe`, `tunnel.dll`, `wintun.dll`) и подписывает
      его `ssh-keygen -Y sign -n awg-ui-update` в `update-manifest.json.sig`;
   5. пишет `SHA256SUMS.txt` и публикует GitHub Release со всем этим и описанием из обоих журналов изменений.
4. Приложение обновляется только по манифесту, подпись которого совпала с открытым ключом `UPDATE_KEY` в
   `src/update/sign.rs`, и сверяет каждый скачанный файл с манифестом (см. [updates](updates.ru.md)). Форку нужна
   своя пара ключей; смена ключа описана в разделе *Update signing key* в [CONTRIBUTING.md](../CONTRIBUTING.md).

Релиз пока не подписан кодовой подписью; в описании упомянуто предупреждение SmartScreen.

## Перед push

1. `cargo fmt`, `cargo test --release`, `cargo build --release` без предупреждений.
2. Изменения интерфейса: следуйте [ui-guidelines.ru.md](ui-guidelines.ru.md) и посмотрите результат в окне.
3. Обновите затронутые страницы этой папки в том же коммите ([оглавление](README.ru.md)).
