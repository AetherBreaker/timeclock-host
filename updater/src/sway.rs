//! sway's control socket: its window tree (is the clock window there, does it show). Standard library only, so a
//! minimal JSON reader comes with it.
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const GET_TREE: u32 = 4;
const MAGIC: &[u8] = b"i3-ipc";

/// A JSON value, as much as sway's tree needs.
#[derive(Debug, PartialEq)]
pub enum Json {
  Null,
  Bool(bool),
  Number(f64),
  Str(String),
  Array(Vec<Json>),
  Object(Vec<(String, Json)>),
}

impl Json {
  pub fn get(&self, key: &str) -> Option<&Json> {
    match self {
      Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
      _ => None,
    }
  }
}

/// Parse a JSON document; `None` if it isn't one.
pub fn parse_json(text: &str) -> Option<Json> {
  let mut p = Parser {
    s: text.as_bytes(),
    i: 0,
  };
  let v = p.value()?;
  p.ws();
  (p.i == p.s.len()).then_some(v)
}

struct Parser<'a> {
  s: &'a [u8],
  i: usize,
}

impl Parser<'_> {
  fn ws(&mut self) {
    while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
      self.i += 1;
    }
  }

  fn eat(&mut self, word: &[u8]) -> Option<()> {
    self.s[self.i..].starts_with(word).then(|| self.i += word.len())
  }

  fn value(&mut self) -> Option<Json> {
    self.ws();
    match *self.s.get(self.i)? {
      b'n' => self.eat(b"null").map(|()| Json::Null),
      b't' => self.eat(b"true").map(|()| Json::Bool(true)),
      b'f' => self.eat(b"false").map(|()| Json::Bool(false)),
      b'"' => self.string().map(Json::Str),
      b'[' => {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.eat(b"]").is_some() {
          return Some(Json::Array(items));
        }
        loop {
          items.push(self.value()?);
          self.ws();
          match *self.s.get(self.i)? {
            b',' => self.i += 1,
            b']' => break self.i += 1,
            _ => return None,
          }
        }
        Some(Json::Array(items))
      }
      b'{' => {
        self.i += 1;
        let mut fields = Vec::new();
        self.ws();
        if self.eat(b"}").is_some() {
          return Some(Json::Object(fields));
        }
        loop {
          self.ws();
          let key = self.string()?;
          self.ws();
          self.eat(b":")?;
          fields.push((key, self.value()?));
          self.ws();
          match *self.s.get(self.i)? {
            b',' => self.i += 1,
            b'}' => break self.i += 1,
            _ => return None,
          }
        }
        Some(Json::Object(fields))
      }
      b'-' | b'0'..=b'9' => {
        let start = self.i;
        while self
          .s
          .get(self.i)
          .is_some_and(|c| matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
          self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i])
          .ok()?
          .parse()
          .ok()
          .map(Json::Number)
      }
      _ => None,
    }
  }

  fn string(&mut self) -> Option<String> {
    self.eat(b"\"")?;
    let mut out = Vec::new();
    loop {
      let c = *self.s.get(self.i)?;
      self.i += 1;
      match c {
        b'"' => return String::from_utf8(out).ok(),
        b'\\' => {
          let e = *self.s.get(self.i)?;
          self.i += 1;
          match e {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'u' => {
              // Window titles may hold any character; one outside the basic plane (a surrogate pair) becomes U+FFFD,
              // which is fine for matching app ids.
              let hex = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
              self.i += 4;
              let ch = char::from_u32(u32::from_str_radix(hex, 16).ok()?).unwrap_or('\u{FFFD}');
              out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            }
            other => out.push(other), // \" \\ \/
          }
        }
        _ => out.push(c),
      }
    }
  }
}

/// The window with `app_id` in sway's tree: `None` if there's none, else whether it shows.
pub fn window_shown(tree: &Json, app_id: &str) -> Option<bool> {
  if tree.get("app_id").is_some_and(|v| *v == Json::Str(app_id.into())) {
    return Some(tree.get("visible") == Some(&Json::Bool(true)));
  }
  ["nodes", "floating_nodes"].iter().find_map(|key| match tree.get(key) {
    Some(Json::Array(children)) => children.iter().find_map(|c| window_shown(c, app_id)),
    _ => None,
  })
}

/// A window's rectangle in sway's layout coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
}

/// The window with `app_id`: `None` if there's none (or its rect is unreadable), else whether it shows and where it is.
pub fn window_rect(tree: &Json, app_id: &str) -> Option<(bool, Rect)> {
  if tree.get("app_id").is_some_and(|v| *v == Json::Str(app_id.into())) {
    let shown = tree.get("visible") == Some(&Json::Bool(true));
    let field = |key: &str| match tree.get("rect")?.get(key)? {
      Json::Number(n) => Some(*n),
      _ => None,
    };
    // `as` saturates, which is fine for on-screen sizes.
    let rect = Rect {
      x: field("x")? as i32,
      y: field("y")? as i32,
      width: field("width")? as u32,
      height: field("height")? as u32,
    };
    return Some((shown, rect));
  }
  ["nodes", "floating_nodes"].iter().find_map(|key| match tree.get(key) {
    Some(Json::Array(children)) => children.iter().find_map(|c| window_rect(c, app_id)),
    _ => None,
  })
}

