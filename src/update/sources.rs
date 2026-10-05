//! Источники обновлений за интерфейсом: что `Manager` спрашивает у сети — последние релизы и загрузка файла.
//! `GithubSources` — настоящие запросы (`feed` + `net`); в тестах менеджера вместо него подставляется подделка,
//! поэтому `check` и ветки «источник недоступен» / «уже стоит» проверяются без сети.

use std::path::Path;

use super::{feed, net, ours, sign};

/// Почему не удалось получить наш релиз. Релиз без подписанного манифеста (до 0.4.0) — не сбой: он ставится только
/// вручную, и окно показывает это нейтрально, а не красной ошибкой.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OursError {
    /// Готовый текст для окна и журнала.
    pub(super) message: String,
    /// В релизе нет манифеста и подписи.
    pub(super) no_manifest: bool,
}

impl OursError {
    pub(super) fn no_manifest(message: String) -> Self {
        OursError { message, no_manifest: true }
    }
}

impl From<String> for OursError {
    fn from(message: String) -> Self {
        OursError { message, no_manifest: false }
    }
}

#[cfg(test)]
impl From<&str> for OursError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// Откуда менеджер берёт релизы и файлы. Ошибка — готовый текст для журнала и строки истории.
pub(super) trait Sources: Send + Sync {
    /// Последний релиз оригинального AmneziaWG.
    fn latest_native(&self) -> Result<feed::Release, String>;
    /// Релиз оригинального AmneziaWG по тегу (для копии установленной версии).
    fn native_by_tag(&self, tag: &str) -> Result<feed::Release, String>;
    /// Последний наш релиз с проверенным манифестом (программа и движок).
    fn latest_ours(&self) -> Result<(feed::Release, sign::Manifest), OursError>;
    /// Самая новая стабильная метка amneziawg-windows (из неё собран наш движок).
    fn latest_engine_tag(&self) -> Result<String, String>;
    /// Небольшой файл `url` целиком в память (манифест и подпись), не больше `max` байт.
    fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, String>;
    /// Манифест с подписью, проверенный ключом обновлений. За интерфейсом, чтобы тест подписывал своим ключом, а
    /// не подделывал сам разбор: подпись и поля проверяет настоящий `sign`.
    fn verify_manifest(&self, json: &[u8], sig: &str) -> Result<sign::Manifest, String>;
    /// Файл `url` в `dest` не больше `max` байт; `progress(скачано, всего если известно)`. Возвращает размер.
    fn download(&self, url: &str, dest: &Path, max: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<u64, String>;
}

/// Настоящие источники: релизы GitHub по HTTPS.
pub(super) struct GithubSources;

impl Sources for GithubSources {
    fn latest_native(&self) -> Result<feed::Release, String> {
        feed::latest(feed::NATIVE_REPO)
    }

    fn native_by_tag(&self, tag: &str) -> Result<feed::Release, String> {
        feed::by_tag(feed::NATIVE_REPO, tag)
    }

    fn latest_ours(&self) -> Result<(feed::Release, sign::Manifest), OursError> {
        ours::latest()
    }

    fn latest_engine_tag(&self) -> Result<String, String> {
        feed::latest_stable_tag(feed::ENGINE_UPSTREAM_REPO)
    }

    fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        net::get(url, None, max)
    }

    fn verify_manifest(&self, json: &[u8], sig: &str) -> Result<sign::Manifest, String> {
        sign::manifest(json, sig)
    }

    fn download(&self, url: &str, dest: &Path, max: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<u64, String> {
        net::download(url, dest, max, progress)
    }
}

#[cfg(test)]
pub(super) mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::update::ours::testutil;

