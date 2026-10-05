//! Помощник в сеансе пользователя: действия в родном окне AmneziaWG (UI Automation), которых ядро из сеанса 0
//! сделать не может. Ядро кладёт задание в свою папку `ops` (только SYSTEM и администраторы), запускает
//! `awg-ui.exe --native-op <задание>` с повышенным токеном пользователя и забирает ответ из той же папки.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::conf::TunnelInfo;

pub const FLAG: &str = "--native-op";
const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Serialize, Deserialize, Debug)]
pub enum Op {
    Open,
    Edit(String),
    Import(Option<String>),
    Close,
    Delete(String),
    Details(String),
    ReadConfig(String),
    WriteConfig(String, String),
    /// «Export all tunnels to zip» в этот файл.
    Export(String),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Out {
    Ok,
    Text(String),
    Info(TunnelInfo),
    Err(String),
}

/// Сторона ядра: выполнить `op` в сеансе `session` и вернуть ответ помощника.
pub fn run(session: u32, caller_sid: &str, op: &Op) -> Result<Out, String> {
    let dir = super::data_dir().join("ops");
    crate::win::protect_dir(&dir, super::DATA_SDDL)?;
    let id = crate::store::random_hex();
    let input = dir.join(format!("{id}.in.json"));
    let output = out_path(&input);
    std::fs::write(&input, serde_json::to_vec(op).map_err(|e| e.to_string())?).map_err(|e| crate::fsutil::io_ctx(&input, e))?;
    let exe = crate::engine::install_dir().join(crate::engine::FILES[0]);
    let ran = super::session::run_elevated(session, caller_sid, &exe, &format!("{FLAG} \"{}\"", input.display()), TIMEOUT);
    let out = std::fs::read(&output);
    // В заданиях и ответах бывают конфиги с ключами — файлы не задерживаются. Сбой удаления терпим: каталог `ops`
    // закрыт правами `DATA_SDDL` (SYSTEM и администраторы), а имена случайные и не повторяются.
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    ran?;
    let out = out.map_err(|e| crate::fsutil::io_ctx(&output, e))?;
    serde_json::from_slice(&out).map_err(|e| format!("helper: {e}"))
}

fn out_path(input: &Path) -> PathBuf {
    let name = input.file_name().map(|n| n.to_string_lossy().replace(".in.json", ".out.json")).unwrap_or_default();
    input.with_file_name(name)
}

/// Сторона помощника: прочитать задание, выполнить в родном окне, записать ответ. Код выхода 0 — ответ записан.
pub fn main(input: &Path) -> i32 {
    let out = match std::fs::read(input).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Op>(&b).map_err(|e| e.to_string())) {
        Ok(op) => execute(&crate::backend::native_exe(), op),
        Err(e) => Out::Err(crate::fsutil::io_ctx(&input, e)),
    };
    match serde_json::to_vec(&out).map(|b| std::fs::write(out_path(input), b)) {
        Ok(Ok(())) => 0,
        _ => 1,
    }
}

fn execute(exe: &Path, op: Op) -> Out {
    use crate::native;
    let done = |r: Result<(), String>| r.map_or_else(Out::Err, |()| Out::Ok);
    match op {
        Op::Open => done(std::process::Command::new(exe).spawn().map(drop).map_err(|e| crate::fsutil::io_ctx(&exe, e))),
        Op::Edit(t) => done(native::edit(exe, &t)),
        Op::Import(file) => done(native::import(exe, file.as_deref().map(Path::new))),
        Op::Close => {
            crate::win::close_session_processes("amneziawg.exe");
            Out::Ok
        }
        Op::Delete(t) => done(native::delete(exe, &t)),
        Op::Details(t) => native::read_details(exe, &t).map_or_else(Out::Err, Out::Info),
        Op::ReadConfig(t) => native::read_config(exe, &t).map_or_else(Out::Err, Out::Text),
        Op::WriteConfig(t, text) => done(native::write_config(exe, &t, &text)),
        Op::Export(zip) => done(native::export_all(exe, Path::new(&zip)).map(drop)),
    }
}
