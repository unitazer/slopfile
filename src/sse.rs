
use crate::types::SseChunk;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Debug)]
pub enum SseError {
    BadJson(String),
    Reqwest(reqwest::Error),
    Io(String),
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SseError::BadJson(s) => write!(f, "bad sse json: {s}"),
            SseError::Reqwest(e) => write!(f, "reqwest: {e}"),
            SseError::Io(s) => write!(f, "io: {s}"),
        }
    }
}

impl Error for SseError {}

impl From<reqwest::Error> for SseError {
    fn from(e: reqwest::Error) -> Self {
        SseError::Reqwest(e)
    }
}

pub struct SseStream<S> {
    stream: S,
    buf: Vec<u8>,
    done: bool,
}

impl<S> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<SseChunk, SseError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        loop {
            match take_next_event(&mut self.buf) {
                Ok(Some((chunk, done))) => {
                    self.done = done;
                    if done {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Ok(None) => {
                    match self.stream.poll_next_unpin(cx) {
                        Poll::Ready(Some(Ok(bytes))) => {
                            if !bytes.is_empty() {
                                self.buf.extend_from_slice(&bytes);
                            }
                        }
                        Poll::Ready(Some(Err(e))) => {
                            return Poll::Ready(Some(Err(SseError::Reqwest(e))))
                        }
                        Poll::Ready(None) => {
                            if !self.buf.is_empty() {
                                let trailing = String::from_utf8_lossy(&self.buf).to_string();
                                self.buf.clear();
                                return Poll::Ready(Some(Err(SseError::Io(format!(
                                    "stream ended with {trailing:?} unparsed"
                                )))));
                            }
                            return Poll::Ready(None);
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                Err(e) => {
                    self.buf.clear();
                    return Poll::Ready(Some(Err(e)));
                }
            }
        }
    }
}

fn take_next_event(buf: &mut Vec<u8>) -> Result<Option<(SseChunk, bool)>, SseError> {
    loop {
        let Some(nl) = buf.iter().position(|&b| b == b'\n') else {
            return Ok(None);
        };
        let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
        let line = String::from_utf8_lossy(&line_bytes);
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with(':') || !line.starts_with("data:") {
            continue;
        }
        let payload = line[5..].trim_start();
        if payload == "[DONE]" {
            return Ok(Some((
                SseChunk {
                    choices: Vec::new(),
                    usage: None,
                },
                true,
            )));
        }
        match serde_json::from_str::<SseChunk>(payload) {
            Ok(chunk) => return Ok(Some((chunk, false))),
            Err(e) => {
                return Err(SseError::BadJson(format!(
                    "unparseable data payload: {e}: {payload}"
                )))
            }
        }
    }
}

pub fn stream_chunks<S>(stream: S) -> SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    SseStream {
        stream,
        buf: Vec::new(),
        done: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::iter;

    fn stream_from_bytes(
        chunks: Vec<&str>,
    ) -> SseStream<impl Stream<Item = Result<Bytes, reqwest::Error>>> {
        let it = chunks
            .into_iter()
            .map(|s| Ok(Bytes::from(s.to_string())))
            .collect::<Vec<_>>();
        stream_chunks(iter(it))
    }

    fn collect<S>(mut stream: S) -> Result<Vec<SseChunk>, SseError>
    where
        S: Stream<Item = Result<SseChunk, SseError>> + Unpin,
    {
        let mut out = Vec::new();
        loop {
            let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
            match Pin::new(&mut stream).poll_next(&mut cx) {
                Poll::Ready(Some(Ok(c))) => out.push(c),
                Poll::Ready(None) => break,
                Poll::Ready(Some(Err(e))) => return Err(e),
                Poll::Pending => unreachable!("synchronous fixture"),
            }
        }
        Ok(out)
    }

    #[test]
    fn parses_single_event() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
        let chunks = collect(stream_from_bytes(vec![sse])).expect("collect");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("hi"));
    }

    #[test]
    fn parses_multiple_events_across_chunk_boundaries() {
        let sse1 = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"We\"}}]}\n\n";
        let sse2 = "data: {\"choices\":[{\"delta\":{\"content\":\" need\"}}]}\n";
        let sse3 = "data: {\"choices\":[{\"delta\":{\"content\":\" rest\"}}]}\n\n";
        let chunks = collect(stream_from_bytes(vec![sse1, sse2, sse3])).expect("collect");
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0].choices[0].delta.reasoning_content.as_deref(),
            Some("We")
        );
        assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some(" need"));
        assert_eq!(chunks[2].choices[0].delta.content.as_deref(), Some(" rest"));
    }

    #[test]
    fn done_terminates_stream() {
        let sse = "data: [DONE]\n\n";
        let chunks = collect(stream_from_bytes(vec![sse])).expect("collect");
        assert!(chunks.is_empty());
    }

    #[test]
    fn partial_line_waits_for_more() {
        let sse1 = "data: {\"choices\":[{\"delta\":{\"content\":\"par";
        let sse2 = "tial\"}}]}\n\n";
        let chunks = collect(stream_from_bytes(vec![sse1, sse2])).expect("collect");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("partial"));
    }

    #[test]
    fn bad_json_is_error() {
        let sse = "data: {not json}\n\n";
        let err = collect(stream_from_bytes(vec![sse])).expect_err("must fail");
        assert!(matches!(err, SseError::BadJson(_)));
    }
}