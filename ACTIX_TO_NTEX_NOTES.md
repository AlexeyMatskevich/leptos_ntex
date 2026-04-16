# leptos_actix -> leptos_ntex

Этот репозиторий держит адаптер Leptos для `ntex`, основанный на `leptos_actix`
и обогащённый идеями из `leptos_axum`:

- базовый ориентир для структуры/имён: `integrations/actix/src/lib.rs` из
  `leptos` (ветка `main`)
- идеи по fallback-обработке статики, async I/O, state-threading и
  композабельности пришли из `integrations/axum/src/lib.rs`
- цель: `src/leptos_ntex.rs` максимально близко по публичному API к обеим
  интеграциям, меняя только то, что требует API `ntex`

## Что перенесено из actix (1:1)

- `ResponseParts`, `ResponseOptions`, `Request`, `redirect()`
- `handle_server_fns()` и `handle_server_fns_with_context()`
- `render_app_to_stream*` / `render_app_to_stream_in_order*` / `render_app_async*`
  / `render_app_to_stream_with_context_and_replace_blocks`
- `generate_route_list*` семейство (5 вариантов)
- `StaticRouteGenerator` и все его методы
- `LeptosRoutes` trait с двумя impl: для `App<M, T, Err>` и для
  `&mut ServiceConfig<Err>`
- `extract()` helper (для ntex extractors, работающих с head части запроса)
- атрибуты на crate root: `#![forbid(unsafe_code)]`, `#![deny(missing_docs)]`
- doc-комментарии на всех публичных элементах API

## Что перенесено из axum (axum-идеи, портированные под ntex)

- `file_and_error_handler` / `file_and_error_handler_with_context` — GET
  Route, который сначала ищет файл в `options.site_root`, а при отсутствии
  рендерит shell с `404 Not Found`. У нас использует
  `ntex_files::NamedFile` (MIME/ETag/Last-Modified автоматически)
- `site_pkg_dir_service(options)` — готовый `ntex_files::Files` сервис для
  `options.site_pkg_dir` под `options.site_root`, аналог axum'овского
  `ServeDir` хелпера
- `PinnedHtmlStream` — публичный type alias
  `Pin<Box<dyn Stream<Item = io::Result<NBytes>> + Send>>`
- `generate_request_and_parts(req, payload) -> (NtexRequest, HttpRequest)`
  — симметричный axum-хелпер для декомпозиции запроса
- `handle_response_inner(...)` — публичная low-level точка входа для
  SSR-рендера одного запроса (возвращает `PinnedFuture<HttpResponse>`),
  которую можно встраивать в свои route handlers
- `use_app_state::<T>() -> Option<T>` — достаёт клон ntex app state из
  `Request` в реактивном контексте (идиоматичнее, чем
  `extract::<types::State<T>>()`)
- method-specific роутинг для server fn: при регистрации через
  `leptos_routes*` каждый `(path, method)` получает свой Route с filter на
  метод. Неверный метод теперь ловится на уровне роутера (405), а не
  проваливается к 400 от диспатчера
- async I/O в static path: `write_static_route` и `handle_static_route`
  оборачивают `fs::write` / `fs::create_dir_all` / `NamedFile::open` в
  `ntex::rt::spawn_blocking`, чтобы не блокировать arbiter

## Feature flags

```toml
[features]
default = []
tracing = ["dep:tracing"]
islands-router = ["leptos/islands-router"]
```

- `tracing`: включает `#[tracing::instrument(...)]` на публичных route
  функциях и `tracing::warn!/error!` в redirect / OptionalParam branch
- `islands-router`: переключает стрим-строители на `*_branching()` варианты
  и активирует header-based island navigation detection

## Что пришлось изменить под `ntex`

- вместо `actix_web::{App, Route, HttpRequest, HttpResponse}` используется
  семейство `ntex::web`
- вместо `HttpServer::new(...)` используется `ntex::web::server(...)` с
  async factory
- у `LeptosRoutes for App<M, T, Err>` более развёрнутые type bounds
  (`T: ServiceFactory<..., ntex::service::cfg::SharedCfg>`,
  `Err: ErrorRenderer`, `Err::Container: From<StateExtractorError>`);
  `register_leptos_routes(cfg, ...)` — shortcut через `ServiceConfig`
