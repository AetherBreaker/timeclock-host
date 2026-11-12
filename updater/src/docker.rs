//! What the updater asks of Docker: whether Coolify's helper container is running, and its API's answers.
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Whether an image reference names `helper`, with any registry, tag or digest.
pub fn image_matches(image: &str, helper: &str) -> bool {
  let name = image.split('@').next().unwrap_or(image);
  // A tag follows the last ':' after the last '/'; a ':' before that is a registry's port.
  let name = match name.rfind(':') {
    Some(i) if !name[i..].contains('/') => &name[..i],
    _ => name,
  };
  name == helper || name.strip_suffix(helper).is_some_and(|rest| rest.ends_with('/'))
}

/// Whether Docker's `/containers/json` answer lists a container running `helper`. No JSON library: every
/// `"Image":"…"` value is checked (the exact key, so `"ImageID"` never matches).
pub fn helper_in_containers(json: &str, helper: &str) -> bool {
  json
    .split(r#""Image":""#)
    .skip(1)
    .any(|rest| rest.split('"').next().is_some_and(|image| image_matches(image, helper)))
}

/// Ask Docker for its running containers over its socket; any failure reads as no helper.
pub fn helper_running(socket: &Path, helper: &str) -> bool {
  let ask = || -> io::Result<String> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    // HTTP/1.0, so Docker answers without chunked encoding and closes when done.
    s.write_all(b"GET /containers/json HTTP/1.0\r\nHost: docker\r\n\r\n")?;
    let mut answer = String::new();
    s.read_to_string(&mut answer)?;
    Ok(answer)
  };
  ask().is_ok_and(|a| helper_in_containers(&a, helper))
}

/// GET `path` from Docker's API over its socket: the body of a 200 answer.
pub fn docker_get(socket: &Path, path: &str) -> io::Result<String> {
  let mut s = UnixStream::connect(socket)?;
  s.set_read_timeout(Some(Duration::from_secs(5)))?;
  s.set_write_timeout(Some(Duration::from_secs(5)))?;
  s.write_all(format!("GET {path} HTTP/1.0\r\nHost: docker\r\n\r\n").as_bytes())?;
  let mut answer = String::new();
  s.read_to_string(&mut answer)?;
  let (head, body) = answer
    .split_once("\r\n\r\n")
    .ok_or_else(|| io::Error::other("no HTTP answer"))?;
  if !head.starts_with("HTTP/1.0 200") && !head.starts_with("HTTP/1.1 200") {
    return Err(io::Error::other(head.lines().next().unwrap_or("").to_string()));
  }
  Ok(body.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  const CONTAINERS: &str = r#"[{"Id":"a1","Names":["/coolify-sentinel"],"Image":"ghcr.io/coollabsio/sentinel:1.1.3","ImageID":"sha256:81f8","Command":"/app/sentinel"},{"Id":"b2","Names":["/px0q"],"Image":"coollabsio/coolify-helper:1.0.19","ImageID":"sha256:ab12"}]"#;

  #[test]
  fn helper_tag_and_registry_match() {
    assert!(image_matches(
      "coollabsio/coolify-helper:1.0.19",
      "coollabsio/coolify-helper"
    ));
    assert!(image_matches("coollabsio/coolify-helper", "coollabsio/coolify-helper"));
    assert!(image_matches(
      "ghcr.io/coollabsio/coolify-helper:1.0.20",
      "coollabsio/coolify-helper"
    ));
    assert!(image_matches(
      "coollabsio/coolify-helper@sha256:abc",
      "coollabsio/coolify-helper"
    ));
    assert!(helper_in_containers(CONTAINERS, "coollabsio/coolify-helper"));
  }

  #[test]
  fn sentinel_does_not_match() {
    assert!(!image_matches(
      "ghcr.io/coollabsio/sentinel:1.1.3",
      "coollabsio/coolify-helper"
    ));
    assert!(!image_matches(
      "other/coollabsio/coolify-helper-x:1",
      "coollabsio/coolify-helper"
    ));
    let only_sentinel = CONTAINERS.split(r#",{"Id":"b2""#).next().unwrap().to_string() + "]";
    assert!(!helper_in_containers(&only_sentinel, "coollabsio/coolify-helper"));
  }

  #[test]
  fn imageid_key_is_ignored() {
    // "ImageID" starts like "Image"; only the Image value names the image.
    let json = r#"[{"Id":"c3","Image":"python:3.13-slim","ImageID":"coollabsio/coolify-helper"}]"#;
    assert!(!helper_in_containers(json, "coollabsio/coolify-helper"));
  }

  #[test]
  fn empty_list_is_no_helper() {
    assert!(!helper_in_containers("[]", "coollabsio/coolify-helper"));
    assert!(!helper_in_containers("", "coollabsio/coolify-helper"));
  }
}
