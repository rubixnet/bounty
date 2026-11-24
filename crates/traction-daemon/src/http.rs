//! Loopback HTTP transport for the Local Ingestion Bus. The listener is bound
//! to `127.0.0.1` only by `daemon::start`; this module only serves it.

use std::sync::Arc;
use std::time::Duration;

use hyper::rt::{Read, Write};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::bus::Bus;
use crate::log;

/// A client must finish sending request headers within this time.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// On shutdown, in-flight requests get this long to finish.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Accepts connections until `shutdown` fires, then drains in-flight
/// connections (bounded by [`SHUTDOWN_GRACE`]).
pub async fn serve(listener: TcpListener, bus: Arc<Bus>, mut shutdown: oneshot::Receiver<()>) {
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => serve_connection(TokioIo::new(stream), &bus, &graceful),
                Err(error) => {
                    log::error("failed to accept a bus connection", &error);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            _ = &mut shutdown => break,
        }
    }
    drop(listener);
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()).await;
}

/// Serves one HTTP/1.1 connection through the bus pipeline. Transport
/// agnostic, so the Unix-socket listener can reuse it.
pub fn serve_connection<I>(io: I, bus: &Arc<Bus>, graceful: &GracefulShutdown)
where
    I: Read + Write + Unpin + Send + 'static,
{
    let bus = Arc::clone(bus);
    let service = service_fn(move |request| {
        let bus = Arc::clone(&bus);
        async move { bus.handle(request).await }
    });
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(io, service);
    let connection = graceful.watch(connection);
    tokio::spawn(async move {
        // Client-side errors (resets, timeouts) are routine; nothing to log.
        let _ = connection.await;
    });
}
