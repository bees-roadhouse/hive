//! The data path, and the audit tap that proves what is in it.
//!
//! [`splice`] with `tap: None` is the production path: two raw sockets handed
//! to [`tokio::io::copy_bidirectional`]. The daemon has no opportunity to read
//! anything because it never holds the bytes.
//!
//! With a tap it copies manually and mirrors every byte to a file. That exists
//! for ONE reason ... so a reviewer can run the demo, open the capture, and see
//! TLS records rather than HTTP. It is off by default and the daemon shouts
//! when it is on. It should not ship enabled, and a relay operator who turns it
//! on is recording their users' ciphertext, which is rude even though it is
//! unreadable.
//!
//! # The idle timeout
//!
//! An established splice ends when both halves see EOF, which someone who
//! connects and then says nothing is under no obligation to arrange. Sixty-four
//! of those took an instance off the relay permanently, because the
//! per-instance cap counts connections and nothing was releasing them.
//!
//! So the streams are wrapped in [`Watched`], which does one thing: stamp a
//! shared clock every time a read produces bytes. A watchdog closes the session
//! when that stamp goes stale. **Idle, not total** ... an hours-long download
//! keeps moving bytes and is never touched, while a session that has said
//! nothing in either direction for the timeout is not a session.
//!
//! Note what the wrapper does NOT do: it does not buffer, copy, or look at the
//! bytes. Blindness stays structural.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::Mutex;

#[derive(Debug)]
pub struct Tap {
    path: PathBuf,
    file: Mutex<File>,
}

impl Tap {
    pub async fn create(path: impl AsRef<Path>) -> Result<Arc<Self>> {
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path).await?;
        Ok(Arc::new(Self {
            path,
            file: Mutex::new(file),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn record(&self, dir: &str, bytes: &[u8]) {
        let mut f = self.file.lock().await;
        let _ = f
            .write_all(format!("\n--- {dir} {} bytes ---\n", bytes.len()).as_bytes())
            .await;
        let _ = f.write_all(bytes).await;
        let _ = f.flush().await;
    }
}

/// How a splice is set up: the bytes already read off one side that belong to
/// the other, the idle timeout, and the verification tap.
#[derive(Default)]
pub struct Splice<'a> {
    /// The ClientHello the daemon read while looking for the SNI. Replayed
    /// byte for byte so the instance sees exactly what the browser sent.
    pub to_instance: &'a [u8],
    /// Anything read off the instance past its opening control line. Normally
    /// empty ... the agent waits for the browser ... but discarding it would
    /// be a silent corruption the day that changes.
    pub to_client: &'a [u8],
    /// `None` disables the watchdog. Only for splices that are not on a public
    /// port.
    pub idle_timeout: Option<Duration>,
    pub tap: Option<Arc<Tap>>,
}

/// Pump bytes both ways until either side closes or the session goes idle.
/// Returns (client-to-instance, instance-to-client) byte counts, which is all
/// the daemon retains.
pub async fn splice<A, B>(client: A, instance: B, opts: Splice<'_>) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let activity = Arc::new(Activity::new());
    let mut client = Watched::new(client, activity.clone());
    let mut instance = Watched::new(instance, activity.clone());

    instance.write_all(opts.to_instance).await?;
    client.write_all(opts.to_client).await?;
    if let Some(tap) = &opts.tap {
        tap.record("client->instance (replayed ClientHello)", opts.to_instance)
            .await;
        if !opts.to_client.is_empty() {
            tap.record("instance->client (replayed residue)", opts.to_client)
                .await;
        }
    }
    let prefixed = (opts.to_instance.len() as u64, opts.to_client.len() as u64);

    let copy = async {
        let Some(tap) = opts.tap else {
            // Production path. No buffer, no parse, nothing the daemon can
            // inspect.
            let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut instance).await?;
            return Ok::<(u64, u64), io::Error>((up, down));
        };

        let (mut cr, mut cw) = tokio::io::split(client);
        let (mut ir, mut iw) = tokio::io::split(instance);

        let up = async {
            let mut buf = vec![0u8; 16 * 1024];
            let mut total = 0u64;
            loop {
                let n = cr.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                tap.record("client->instance", &buf[..n]).await;
                iw.write_all(&buf[..n]).await?;
                total += n as u64;
            }
            let _ = iw.shutdown().await;
            Ok::<u64, io::Error>(total)
        };

        let down = async {
            let mut buf = vec![0u8; 16 * 1024];
            let mut total = 0u64;
            loop {
                let n = ir.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                tap.record("instance->client", &buf[..n]).await;
                cw.write_all(&buf[..n]).await?;
                total += n as u64;
            }
            let _ = cw.shutdown().await;
            Ok::<u64, io::Error>(total)
        };

        tokio::try_join!(up, down)
    };

    let (up, down) = match opts.idle_timeout {
        None => copy.await?,
        Some(idle) => {
            tokio::select! {
                r = copy => r?,
                // Dropping `copy` here drops both sockets, which closes them.
                _ = watchdog(activity, idle) => bail!("no bytes in either direction for {idle:?}"),
            }
        }
    };
    Ok((up + prefixed.0, down + prefixed.1))
}

/// Sleeps exactly as long as the session could still be considered live, so a
/// busy session costs one wakeup per `idle` rather than a poll loop.
async fn watchdog(activity: Arc<Activity>, idle: Duration) {
    loop {
        let quiet = activity.quiet_for();
        if quiet >= idle {
            return;
        }
        tokio::time::sleep(idle - quiet).await;
    }
}

/// A shared "last time bytes moved" stamp, in milliseconds since the splice
/// started. Millisecond resolution is plenty for a timeout measured in
/// minutes, and it fits an atomic with no lock anywhere on the data path.
struct Activity {
    base: tokio::time::Instant,
    last_ms: AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self {
            base: tokio::time::Instant::now(),
            last_ms: AtomicU64::new(0),
        }
    }

