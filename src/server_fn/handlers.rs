//! Server-function request dispatch: the per-path handler factory and the
//! catch-all [`handle_server_fns`] / [`handle_server_fns_with_context`]
//! entry points.

use leptos::{
    context::provide_context,
    reactive::{computed::ScopedFuture, owner::Owner},
};
use leptos_integration_utils::ExtendResponse;
use ntex::http::{
    Payload, StatusCode, Uri,
    header::{self, HeaderValue},
};
use ntex::web::{self, ErrorRenderer, HttpRequest, HttpResponse, Route};
use server_fn::{
    ServerFnTraitObj,
    error::SERVER_FN_ERROR_HEADER,
    middleware::{BoxedService, Layer},
};
use std::sync::Arc;

use crate::config::{PayloadTooLarge, content_length_exceeds, server_fn_config};
use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions, accept_header_includes_html};
use crate::routes::ensure_executor_initialized;
use crate::server_fn::registry::{get_server_fn_service, server_fn_methods};
use crate::server_fn::request::NtexRequest;
use crate::server_fn::response::NtexServerResponse;

/// Runs a prepared [`BoxedService`] for a server function, setting up the
/// reactive Owner, provided contexts, referrer-based redirect fixup, and
/// `ResponseOptions` extension. Shared between the catchall
/// [`handle_server_fns_with_context`] handler and the per-path
/// [`handle_specific_server_fn_with_context`] handler.
pub(crate) async fn dispatch_server_fn(
    mut service: BoxedService<NtexRequest, NtexServerResponse>,
    req: HttpRequest,
    payload: Payload,
    additional_context: impl FnOnce() + 'static + Send,
) -> HttpResponse {
    let owner = Owner::new();
    owner
        .with(|| {
            ScopedFuture::new(async move {
                provide_context(Request::new(&req));
                let res_options = ResponseOptions::default();
                provide_context(res_options.clone());
                additional_context();

                let accepts_html = req
                    .headers()
                    .get(header::ACCEPT)
                    .and_then(|v| v.to_str().ok())
                    .map(accept_header_includes_html)
                    .unwrap_or(false);
                let raw_referrer = req.headers().get(header::REFERER).cloned();
                // The request is consumed by `service.run` below, so the
                // origin used for the post-run Location checks is captured
                // up front.
                let (conn_scheme, conn_host) = {
                    let connection = req.connection_info();
                    (
                        connection.scheme().to_string(),
                        connection.host().to_string(),
                    )
                };
                let referrer = raw_referrer
                    .as_ref()
                    .and_then(|value| same_origin_location(&conn_scheme, &conn_host, value));

                let mut res = service.run(NtexRequest::from((req, payload))).await;

                // Whether the post-run `Location` is an echo of the request's
                // Referer — the signature of server_fn's form-redirect fallback
                // for an HTML form. It may append a bare `?`, which
                // `location_matches_referrer` tolerates.
                let location_is_referrer = raw_referrer.as_ref().is_some_and(|raw| {
                    res.0
                        .headers()
                        .get(header::LOCATION)
                        .is_some_and(|location| location_matches_referrer(location, raw))
                });

                if accepts_html {
                    if location_is_referrer {
                        if let Some(referrer) = referrer.clone() {
                            *res.0.status_mut() = StatusCode::FOUND;
                            res.0.headers_mut().insert(header::LOCATION, referrer);
                        } else {
                            res.0.headers_mut().remove(header::LOCATION);
                            reset_status_after_redirect_strip(&mut res);
                        }
                    } else if res.0.status().is_success()
                        && res.0.headers().get(header::LOCATION).is_none()
                        && let Some(referrer) = referrer
                    {
                        *res.0.status_mut() = StatusCode::FOUND;
                        res.0.headers_mut().insert(header::LOCATION, referrer);
                    }
                }
                // No non-HTML branch: when the strict `Accept` parser refuses
                // HTML (e.g. `text/html;q=0`) this integration deliberately does
                // NOT strip server_fn's form-redirect — matching `leptos_axum` /
                // `leptos_actix`, whose form-redirect fallback fires on the same
                // loose `contains("text/html")`. A same-origin form `302` is left
                // intact, and a user middleware's own short-circuit `3xx` is
                // likewise untouched. (There is no way to suppress only the
                // fallback: server_fn drives it solely off `req.accepts()`, which
                // middleware shares — see `NtexRequest::accepts`.) The cross-origin
                // case is still handled by the same-origin guard below.

                // Same-origin invariant for the server_fn-layer `Location`.
                // The block above only repairs the EXACT referer echo, and
                // it is gated on the strict `Accept` parser — but server_fn's
                // own form-redirect fallback gates on a loose
                // `contains("text/html")` check and rewrites the referer on
                // the error path (appending the error query), so a
                // referer-derived `Location` can reach this point in shapes
                // the block above never sees (`Accept: text/html;q=0`, error
                // URLs). Rather than chase every shape, enforce the
                // documented policy wholesale: at this layer a redirect may
                // only target the current origin. Application-level
                // redirects via `redirect()`/`ResponseOptions` are applied
                // in `extend_response` AFTER this check and stay untouched.
                if let Some(location) = res.0.headers().get(header::LOCATION).cloned()
                    && same_origin_location(&conn_scheme, &conn_host, &location).is_none()
                {
                    res.0.headers_mut().remove(header::LOCATION);
                    reset_status_after_redirect_strip(&mut res);
                }

                let mut wrapped = NtexResponse(res.take());
                wrapped.extend_response(&res_options);
                wrapped.take()
            })
        })
        .await
}

