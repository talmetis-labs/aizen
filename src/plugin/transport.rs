//! Bounded protocol IO over already-owned pipes. Process policy and settlement live in the host.

use super::memory_api::{self, ErrorCode, MemoryError, Result, MAX_FRAME, MAX_TRANSACTION_IO};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Default)]
pub struct IoBudget {
    bytes: Arc<AtomicUsize>,
}

impl IoBudget {
    /// Shared by stdin, stdout and stderr. A rejected charge does not wrap or reset the budget.
    pub(crate) fn charge(&self, count: usize) -> Result<()> {
        self.bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(count)
                    .filter(|next| *next <= MAX_TRANSACTION_IO)
            })
            .map_err(|_| MemoryError::invalid("transaction IO limit exceeded"))?;
        Ok(())
    }
}

pub struct Connection<R, W> {
    input: R,
    output: W,
    next_id: u64,
    poisoned: bool,
    budget: IoBudget,
}

impl<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin> Connection<R, W> {
    #[cfg(test)]
    pub fn new(input: R, output: W) -> Self {
        Self::with_budget(input, output, IoBudget::default())
    }

    pub(crate) fn with_budget(input: R, output: W, budget: IoBudget) -> Self {
        Self {
            input,
            output,
            next_id: 1,
            poisoned: false,
            budget,
        }
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub async fn request<P: Serialize, T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
        deadline: Duration,
    ) -> Result<T> {
        if self.poisoned {
            return Err(MemoryError::new(
                ErrorCode::AmbiguousResult,
                "provider transaction is poisoned",
            ));
        }
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| MemoryError::invalid("request ID exhausted"))?;
        let bytes = memory_api::encode_request(id, method, params)?;
        self.budget.charge(bytes.len())?;
        // From this point even cancellation during a write can leave an effectful request dispatched.
        self.poisoned = true;
        let response = tokio::time::timeout(deadline, async {
            self.output.write_all(&bytes).await.map_err(|_| {
                MemoryError::new(ErrorCode::AmbiguousResult, "provider request write failed")
            })?;
            self.output.flush().await.map_err(|_| {
                MemoryError::new(ErrorCode::AmbiguousResult, "provider request flush failed")
            })?;
            read_frame(&mut self.input, &mut self.budget).await
        })
        .await
        .map_err(|_| {
            MemoryError::new(
                ErrorCode::Timeout,
                "provider request exceeded wall deadline",
            )
        })??;
        match memory_api::decode_reply(&response, id)? {
            memory_api::Reply::Result(value) => {
                self.poisoned = false;
                Ok(value)
            }
            memory_api::Reply::Error(error) => {
                self.poisoned = false;
                Err(error)
            }
        }
    }

    /// The caller must close stdin and prove process/tree+pipe settlement after this. This method
    /// only rejects queued unsolicited stdout and accounts for trailing bytes through EOF.
    pub async fn finish_stdout(mut self, deadline: Duration) -> Result<()> {
        tokio::time::timeout(deadline, async move {
            self.output.shutdown().await.map_err(|_| {
                MemoryError::new(ErrorCode::AmbiguousResult, "cannot close provider input")
            })?;
            drop(self.output);
            let mut buffer = [0u8; 8192];
            let mut extra = false;
            loop {
                let count = self
                    .input
                    .read(&mut buffer)
                    .await
                    .map_err(|_| MemoryError::invalid("provider stdout drain failed"))?;
                if count == 0 {
                    break;
                }
                self.budget.charge(count)?;
                extra = true;
            }
            if extra {
                Err(MemoryError::invalid("unsolicited provider stdout"))
            } else {
                Ok(())
            }
        })
        .await
        .map_err(|_| MemoryError::new(ErrorCode::Timeout, "provider stdout did not close"))?
    }
}