- `ntex::http::HeaderMap` не реализует owned `IntoIterator`, поэтому
  `extend_response` переносит весь `ResponseParts` через `std::mem::take` и
  итерирует по `iter()` с клонированием значений
- `ntex` body-stream API требует `BoxedBodyStream`, HTML/stream ответы
  оборачиваются через `Body::from(BoxedBodyStream::new(...))`
- `ntex::web::HttpRequest` и `HttpResponse` не `Send`, поэтому:
  - `Request` хранит `Option<SendWrapper<HttpRequest>>`
  - backend для `server_fn` реализован через `NtexRequest`,
    `NtexServerResponse`, `NtexServerFnBackend`
  - `NtexServerResponse` оборачивает `HttpResponse` в `SendWrapper`
  - `extract()` и `handle_response_inner()` оборачивают async блок в
    `SendWrapper` (`PinnedFuture<HttpResponse>` требует Send)
- `Request::Drop` defensive: если `SendWrapper<HttpRequest>` оказался не на
  своём потоке (static prerender может переносить owner'ы), он форгетится
  через `std::mem::forget`, чтобы избежать panic на cross-thread drop
- `any_spawner::Executor` инициализируется через
  `init_custom_executor(NtexExecutor)`. `NtexExecutor` делегирует
  `spawn`/`spawn_local` в `ntex::rt::spawn`. Это важно, потому что ntex
  имеет три рантайма (`tokio` / `compio` / `neon` по умолчанию), и
  `any_spawner` не знает ни про один из них. `ntex::rt::spawn`
  абстрагирует все три и кидает задачу на тот же arbiter, где
  обрабатывается запрос. Инициализация идемпотентна и вызывается во всех
  публичных entry points (`generate_route_list*`, `handle_server_fns*`,
  `render_app_*`, `handle_static_route`, `file_and_error_handler*`,
  `handle_response_inner`)
- для generic `Err: ErrorRenderer` используется явный
  `Route::<Err>::new().method(...).to(...)`, потому что shortcut
  `web::get()/post()/head()` сваливают тип на `Route<DefaultError>`

## State threading

ntex state устроен иначе, чем у axum (там один типовой `S` на Router). В
ntex app state типовой (через `.state::<T>(value)`) и может быть несколько
типов. Соответственно:

- чтение состояния изнутри server fn:
  - `use_app_state::<T>() -> Option<T>` — синхронно, клонирует из
    `Request` в Leptos-контексте (требует `T: Clone + 'static`)
  - альтернатива: `extract::<ntex::web::types::State<T>>().await?`
- проброс состояния в SSR-компоненты: через `leptos_routes_with_context`
  пользователь пишет `additional_context`, который вызывает
  `provide_context(state.clone())`

## Что покрыто тестами

**Unit tests (16 штук, `src/lib.rs`):**

- SSR-рендер: OutOfOrder (`/`, `/about`), InOrder, Async
- HEAD 200 через `App::leptos_routes()` и через `register_leptos_routes`
- `App::leptos_routes()` impl
- `register_leptos_routes(cfg, ...)` через `ServiceConfig`
- server fn POST / `handle_server_fns()`
- `redirect()` для HTML form (302) и XHR (client-side header)
- websocket server fn (реальный ws-upgrade, binary frames)
- `extract()` helper
- `use_app_state::<T>()` из server fn
- `StaticRouteGenerator` пишет HTML на диск
- `handle_static_route` HTTP layer
- `file_and_error_handler` serve + 404 fallback

**Integration tests (6 штук, `tests/integration.rs`):**

- реальный TCP-сервер через `ntex::web::test::server(...)`
- SSR + server fn end-to-end (запрос через `srv.request(...)`)
- method-specific routing: GET на POST-only server fn отклоняется роутером
  при регистрации через `leptos_routes`
- catchall-режим `handle_server_fns()`: GET на POST-only отдаёт 400 изнутри
  диспатчера (документированное отличие)
- server fn без `register_explicit()` — авто-регистрация через
  `inventory::submit!` работает на native
- `file_and_error_handler` end-to-end: файл отдаётся, 404 для отсутствующих
- `site_pkg_dir_service` end-to-end: `/pkg/app.js` отдаётся из filesystem

Проверяется: `cargo test`, `cargo test --test integration`,
`cargo test --all-features`, `cargo clippy --all-features --tests` (ноль
предупреждений).

## Исправления после ревью

Проведено три ревью (критический, перфоманс+идиомы, API/используем-ли-актуальные-возможности).
Найденные и исправленные проблемы:

**Корректность:**

- `register_leptos_routes`/`ServiceConfig` impl раньше упускал HEAD-хендлер
  и `provide_context(method)` — теперь паритет с `App::leptos_routes()`
- `SsrMode::_ => unreachable!()` заменено на fallback к `OutOfOrder` с
  `tracing::warn!`, потому что `SsrMode` помечен `#[non_exhaustive]` и
  новый вариант upstream приводил бы к runtime-панике
- `redirect()` больше не пишет в `eprintln!` без фичи `tracing`
- `Request::Drop` с `mem::forget` теперь явно документирует ограниченную
  утечку `Rc` при cross-thread drop (осознанный trade-off
  leak-vs-arbiter-panic)

**Перфоманс:**

- `ensure_executor_initialized()` гейтится через `std::sync::Once` —
  больше не аллоцирует `Box::new(NtexExecutor)` на каждый запрос
- `get_server_fn_service` не делает бесполезный `match` по методу
  (`ntex::http::Method` ≡ `http::Method`)
- `Path::exists()` в `handle_static_route` больше не отправляется в
  `spawn_blocking` — один `stat(2)` syscall, hop на thread pool дороже
- `handle_server_fns_with_context` больше не аллоцирует `String` и клон
  `Method` per-request — используется `req.path()`/`req.method()` inline

**Идиомы:**

- `HttpResponse::streaming(...)` вместо ручного `BoxedBodyStream::new(...)`
  + `Body::from(...)`
- `NBytes::copy_from_slice(&data)` вместо `NBytesMut::from(&data[..]).freeze()`
- `HeaderValue::from_static("")` вместо `HeaderValue::from_str("").unwrap()`
- `io::Error::other(e)` вместо `io::Error::other(e.to_string())` (сохраняет
  source chain)

**Cargo.toml:**

- `send_wrapper = { features = ["futures"] }` теперь явно включена
  (`SendWrapper<impl Future>: Future` требует этой фичи)
- `leptos`/`leptos_router` подняты до актуальных 0.8.17/0.8.13
- `serde_json` в `[dev-dependencies]` (использовался только в тестах)
- убрана избыточная прямая зависимость `http = "1.4.0"` (реэкспортится
  через `ntex-http`)

**Документация:**

- `register_explicit()` переписан — на native не нужен (inventory работает),
  нужен только на wasm/edge runtimes
- `Request` явно документирует семантику cross-thread drop (leak vs panic)
- `handle_static_route` комментирует почему content-type жёстко `text/html`
  (SSG всегда HTML, паритет с actix/axum)
- `replace_blocks` параметр в `render_app_to_stream_with_context_and_replace_blocks`
  помечен TODO — Leptos stream API не экспонируют этот toggle,
  параметр оставлен для API-паритета

## Известные расхождения с actix/axum

- `LeptosRoutes for App<...>` имеет более развёрнутые type bounds
- `ntex_files::FilesError` не реэкспортируется, поэтому type-bound на
  `Err::Container: From<FilesError>` в сигнатуре `site_pkg_dir_service` не
  выражается явно — пользователь упирается в это ограничение при
  инстанциировании с нестандартным `Err`
- `islands-router` форвардится на `leptos/islands-router`, а actix/axum
  напрямую включают `tachys/islands`/`tachys/mark_branches` через workspace

Если сессия оборвётся, продолжать стоит с проверки `src/leptos_ntex.rs`.
Публичный API совпадает с actix-версией и имеет axum-инспирированные
дополнения.
