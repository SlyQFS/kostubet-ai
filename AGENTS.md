# AGENTS.md — KostubetAI 2.0

Регламент для ИИ-агентов и разработчиков.

## Стек

teloxide 0.14 (macros, rustls), tokio, sqlx 0.8 (sqlite, migrate), reqwest 0.12 (rustls, webpki-roots, stream), rust-stemmers, serde, dotenvy, tracing, thiserror. Edition 2021, release-профиль: `lto`, `strip`, `panic = "abort"`.

## Структура

- `src/main.rs` — конфиг, пул, матчер, запуск Dispatcher.
- `src/config.rs` — всё из `.env` с clamp'ами. Новые настройки только сюда + в `.env.example` и README.
- `src/storage/mod.rs` — sqlx-пул, `messages` (скользящее окно по `chat_id, thread_id`) и `user_states` (мета-блоки). Схема меняется только новой миграцией в `migrations/` (грузятся рантаймом из `./migrations`).
- `src/llm/client.rs` — OpenAI-совместимый `/chat/completions`, ретраи (2, backoff 1.5c×2^n на 429/5xx/сеть), SSE-стрим с fallback на не-стрим. `state.rs` — активная модель. `prompt.rs` — чистая сборка `[System + Knowledge + User Context] + [History] + [User]`.
- `src/knowledge/` — `loader.rs` (карточки `knowledge/*.json`), `matcher.rs` (поиск по корням слов без окончаний через русский стемминг Snowball + точные ключи и сленг).
- `src/bot/` — `router.rs` (маршрутизация), `handlers_admin.rs` (ЛС-админка, `/model` + CallbackQuery), `handlers_chat.rs` (триггеры, очередь, прогрессивный вывод), `mod.rs` (`App`, `RateLimiter`, пейсер 50 мс).
- `src/utils/` — `sanitizer.rs` (парсинг/вырезание `<user:...>`), `text.rs` (`split_telegram`, токен-эвристика ×3 символа).

## Поведение бота

- ЛС: только админка; не-админам заглушка. Группы: ответ только по триггерам (`@бот`, reply на бота); чужие `/команды` — молча.
- Память изолирована по `(chat_id, thread_id)`. system-промпт и мета-блоки в окно обрезки не входят.
- `<user:...>` теги из ответа модели: извлечь → upsert → вырезать → отправить. Сырые теги в чат не попадают.
- Ошибки LLM/Telegram — только человекочитаемые подсказки (`LlmError::user_hint`), без стектрейсов.

## Правила кода

- Без `.unwrap()`/`.expect()` в рантайме (допустимы в тестах и при старте до запуска диспетчера). Poisoned mutex — обрабатывать (`lock().ok()` / `map_err`).
- Ошибки Telegram-запросов не роняют бот: `if let Err(e) = ... { warn!(...) }` или `ResponseResult`.
- Ответы — plain text без Markdown; длинные сообщения через `split_telegram(3900)` с паузой 250 мс между чанками.
- Каждый новый запрос к LLM проходит: семафор очереди → пейсер → rate limiter уже проверен на входе.
- Схема БД — только добавлением миграции, `migrations/NNNN_*.sql`.
- Юнит-тесты обязательны: парсеры, санитайзер, промпт-сборка, матчер стемминга, SQL-методы (in-memory).

## Команды

```bash
cargo check
cargo clippy --all-targets   # без предупреждений
cargo test                   # перед завершением любой задачи
docker compose up -d --build
```

## Docker

Multi-stage: builder `rust:alpine` + cargo-chef, runtime `alpine`, non-root. Тома: `/app/data` (БД), `migrations/` и `knowledge/` копируются в образ. Рабочая директория `/app` — миграции и knowledge ищутся относительно неё.