/// Resets the status of a response whose `Location` was just stripped — either
/// because it failed the same-origin invariant or because it was a referer echo
/// refused by the strict `Accept` parser.
///
/// A server-function **error** response carries [`SERVER_FN_ERROR_HEADER`]: it
/// started life as a `500` from `NtexServerResponse::error_response` that
/// server_fn's form-redirect fallback then overwrote with `302`. Dropping that
/// redirect must not silently promote the failure to `200 OK`, so an error
/// response is restored to `500` while an ordinary, successful form redirect
/// becomes `200`.
fn reset_status_after_redirect_strip(res: &mut NtexServerResponse) {
    if res.0.status().is_redirection() {
        let was_error = res.0.headers().get(SERVER_FN_ERROR_HEADER).is_some();
        *res.0.status_mut() = if was_error {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        };
    }
}

fn location_matches_referrer(location: &HeaderValue, referrer: &HeaderValue) -> bool {
    let Some(location) = location.to_str().ok() else {
        return false;
    };
    let Some(referrer) = referrer.to_str().ok() else {
        return false;
    };
    location == referrer || location.strip_suffix('?') == Some(referrer)
}

/// Accepts a `Location`/`Referer` value only when it stays on the current
/// origin: either an origin-relative path (`/...`, but not the
/// protocol-relative `//...`) or an absolute URI whose scheme and authority
/// match the connection. Returns the same-origin path-and-query form, or
/// `None` for everything else (cross-origin, protocol-relative,
/// `javascript:`-style schemes, unparseable values).
fn same_origin_location(
    conn_scheme: &str,
    conn_host: &str,
    value: &HeaderValue,
) -> Option<HeaderValue> {
    let value = value.to_str().ok()?;
    // An origin-relative path is a SINGLE leading '/'. Reject both the
    // protocol-relative `//host` form and the backslash-prefixed `/\host`
    // form: browsers fold `\` to `/` in HTTP(S) URLs, so a `Location: /\evil.com`
    // resolves as the cross-origin `//evil.com` and would otherwise slip
    // through this same-origin fast path. A bare `/` (len 1) has no second
    // byte and stays same-origin.
    if value.starts_with('/')
        && !value
            .as_bytes()
            .get(1)
            .is_some_and(|b| *b == b'/' || *b == b'\\')
    {
        return HeaderValue::from_str(value).ok();
    }

    let uri = value.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    let authority = uri.authority()?.as_str();
    if !scheme.eq_ignore_ascii_case(conn_scheme) || !authority.eq_ignore_ascii_case(conn_host) {
        return None;
    }

    let path_and_query = uri.path_and_query().map_or("/", |pq| pq.as_str());
    HeaderValue::from_str(path_and_query).ok()
}

