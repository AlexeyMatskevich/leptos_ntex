//! Minimal extractor example for Leptos server functions.
//!
//! This shows how to implement a custom ntex `FromRequest` extractor that
//! reads a session cookie and app state, then use it inside a Leptos
//! `#[server]` function through `leptos_ntex_unofficial::extract`.
//!
//! Check it with:
//!
//! ```sh
//! cargo check --example auth_extractor --features cookie
//! ```

use std::{collections::HashMap, fmt};

use leptos::prelude::*;
use leptos_ntex_unofficial::{NtexServerFnBackend, extract, handle_server_fns};
use ntex::{
    http::{HttpMessage, Payload},
    web::{self, App as NtexApp, ErrorRenderer, FromRequest, HttpRequest},
};

const SESSION_COOKIE: &str = "session";

#[derive(Clone)]
struct User {
    name: String,
}

// This is ordinary ntex application state: register it with `App::state` and
// read it back from the request while extracting the current user.
// See: https://ntex.rs/docs/application#state
struct AuthState {
    users_by_session: HashMap<String, User>,
}

impl AuthState {
    fn demo() -> Self {
        Self {
            users_by_session: HashMap::from([(
                "demo-session".to_string(),
                User {
                    name: "Ada".to_string(),
                },
            )]),
        }
    }

    fn user_for_session(&self, session: &str) -> Option<User> {
        self.users_by_session.get(session).cloned()
    }
}

struct CurrentUser(User);

#[derive(Debug)]
enum CurrentUserError {
    MissingCookie,
    MissingState,
    UnknownSession,
}

impl fmt::Display for CurrentUserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCookie => write!(f, "missing session cookie"),
            Self::MissingState => write!(f, "auth state is not configured"),
            Self::UnknownSession => write!(f, "unknown session"),
        }
    }
}

// ntex request extractors are types that implement `FromRequest`.
// See: https://ntex.rs/docs/extractors
impl<Err> FromRequest<Err> for CurrentUser
where
    Err: ErrorRenderer,
{
    type Error = CurrentUserError;

    async fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Result<Self, Self::Error> {
        let session = req
            .cookie(SESSION_COOKIE)
            .ok_or(CurrentUserError::MissingCookie)?
            .value()
            .to_owned();

        let auth = req
            .app_state::<AuthState>()
            .ok_or(CurrentUserError::MissingState)?;

        auth.user_for_session(&session)
            .map(CurrentUser)
            .ok_or(CurrentUserError::UnknownSession)
    }
}

#[server(
    name = CurrentUserName,
    prefix = "/api",
    endpoint = "current_user_name",
    server = NtexServerFnBackend
)]
async fn current_user_name() -> Result<String, ServerFnError> {
    // This is the Leptos-side use: the adapter calls `CurrentUser::from_request`
    // against the ntex request stored in the server-function context.
    let CurrentUser(user) = extract::<CurrentUser>()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(user.name)
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::server(|| async {
        NtexApp::new()
            // Make `AuthState` available to ntex handlers and extractors.
            // See: https://ntex.rs/docs/extractors#application-state-extractor
            .state(AuthState::demo())
            .route("/api/{tail}*", handle_server_fns())
    })
    .bind(("127.0.0.1", 3000))?
    .run()
    .await
}
