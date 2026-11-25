use std::io;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep_until};
use tokio_rustls::server::TlsStream;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::{Client, PHASE_TIMEOUT, allowed_client};
use crate::keys::KeyManager;

const KEY_SWEEP: Duration = Duration::from_secs(15);

pub(super) async fn run(
    mut client_stream: TlsStream<TcpStream>,
    mut upstream: TcpStream,
    buffered: &[u8],
    client: &Client,
    keys: &KeyManager,
    idle_timeout: Duration,
    shutdown: &CancellationToken,
) {
    let started = Instant::now();
    let activity = Mutex::new(started);
    let mut sent = 0;
    let mut received = 0;
    {
        let (client_read, mut client_write) = tokio::io::split(&mut client_stream);
        let (upstream_read, mut upstream_write) = upstream.split();
        let send = async {
            write(&mut upstream_write, buffered, &activity, &mut sent).await?;
            copy(client_read, upstream_write, &activity, &mut sent).await
        };
        tokio::select! {
            _ = send => {}
            _ = copy(upstream_read, &mut client_write, &activity, &mut received) => {}
            _ = wait_for_idle(&activity, idle_timeout) => {}
            _ = wait_for_revocation(keys, &client.key_hash) => {}
            _ = shutdown.cancelled() => {}
        }
    }
    drop(upstream);
    info!(client = %client.id, sent, received, duration_ms = started.elapsed().as_millis() as u64, "forward proxy tunnel closed");

    let _ = shutdown.run_until_cancelled(tokio::time::timeout(PHASE_TIMEOUT, client_stream.shutdown())).await;
}

async fn copy(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    activity: &Mutex<Instant>,
    bytes: &mut u64,
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let len = reader.read(&mut buffer).await?;
        if len == 0 {
            return Ok(());
        }
        write(&mut writer, &buffer[..len], activity, bytes).await?;
    }
}

async fn write(writer: &mut (impl AsyncWrite + Unpin), mut buffer: &[u8], activity: &Mutex<Instant>, bytes: &mut u64) -> io::Result<()> {
    while !buffer.is_empty() {
        let len = writer.write(buffer).await?;
        if len == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        *bytes += len as u64;
        *activity.lock() = Instant::now();
        buffer = &buffer[len..];
    }
    writer.flush().await
}

async fn wait_for_idle(activity: &Mutex<Instant>, idle_timeout: Duration) {
    loop {
        let deadline = *activity.lock() + idle_timeout;
        if deadline <= Instant::now() {
            return;
        }
        sleep_until(deadline).await;
    }
}

async fn wait_for_revocation(keys: &KeyManager, key_hash: &str) {
    let mut interval = tokio::time::interval_at(Instant::now() + KEY_SWEEP, KEY_SWEEP);
    loop {
        interval.tick().await;
        if keys.lookup(key_hash).and_then(allowed_client).is_none() {
            return;
        }
    }
}
