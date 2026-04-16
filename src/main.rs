use leptos_ntex::{App, leptos_ntex::{generate_route_list, register_leptos_routes}, shell};
use ntex::web::{self, App as NtexApp};

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