    /// Подделка: готовые ответы; файлы по адресам отдаются из памяти (`serve`), `download` пишет их на диск и
    /// помнит адреса загрузок; манифест проверяется настоящим `sign` тестовым ключом.
    pub(in super::super) struct FakeSources {
        pub native: Mutex<Result<feed::Release, String>>,
        pub ours: Mutex<Result<(feed::Release, sign::Manifest), OursError>>,
        /// Ответ на «самая новая метка движка»; по умолчанию сеть недоступна.
        pub engine_tag: Mutex<Result<String, String>>,
        pub latest_calls: Mutex<u32>,
        /// Релизы AmneziaWG, которые отдаёт `native_by_tag` (по `tag`); остальные метки — «нет релиза».
        pub tags: Mutex<Vec<feed::Release>>,
        /// Метки, о которых спрашивали `native_by_tag`.
        pub tag_calls: Mutex<Vec<String>>,
        files: Mutex<HashMap<String, Vec<u8>>>,
        /// Адреса, по которым `download` и `get` отказывают («обрыв сети»).
        broken: Mutex<Vec<String>>,
        pub downloads: Mutex<Vec<String>>,
    }

    impl FakeSources {
        pub fn new(native: Result<feed::Release, String>, ours: Result<(feed::Release, sign::Manifest), OursError>) -> FakeSources {
            FakeSources {
                native: Mutex::new(native),
                ours: Mutex::new(ours),
                engine_tag: Mutex::new(Err("offline".into())),
                latest_calls: Mutex::new(0),
                tags: Mutex::new(Vec::new()),
                tag_calls: Mutex::new(Vec::new()),
                files: Mutex::new(HashMap::new()),
                broken: Mutex::new(Vec::new()),
                downloads: Mutex::new(Vec::new()),
            }
        }

        /// Оба источника недоступны.
        pub fn down() -> FakeSources {
            FakeSources::new(Err("offline".into()), Err("offline".into()))
        }

        /// Отдавать `bytes` по адресу `url` (заменяет прежнее).
        pub fn serve(&self, url: &str, bytes: &[u8]) {
            self.files.lock().unwrap().insert(url.to_string(), bytes.to_vec());
        }

        /// Загрузка с `url` обрывается ошибкой.
        pub fn break_url(&self, url: &str) {
            self.broken.lock().unwrap().push(url.to_string());
        }

        fn body(&self, url: &str) -> Result<Vec<u8>, String> {
            if self.broken.lock().unwrap().iter().any(|u| u == url) {
                return Err(format!("fake: connection lost {url}"));
            }
            self.files.lock().unwrap().get(url).cloned().ok_or_else(|| format!("fake: no network for {url}"))
        }
    }

    impl Sources for FakeSources {
        fn latest_native(&self) -> Result<feed::Release, String> {
            *self.latest_calls.lock().unwrap() += 1;
            self.native.lock().unwrap().clone()
        }

        fn native_by_tag(&self, tag: &str) -> Result<feed::Release, String> {
            self.tag_calls.lock().unwrap().push(tag.to_string());
            self.tags.lock().unwrap().iter().find(|r| r.tag == tag).cloned().ok_or_else(|| format!("fake: no release {tag}"))
        }

        fn latest_ours(&self) -> Result<(feed::Release, sign::Manifest), OursError> {
            self.ours.lock().unwrap().clone()
        }

        fn latest_engine_tag(&self) -> Result<String, String> {
            self.engine_tag.lock().unwrap().clone()
        }

        fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, String> {
            let body = self.body(url)?;
            if body.len() > max {
                return Err(format!("response too large: {} > {max} bytes", body.len()));
            }
            Ok(body)
        }

        fn verify_manifest(&self, json: &[u8], sig: &str) -> Result<sign::Manifest, String> {
            testutil::signed_by_test_signer(json, sig)
        }

        fn download(&self, url: &str, dest: &Path, max: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<u64, String> {
            self.downloads.lock().unwrap().push(url.to_string());
            let body = self.body(url)?;
            let len = body.len() as u64;
            if len > max {
                return Err(format!("response too large: {len} > {max} bytes"));
            }
            progress(len / 2, Some(len));
            std::fs::write(dest, &body).map_err(|e| crate::fsutil::io_ctx(&dest, e))?;
            progress(len, Some(len));
            Ok(len)
        }
    }
}
