//! Подделка ядра для тестов окна: ответ на запрос задаёт тест, запросы записываются по порядку.

use std::sync::Mutex;

use super::proto::{Request, Response};
use super::CoreApi;
use crate::crash::lock;

type Reply = dyn Fn(&Request) -> Result<Response, String> + Send + Sync;

pub struct FakeCore {
    reply: Box<Reply>,
    /// Запросы в виде `{:?}`: у `Request` нет `Clone`, а тестам хватает текста.
    seen: Mutex<Vec<String>>,
}

impl FakeCore {
    pub fn new(reply: impl Fn(&Request) -> Result<Response, String> + Send + Sync + 'static) -> FakeCore {
        FakeCore { reply: Box::new(reply), seen: Mutex::default() }
    }

    /// Ядро не запущено: каждый запрос — ошибка канала.
    pub fn unreachable(error: &str) -> FakeCore {
        let error = error.to_string();
        FakeCore::new(move |_| Err(error.clone()))
    }

    pub fn requests(&self) -> Vec<String> {
        lock(&self.seen).clone()
    }
}

impl CoreApi for FakeCore {
    fn call(&self, req: Request) -> Result<Response, String> {
        lock(&self.seen).push(format!("{req:?}"));
        (self.reply)(&req)
    }
}
