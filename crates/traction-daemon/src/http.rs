//! HTTP/1.1 serving for the Local Ingestion Bus: loopback TCP on every OS and,
//! on macOS/Linux, the Unix socket. Both listeners are bound by
//! `daemon::start`; this module only serves them through the same bus.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use hyper::rt::{Read, Write};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use crate::bus::{Bus, Transport};
use crate::log;

/// A caller must finish sending request headers within this time.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// On shutdown, in-flight requests get this long to finish.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Every listener the bus serves.
pub struct Listeners {
    pub tcp: TcpListener,
    #[cfg(unix)]
    pub unix: tokio::net::UnixListener,
}

/// One accepted connection, tagged by the listener it came from.
enum Accepted {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl Listeners {
    async fn accept(&self) -> io::Result<Accepted> {
        #[cfg(unix)]
        {
            tokio::select! {
                accepted = self.tcp.accept() => accepted.map(|(stream, _)| Accepted::Tcp(stream)),
                accepted = self.unix.accept() => accepted.map(|(stream, _)| Accepted::Unix(stream)),
            }
        }
        #[cfg(not(unix))]
        {
            self.tcp
                .accept()
                .await
                .map(|(stream, _)| Accepted::Tcp(stream))
        }
    }
}

/// Accepts connections until `shutdown` fires, then drains in-flight
/// connections (bounded by [`SHUTDOWN_GRACE`]).
pub async fn serve(listeners: Listeners, bus: Arc<Bus>, mut shutdown: oneshot::Receiver<()>) {
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listeners.accept() => match accepted {
                Ok(Accepted::Tcp(stream)) => {
                    serve_connection(TokioIo::new(stream), Transport::Http, &bus, &graceful);
                }
                #[cfg(unix)]
                Ok(Accepted::Unix(stream)) => {
                    serve_connection(TokioIo::new(stream), Transport::Unix, &bus, &graceful);
                }
                Err(error) => {
                    log::error("failed to accept a bus connection", &error);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            _ = &mut shutdown => break,
        }
    }
    drop(listeners);
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()).await;
}

/// Serves one HTTP/1.1 connection through the bus pipeline. `transport` tells
/// the bus whether to check `Host`, so the Unix-socket listener can reuse it.
fn serve_connection<I>(io: I, transport: Transport, bus: &Arc<Bus>, graceful: &GracefulShutdown)
where
    I: Read + Write + Unpin + Send + 'static,
{
    let bus = Arc::clone(bus);
    let service = service_fn(move |request| {
        let bus = Arc::clone(&bus);
        async move { bus.handle(request, transport).await }
    });
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(io, service);
    let connection = graceful.watch(connection);
    tokio::spawn(async move {
        // Caller-side errors (resets, timeouts) are routine; nothing to log.
        let _ = connection.await;
    });
}
