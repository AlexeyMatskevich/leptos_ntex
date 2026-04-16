//! Minimal demo: serves the Leptos `App` over ntex on 127.0.0.1:3000.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example basic
//! ```

use leptos::prelude::*;
use leptos_meta::{MetaTags, Title, provide_meta_context};
use leptos_ntex_unofficial::{generate_route_list, register_leptos_routes};
use leptos_router::{
    components::{Route, Router, Routes},
    path,
};
use ntex::web::{self, App as NtexApp};

#[component]
fn App() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/")
                        view=|| {
                            view! {
                                <>
                                    <Title text="Leptos + ntex"/>
                                    <h1>"Leptos over ntex"</h1>
                                    <p>"SSR is rendered through an adapter derived from leptos_actix."</p>
                                </>
                            }
                        }
                    />
                    <Route
                        path=path!("/about")
                        view=|| {
                            view! {
                                <>
                                    <Title text="About"/>
                                    <h1>"About"</h1>
                                    <p>"This route is generated from the Leptos router and served by ntex."</p>
                                </>
                            }
                        }
                    />
                </Routes>
            </main>
        </Router>
    }
}

fn shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <MetaTags/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let routes = generate_route_list(App);

    web::server(move || {
        let routes = routes.clone();
        async move {
            NtexApp::new().configure(move |cfg| {
                register_leptos_routes(cfg, routes.clone(), shell);
            })
        }
    })
    .bind(("127.0.0.1", 3000))?
    .run()
    .await
}
