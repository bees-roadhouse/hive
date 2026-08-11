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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
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

/// Pump bytes both ways until either side closes. Returns (client-to-instance,
/// instance-to-client) byte counts, which is all the daemon retains.
///
/// `prefix` is the ClientHello already read off the client while looking for
/// the SNI. It is replayed to the instance byte for byte so the instance sees
/// exactly what the browser sent.
pub async fn splice<A, B>(
    mut client: A,
    mut instance: B,
    prefix: &[u8],
    tap: Option<Arc<Tap>>,
) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    instance.write_all(prefix).await?;
    if let Some(tap) = &tap {
        tap.record("client->instance (replayed ClientHello)", prefix)
            .await;
    }
    let prefixed = prefix.len() as u64;

    let Some(tap) = tap else {
        // Production path. No buffer, no parse, nothing the daemon can inspect.
        let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut instance).await?;
        return Ok((up + prefixed, down));
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
        Ok::<u64, std::io::Error>(total)
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
        Ok::<u64, std::io::Error>(total)
    };

    let (up, down) = tokio::try_join!(up, down)?;
    Ok((up + prefixed, down))
}
