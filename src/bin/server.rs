use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    match std::env::args().nth(1).as_deref() {
        Some("service") => return stt_server_next::service::dispatch(),
        Some("install") => return stt_server_next::service::install(),
        Some("uninstall") => return stt_server_next::service::uninstall(),
        _ => {}
    }
    stt_server_next::api::run_http(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