/// Returns an ntex [`Route`] bound to a single, pre-resolved server
/// function — skipping the per-request `HashMap` lookup that
/// [`handle_server_fns_with_context`] performs. Used by
/// [`LeptosRoutes::leptos_routes_with_context`](crate::LeptosRoutes::leptos_routes_with_context)
/// at registration time so every server-fn endpoint closes over its own
/// [`ServerFnTraitObj`] and middleware list.
///
/// The middleware factory is invoked once at registration and the result
/// is cached behind an [`Arc`] — cheap to clone per request (one atomic
/// increment), no per-request `Vec` allocation for the layer list.
pub(crate) fn handle_specific_server_fn_with_context<Err>(
    server_fn: ServerFnTraitObj<NtexRequest, NtexServerResponse>,
    additional_context: impl Fn() + 'static + Clone + Send,
) -> Route<Err>
where
    Err: ErrorRenderer,
{
    ensure_executor_initialized();
    let method = server_fn.method();
    let middleware: Arc<[Arc<dyn Layer<NtexRequest, NtexServerResponse>>]> =
        server_fn.middleware().into();
    let server_fn = Arc::new(server_fn);

    Route::<Err>::new()
        .method(method)
        .to(move |req: HttpRequest, payload: web::types::Payload| {
            let server_fn = server_fn.clone();
            let middleware = middleware.clone();
            let additional_context = additional_context.clone();
            async move {
                let limit = server_fn_config(&req).payload_limit;
                // Preflight: reject oversize bodies declared up-front
                // via Content-Length without reading any of the body.
                if content_length_exceeds(&req, limit) {
                    return oversize_response(limit);
                }
                let mut service = (*server_fn).clone().boxed();
                for m in middleware.iter() {
                    service = m.layer(service);
                }
                let resp = dispatch_server_fn(
                    service,
                    req.clone(),
                    payload.into_inner(),
                    additional_context,
                )
                .await;
                // Promote streaming/chunked overflow (detected by
                // `collect_payload` or the `try_into_stream` adapter
                // through the request-scoped marker) into a real 413.
                if req.extensions().get::<PayloadTooLarge>().is_some() {
                    return oversize_response(limit);
                }
                resp
            }
        })
}

/// Builds a canonical `413 Payload Too Large` response with a human-
/// readable body that states the configured limit.
fn oversize_response(limit: usize) -> HttpResponse {
    HttpResponse::PayloadTooLarge()
        .content_type("text/plain; charset=utf-8")
        .body(format!("payload exceeds limit of {limit} bytes"))
}

/// Returns an ntex [`Route`] that dispatches requests to the registered
/// server functions.
///
/// This can be mounted once at a wildcard path matching the API prefix:
///
/// ```no_run
/// use ntex::web::{self, App as NtexApp};
/// use leptos_ntex_unofficial::handle_server_fns;
///
/// # fn main() -> std::io::Result<()> {
/// web::server(|| async {
///     NtexApp::new().route("/api/{tail}*", handle_server_fns())
/// })
/// .bind(("127.0.0.1", 3000))?
/// .run();
/// # Ok(())
/// # }
/// ```
///
/// # Routing pattern
///
/// Register with the ntex tail pattern **`/{tail}*`**, not actix-web's
/// `/{tail:.*}`. In ntex, `{name:.*}` matches only a *single* path segment, so
/// `/api/{tail:.*}` would `404` any server-function endpoint whose path
/// contains a slash. `{tail}*` is ntex's cross-segment tail match. See
/// [`file_and_error_handler`](crate::file_and_error_handler) for the same rule
/// on the file fallback.
///
/// ## Provided Context Types
/// - [`ResponseOptions`](crate::ResponseOptions)
/// - [`Request`](crate::Request)
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn handle_server_fns<Err>() -> Route<Err>
where
    Err: ErrorRenderer,
{
    handle_server_fns_with_context(|| {})
}

