//! hive-ui ... leptos 0.7 SSR axum server for the journal-canvas.
//!
//! v0 scope: one route (`/`) renders the 5 most-recent journal entries
//! fetched live from hive-api. The markdown canvas, checkbox component,
//! and structured views land in follow-up commits.

mod api;
mod app;
mod pages;

use std::net::SocketAddr;

use axum::Router;
use leptos::config::LeptosOptions;
use leptos_axum::{generate_route_list, LeptosRoutes};
use tower_http::services::ServeDir;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::app::{shell, App};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,hive_ui=debug")))
        .with(fmt::layer())
        .init();

    let port: u16 = std::env::var("HIVE_UI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8091);
    let addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], port));

    let conf = LeptosOptions::builder()
        .output_name("hive-ui")
        .site_root("target/site")
        .site_pkg_dir("pkg")
        .site_addr(addr)
        .reload_port(3001)
        .env(leptos::config::Env::DEV)
        .build();

    let routes = generate_route_list(App);

    let style_dir = ServeDir::new("style");

    let conf_for_shell = conf.clone();
    let app: Router = Router::<LeptosOptions>::new()
        .leptos_routes(&conf, routes, move || shell(conf_for_shell.clone()))
        .nest_service("/style", style_dir)
        .with_state(conf);

    tracing::info!("hive-ui listening on http://{addr}");
    tracing::info!("hive-api base: {}", api::api_base());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}