    fn touch(&self) {
        let ms = self.base.elapsed().as_millis() as u64;
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    fn quiet_for(&self) -> Duration {
        let now = self.base.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_ms.load(Ordering::Relaxed)))
    }
}

/// Stamps [`Activity`] whenever a read yields bytes. It forwards everything
/// else untouched and holds no buffer of its own.
struct Watched<S> {
    inner: S,
    activity: Arc<Activity>,
}

impl<S> Watched<S> {
    fn new(inner: S, activity: Arc<Activity>) -> Self {
        Self { inner, activity }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let r = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(r, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.activity.touch();
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::duplex;

    /// The attack: connect, say something valid, then say nothing and never
    /// close. Without the watchdog this returns only when the process does.
    #[tokio::test]
    async fn a_silent_session_is_closed_after_the_idle_timeout() {
        let (client, _client_peer) = duplex(1024);
        let (instance, _instance_peer) = duplex(1024);

        let started = std::time::Instant::now();
        let err = splice(
            client,
            instance,
            Splice {
                idle_timeout: Some(Duration::from_millis(200)),
                ..Default::default()
            },
        )
        .await
        .expect_err("an idle splice must not hang");
        assert!(err.to_string().contains("no bytes"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "it hung");
    }

    /// The regression that matters in the other direction: a long transfer is
    /// not idle just because it is long.
    #[tokio::test]
    async fn a_slow_but_moving_session_outlives_the_idle_timeout() {
        let (client, mut client_peer) = duplex(1024);
        let (instance, mut instance_peer) = duplex(1024);

        let pump = tokio::spawn(async move {
            splice(
                client,
                instance,
                Splice {
                    idle_timeout: Some(Duration::from_secs(1)),
                    ..Default::default()
                },
            )
            .await
        });

        // Ten hops at 200ms is 2s of a 1s idle timeout. A hop has to overrun
        // by 5x before this goes flaky on a loaded runner.
        let mut sink = vec![0u8; 1];
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            client_peer.write_all(b"x").await.expect("write");
            instance_peer.read_exact(&mut sink).await.expect("read");
        }
        drop(client_peer);
        drop(instance_peer);

        let (up, _down) = pump.await.expect("join").expect("splice");
        assert_eq!(up, 10, "every byte should have crossed");
    }
}