/// Variant of [`handle_server_fns`] that injects additional values into the
/// reactive context before dispatching the server function.
///
/// If your server functions expect some piece of context, make sure to
/// provide it both here and in
/// [`LeptosRoutes::leptos_routes_with_context`](crate::LeptosRoutes::leptos_routes_with_context)
/// (or whichever rendering method you use). During SSR, server functions
/// are called by the rendering method, while subsequent calls from the
/// client are handled by this server function handler — both paths need
/// to see the same context.
///
/// # Proxy headers
///
/// The dispatcher resolves the request's origin through ntex's `ConnectionInfo`
/// — which trusts `Forwarded` / `X-Forwarded-Host` / `X-Forwarded-Proto` — to
/// enforce the same-origin guard on the HTML-form referrer redirect fallback.
/// Behind a reverse proxy, strip any client-supplied forwarding headers and set
/// trusted values at the proxy before the request reaches ntex (see the
/// crate-level "Proxy headers" note).
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn handle_server_fns_with_context<Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
) -> Route<Err>
where
    Err: ErrorRenderer,
{
    ensure_executor_initialized();
    Route::<Err>::new().to(move |req: HttpRequest, payload: web::types::Payload| {
        let additional_context = additional_context.clone();
        async move {
            let limit = server_fn_config(&req).payload_limit;
            if content_length_exceeds(&req, limit) {
                return oversize_response(limit);
            }
            if let Some(service) = get_server_fn_service(req.path(), req.method()) {
                let resp = dispatch_server_fn(
                    service,
                    req.clone(),
                    payload.into_inner(),
                    additional_context,
                )
                .await;
                if req.extensions().get::<PayloadTooLarge>().is_some() {
                    return oversize_response(limit);
                }
                resp
            } else {
                let allowed = server_fn_methods(req.path());
                if allowed.is_empty() {
                    HttpResponse::BadRequest().body(format!(
                        "Could not find a server function at the route {}. \
\n\nIt's likely that either\n1. The API prefix you specify in the `#[server]` macro doesn't match the prefix at which your server function handler is mounted, or\n2. You are on a platform that doesn't support automatic server function registration and you need to call register_explicit() on the server function type, somewhere in your `main` function.",
                        req.path()
                    ))
                } else {
                    let allow = allowed
                        .iter()
                        .map(ntex::http::Method::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    HttpResponse::MethodNotAllowed()
                        .header(header::ALLOW, allow)
                        .body(format!(
                            "Server function at route {} does not accept method {}.",
                            req.path(),
                            req.method()
                        ))
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lets_expect::lets_expect;

    // ----- same_origin_location: the post-run Location safety guard -------
    // A redirect `Location` reaching the server-fn layer may only target the
    // current origin. The guard accepts an origin-relative path or a
    // scheme+authority-matching absolute URI (returning the path-and-query
    // form), and rejects everything else. The connection origin is fixed at
    // `http://example.test:8080`. The load-bearing negative is the
    // backslash-prefixed `/\host`: browsers fold `\` to `/`, so it must be
    // treated as the cross-origin `//host`, not as a same-origin path.
    fn same_origin(value: &str) -> Option<String> {
        same_origin_location(
            "http",
            "example.test:8080",
            &HeaderValue::from_str(value).unwrap(),
        )
        .and_then(|v| v.to_str().ok().map(str::to_owned))
    }

    lets_expect! {
        expect(same_origin(value)) as the_same_origin_location {
            let value = "/dashboard";

            to keeps_an_origin_relative_path { equal(Some("/dashboard".to_string())) }

            when the_value_is_the_bare_root {
                let value = "/";
                to stays_same_origin { equal(Some("/".to_string())) }
            }

            when the_value_is_protocol_relative {
                let value = "//evil.test/steal";
                to is_rejected_as_cross_origin { equal(None) }
            }

            when the_value_is_backslash_prefixed {
                // Browsers fold `\` to `/`, so `/\evil.test` is really
                // `//evil.test` — the guard must NOT pass it as same-origin.
                let value = "/\\evil.test/steal";
                to is_rejected_like_a_protocol_relative_url { equal(None) }
            }

            when the_value_is_a_same_origin_absolute_uri {
                let value = "http://example.test:8080/form?x=1";
                to is_reduced_to_its_path_and_query {
                    equal(Some("/form?x=1".to_string()))
                }
            }

            when the_value_targets_a_different_host {
                let value = "http://evil.test/form";
                to is_rejected { equal(None) }
            }

            when the_value_uses_a_different_scheme {
                let value = "https://example.test:8080/form";
                to is_rejected { equal(None) }
            }

            when the_value_is_a_bare_origin_with_no_path {
                // Pins the business behavior: a bare-origin absolute URI
                // (no path at all) is same-origin and resolves to `/`.
                // NOTE: `http::Uri::path_and_query()` is `Some(&"/")` here
                // (it is only `None` when NO scheme is present at all), so
                // this does not actually drive the `map_or("/", ...)`
                // default's `None` arm on line 205 — that default appears
                // to be unreachable once `scheme_str()?` above it has
                // already succeeded. See the audit note in this module's
                // doc comment / task summary.
                let value = "http://example.test:8080";
                to defaults_to_root { equal(Some("/".to_string())) }
            }

            when the_value_uses_a_different_case_but_same_origin {
                // `eq_ignore_ascii_case` on both scheme and authority: a
                // mixed-case scheme/host must still be recognized as the
                // same origin as the lowercase connection tuple.
                let value = "HTTP://Example.Test:8080/form";
                to is_still_treated_as_same_origin {
                    equal(Some("/form".to_string()))
                }
            }

            when the_value_is_a_scheme_less_relative_reference {
                // No leading `/` and no scheme: `scheme_str()` returns `None`
                // for a relative reference, so this must fail closed rather
                // than being treated as same-origin by default.
                let value = "evil.test/path";
                to is_rejected { equal(None) }
            }

            when the_value_is_a_javascript_scheme_uri {
                // The docstring explicitly calls out `javascript:`-style
                // schemes as a rejected case — `alert(1)` here parses as a
                // URI opaque part, not an authority, so `authority()` is
                // `None`.
                let value = "javascript:alert(1)";
                to is_rejected { equal(None) }
            }
        }
    }

    // A bare (non-UTF-8) `HeaderValue` cannot be built from a `&str`, so the
    // invalid-UTF-8 leaf below constructs the fixture directly rather than
    // reusing the `same_origin(value: &str)` helper.
    fn same_origin_from_header(value: HeaderValue) -> Option<String> {
        same_origin_location("http", "example.test:8080", &value)
            .and_then(|v| v.to_str().ok().map(str::to_owned))
    }

    lets_expect! {
        expect(same_origin_from_header(value.clone())) as the_same_origin_location_header_decode {
            // `to_str()` requires visible ASCII; `0xFA` is a legal header
            // BYTE (`from_bytes` accepts 32..=255 excluding 127) but fails
            // `to_str()`, driving the `value.to_str().ok()?` Err arm on
            // line 182 — unreachable from any `&str`-built fixture.
            let value = HeaderValue::from_bytes(b"hello\xfa").unwrap();

            to is_rejected_as_unparseable { equal(None) }
        }
    }

    // ----- location_matches_referrer: the referer-echo detector ------------
    // Detects whether server_fn's post-run `Location` is an echo of the
    // request's `Referer` (its form-redirect fallback's signature), tolerating
    // a bare trailing `?` server_fn may append. Reached from
    // `dispatch_server_fn`, but never spec'd directly until now.
    fn matches_referrer(location: &str, referrer: &str) -> bool {
        location_matches_referrer(
            &HeaderValue::from_str(location).unwrap(),
            &HeaderValue::from_str(referrer).unwrap(),
        )
    }

    lets_expect! {
        expect(matches_referrer(location, referrer)) as location_matches_referrer_fn {
            let location = "/form";
            let referrer = "/form";

            to matches_on_an_exact_match { be_true }

            when the_location_has_a_trailing_bare_question_mark {
                // server_fn's form-redirect fallback may append a bare `?`
                // to the referer; the detector must still recognize it as
                // an echo.
                let location = "/form?";
                let referrer = "/form";
                to still_matches { be_true }
            }

            when the_location_does_not_match_the_referrer {
                let location = "/form?evil=1";
                let referrer = "/form";
                to does_not_match { be_false }
            }
        }
    }

    lets_expect! {
        expect(location_matches_referrer(&location, &referrer)) as location_matches_referrer_decode_failures {
            // Both sides start out valid so only the varied side's `to_str()`
            // Err arm is exercised per context.
            let location = HeaderValue::from_str("/form").unwrap();
            let referrer = HeaderValue::from_str("/form").unwrap();

            when the_location_is_not_valid_utf8 {
                let location = HeaderValue::from_bytes(b"hello\xfa").unwrap();
                to is_rejected_as_non_matching { equal(false) }
            }

            when the_referrer_is_not_valid_utf8 {
                let referrer = HeaderValue::from_bytes(b"hello\xfa").unwrap();
                to is_rejected_as_non_matching { equal(false) }
            }

            when both_sides_fail_to_decode_identically {
                // Both sides carry the SAME invalid-UTF-8 bytes: a
                // regression that swallowed the `to_str()` Err with
                // `unwrap_or_default()` (collapsing both to `""`) would
                // wrongly report a match here, since `"" == ""` is `true`.
                // The one-sided contexts above cannot catch that specific
                // regression, because a lone empty side never equals the
                // other side's real value either way.
                let location = HeaderValue::from_bytes(b"hello\xfa").unwrap();
                let referrer = HeaderValue::from_bytes(b"hello\xfa").unwrap();
                to still_does_not_match { equal(false) }
            }
        }
    }

    // ----- reset_status_after_redirect_strip: post-strip status repair -----
    // Resets the status of a response whose `Location` was just stripped.
    // Only fires when the CURRENT status is a redirection; a non-redirect
    // status must be left untouched — that no-op branch was previously
    // reachable only indirectly (and never independently pinned).
    fn reset_status(status: StatusCode, with_error_header: bool) -> StatusCode {
        let mut response = HttpResponse::build(status);
        if with_error_header {
            response.header(
                SERVER_FN_ERROR_HEADER,
                HeaderValue::from_static("ServerFnError"),
            );
        }
        let mut res = NtexServerResponse::from(response.finish());
        reset_status_after_redirect_strip(&mut res);
        res.0.status()
    }

    lets_expect! {
        expect(reset_status(status, with_error_header)) as reset_status_after_redirect_strip_fn {
            let status = StatusCode::FOUND;
            let with_error_header = false;

            to resets_a_stripped_success_redirect_to_200 { equal(StatusCode::OK) }

            when the_response_carries_the_server_fn_error_header {
                let with_error_header = true;
                to resets_a_stripped_error_redirect_to_500 {
                    equal(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }

            when the_status_is_not_a_redirection {
                // The `is_redirection()` guard's false branch: called on a
                // response that never had a redirect at all — must be a
                // strict no-op, not just "not 500".
                let status = StatusCode::OK;
                to leaves_the_status_unchanged { equal(StatusCode::OK) }
            }

            when the_status_is_not_a_redirection_and_not_ok {
                // A second non-redirection status (404), so the no-op isn't
                // merely coincidental with the 200 default already used
                // elsewhere in this table.
                let status = StatusCode::NOT_FOUND;
                to leaves_the_status_unchanged { equal(StatusCode::NOT_FOUND) }
            }
        }
    }
}
