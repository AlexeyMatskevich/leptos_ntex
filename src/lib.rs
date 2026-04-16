#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

pub mod config;
pub mod extract;
pub mod files;
pub mod leptos_routes;
pub mod render;
pub mod request;
pub mod response;
pub mod routes;
pub mod server_fn;
pub mod static_routes;

pub use config::{DEFAULT_PAYLOAD_LIMIT, DEFAULT_WS_CHANNEL_BUFFER, LeptosServerFnConfig};
pub use extract::{expect_app_state, extract, extract_with_err, use_app_state};
pub use files::{file_and_error_handler, file_and_error_handler_with_context, site_pkg_dir_service};
pub use leptos_routes::{LeptosRoutes, register_leptos_routes};
pub use render::{
    handle_response_inner, render_app_async, render_app_async_with_context,
    render_app_to_stream, render_app_to_stream_in_order,
    render_app_to_stream_in_order_with_context, render_app_to_stream_with_context,
    render_app_to_stream_with_context_and_replace_blocks,
};
pub use request::Request;
pub use response::{PinnedHtmlStream, ResponseOptions, ResponseParts, redirect};
pub use routes::{
    NtexRouteListing, generate_route_list, generate_route_list_with_exclusions,
    generate_route_list_with_exclusions_and_ssg,
    generate_route_list_with_exclusions_and_ssg_and_context,
    generate_route_list_with_ssg, try_init_executor,
};
pub use server_fn::{
    NtexExecutor, NtexRequest, NtexServerFnBackend, NtexServerResponse,
    generate_request_and_parts, get_server_fn_service, handle_server_fns,
    handle_server_fns_with_context, register_explicit, server_fn_paths,
};
pub use static_routes::StaticRouteGenerator;

#[cfg(test)]
mod tests;