/// One request over sway's socket: the reply's payload.
fn request(socket: &Path, kind: u32, payload: &str) -> io::Result<String> {
  let mut s = UnixStream::connect(socket)?;
  s.set_read_timeout(Some(Duration::from_secs(2)))?;
  s.set_write_timeout(Some(Duration::from_secs(2)))?;
  let len = u32::try_from(payload.len()).map_err(io::Error::other)?;
  let mut msg = MAGIC.to_vec();
  msg.extend_from_slice(&len.to_ne_bytes());
  msg.extend_from_slice(&kind.to_ne_bytes());
  msg.extend_from_slice(payload.as_bytes());
  s.write_all(&msg)?;
  let mut header = [0u8; 14];
  s.read_exact(&mut header)?;
  if &header[..6] != MAGIC {
    return Err(io::Error::other("not an i3-ipc reply"));
  }
  let len = u32::from_ne_bytes(header[6..10].try_into().expect("4 bytes")) as usize;
  let mut body = vec![0u8; len];
  s.read_exact(&mut body)?;
  String::from_utf8(body).map_err(io::Error::other)
}

/// sway's window tree now; `Err` when sway doesn't answer or the reply isn't JSON.
pub fn get_tree(socket: &Path) -> io::Result<Json> {
  parse_json(&request(socket, GET_TREE, "")?).ok_or_else(|| io::Error::other("unreadable tree"))
}

/// The clock window as sway sees it now; `Err` when sway doesn't answer.
pub fn clock_window(socket: &Path, app_id: &str) -> io::Result<Option<bool>> {
  Ok(window_shown(&get_tree(socket)?, app_id))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Trimmed from sway 1.10's GET_TREE on main-warehouse: the standby tiled on workspace 1, the clock hidden in the
  /// scratchpad.
  const TREE: &str = r#"{"id": 1, "type": "root", "name": "root", "visible": null, "nodes": [
    {"id": 2, "type": "output", "name": "__i3", "nodes": [
      {"id": 3, "type": "workspace", "name": "__i3_scratch", "nodes": [], "floating_nodes": [
        {"id": 9, "type": "floating_con", "name": "Time clock é", "app_id": "local.timeclock.Clock", "pid": 4242,
         "visible": false, "fullscreen_mode": 0, "rect": {"x": 0, "y": 0, "width": 1366, "height": 768}}]}]},
    {"id": 4, "type": "output", "name": "DP-2", "nodes": [
      {"id": 5, "type": "workspace", "name": "1", "nodes": [
        {"id": 6, "type": "con", "name": "Cog", "app_id": "local.timeclock.Standby", "pid": 1234, "visible": true,
         "marks": [], "window_properties": null, "percent": 1.0e0, "border": "none"}], "floating_nodes": []}]}]}"#;

  #[test]
  fn hidden_clock_reads_as_not_shown() {
    let tree = parse_json(TREE).unwrap();
    assert_eq!(window_shown(&tree, "local.timeclock.Clock"), Some(false));
    assert_eq!(window_shown(&tree, "local.timeclock.Standby"), Some(true));
  }

  #[test]
  fn shown_clock_reads_as_shown() {
    let tree = parse_json(&TREE.replace(
      r#""pid": 4242,
         "visible": false"#,
      r#""pid": 4242,
         "visible": true"#,
    ))
    .unwrap();
    assert_eq!(window_shown(&tree, "local.timeclock.Clock"), Some(true));
  }

  #[test]
  fn no_clock_window() {
    let tree = parse_json(TREE).unwrap();
    assert_eq!(window_shown(&tree, "local.timeclock.Other"), None);
    // A prefix of the id isn't the id.
    assert_eq!(window_shown(&tree, "local.timeclock"), None);
  }

  #[test]
  fn window_rect_gives_state_and_geometry() {
    let tree = parse_json(TREE).unwrap();
    let rect = Rect {
      x: 0,
      y: 0,
      width: 1366,
      height: 768,
    };
    assert_eq!(window_rect(&tree, "local.timeclock.Clock"), Some((false, rect)));
    assert_eq!(window_rect(&tree, "local.timeclock.Other"), None);
    // The standby has no rect in the trimmed tree.
    assert_eq!(window_rect(&tree, "local.timeclock.Standby"), None);
  }

  #[test]
  fn json_edge_cases() {
    assert_eq!(parse_json(r#""a\"b\\c\/dA""#), Some(Json::Str("a\"b\\c/dA".into())));
    assert_eq!(parse_json("[]"), Some(Json::Array(vec![])));
    assert_eq!(parse_json("{}"), Some(Json::Object(vec![])));
    assert_eq!(
      parse_json(" [true, null, -1.5e3] "),
      Some(Json::Array(vec![Json::Bool(true), Json::Null, Json::Number(-1500.0)]))
    );
    assert_eq!(parse_json("[1,]"), None);
    assert_eq!(parse_json(r#"{"a": 1"#), None);
    assert_eq!(parse_json("[] x"), None);
    assert_eq!(parse_json(r#""\ud83d""#), Some(Json::Str("\u{FFFD}".into())));
  }
}
