use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub fn restart_llama_server(socket: &str, container: &str) -> Result<(), String> {
  let mut stream = UnixStream::connect(socket).map_err(|e| format!("cannot open {socket}: {e}"))?;
  stream
    .set_read_timeout(Some(Duration::from_secs(20)))
    .map_err(|e| e.to_string())?;
  let req = format!(
    "POST /containers/{container}/restart HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
  );
  stream
    .write_all(req.as_bytes())
    .map_err(|e| format!("docker socket write: {e}"))?;
  let mut buf = [0u8; 1024];
  let n = stream
    .read(&mut buf)
    .map_err(|e| format!("docker socket read: {e}"))?;
  let head = String::from_utf8_lossy(&buf[..n]);
  if head.starts_with("HTTP/1.1 204") || head.starts_with("HTTP/1.1 200") {
    Ok(())
  } else {
    Err(format!(
      "docker restart {} failed: {}",
      container,
      head.lines().next().unwrap_or("empty response")
    ))
  }
}