async fn read_frame<R: AsyncBufRead + Unpin>(
    input: &mut R,
    budget: &mut IoBudget,
) -> Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let available = input
            .fill_buf()
            .await
            .map_err(|_| MemoryError::invalid("provider stdout read failed"))?;
        if available.is_empty() {
            return Err(MemoryError::new(
                ErrorCode::AmbiguousResult,
                "provider exited before a complete reply",
            ));
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        budget.charge(count)?;
        if frame.len().saturating_add(count) > MAX_FRAME {
            return Err(MemoryError::invalid(
                "provider response exceeds frame limit",
            ));
        }
        let ended = available[count - 1] == b'\n';
        frame.extend_from_slice(&available[..count]);
        input.consume(count);
        if ended {
            return Ok(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tokio::io::{duplex, BufReader};

    #[tokio::test(flavor = "current_thread")]
    async fn one_matching_reply_and_clean_eof() {
        let (host, mut provider) = duplex(4096);
        let (reader, writer) = tokio::io::split(host);
        let task = tokio::spawn(async move {
            let mut input = vec![0u8; 1024];
            assert!(provider.read(&mut input).await.unwrap() > 0);
            provider
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n")
                .await
                .unwrap();
            provider.shutdown().await.unwrap();
        });
        let mut connection = Connection::new(BufReader::new(reader), writer);
        assert_eq!(
            connection
                .request::<_, Value>("health", &json!({}), Duration::from_secs(1))
                .await
                .unwrap(),
            json!({"ok":true})
        );
        connection
            .finish_stdout(Duration::from_secs(1))
            .await
            .unwrap();
        task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mismatched_reply_and_timeout_poison_without_replay() {
        let (host, mut provider) = duplex(4096);
        let (reader, writer) = tokio::io::split(host);
        provider
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n")
            .await
            .unwrap();
        let mut connection = Connection::new(BufReader::new(reader), writer);
        assert!(connection
            .request::<_, Value>("store", &json!({}), Duration::from_secs(1))
            .await
            .is_err());
        assert_eq!(
            connection
                .request::<_, Value>("store", &json!({}), Duration::from_secs(1))
                .await
                .unwrap_err()
                .code,
            ErrorCode::AmbiguousResult
        );
        drop(provider);
        let (host, _silent) = duplex(4096);
        let (reader, writer) = tokio::io::split(host);
        let mut connection = Connection::new(BufReader::new(reader), writer);
        assert_eq!(
            connection
                .request::<_, Value>("health", &json!({}), Duration::from_millis(5))
                .await
                .unwrap_err()
                .code,
            ErrorCode::Timeout
        );
        assert!(connection.poisoned);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remote_invalid_request_is_terminal_but_bad_schema_is_not() {
        let (host, mut provider) = duplex(4096);
        let (reader, writer) = tokio::io::split(host);
        provider.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"invalid\",\"data\":{\"code\":\"invalid_request\",\"message\":\"invalid\"}}}\n").await.unwrap();
        let mut connection = Connection::new(BufReader::new(reader), writer);
        assert_eq!(
            connection
                .request::<_, Value>("get", &json!({}), Duration::from_secs(1))
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert!(!connection.poisoned);
        provider
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":false}\n")
            .await
            .unwrap();
        assert!(connection
            .request::<_, memory_api::Hits>("recall", &json!({}), Duration::from_secs(1))
            .await
            .is_err());
        assert!(connection.poisoned);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_does_not_allow_another_request() {
        let (host, _provider) = duplex(4096);
        let (reader, writer) = tokio::io::split(host);
        let mut connection = Connection::new(BufReader::new(reader), writer);
        assert!(tokio::time::timeout(
            Duration::from_millis(5),
            connection.request::<_, Value>("store", &json!({}), Duration::from_secs(60))
        )
        .await
        .is_err());
        assert!(connection.poisoned);
        assert_eq!(
            connection
                .request::<_, Value>("store", &json!({}), Duration::from_secs(1))
                .await
                .unwrap_err()
                .code,
            ErrorCode::AmbiguousResult
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_shutdown_is_bounded_and_budget_is_cumulative() {
        use std::pin::Pin;
        use std::task::{Context, Poll};
        struct NeverShutdown;
        impl AsyncWrite for NeverShutdown {
            fn poll_write(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                data: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Ready(Ok(data.len()))
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
        }
        let connection = Connection::new(BufReader::new(b"".as_slice()), NeverShutdown);
        assert_eq!(
            connection
                .finish_stdout(Duration::from_millis(5))
                .await
                .unwrap_err()
                .code,
            ErrorCode::Timeout
        );
        let budget = IoBudget::default();
        budget.charge(MAX_TRANSACTION_IO - 1).unwrap();
        budget.charge(1).unwrap();
        assert!(budget.charge(1).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn extra_stdout_and_oversized_frames_fail_closed() {
        let (host, mut provider) = duplex(4096);
        let (reader, writer) = tokio::io::split(host);
        let task = tokio::spawn(async move {
            let mut input = [0u8; 1024];
            assert!(provider.read(&mut input).await.unwrap() > 0);
            provider
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\nnoise\n")
                .await
                .unwrap();
        });
        let mut connection = Connection::new(BufReader::new(reader), writer);
        connection
            .request::<_, Value>("health", &json!({}), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(connection
            .finish_stdout(Duration::from_secs(1))
            .await
            .is_err());
        task.await.unwrap();
        let data = vec![b'x'; MAX_FRAME + 1];
        assert!(read_frame(
            &mut BufReader::new(data.as_slice()),
            &mut IoBudget::default()
        )
        .await
        .is_err());
    }
}
