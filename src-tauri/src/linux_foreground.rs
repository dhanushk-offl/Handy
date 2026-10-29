//! Foreground app identifiers on Linux. Wayland deliberately has no universal
//! focused-window API, so use each supported compositor's local IPC protocol.
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const IPC_TIMEOUT: Duration = Duration::from_millis(150);
const MAX_REPLY: u64 = 4 * 1024 * 1024;

fn connect(path: &Path) -> Option<UnixStream> {
    let stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(IPC_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IPC_TIMEOUT)).ok()?;
    Some(stream)
}
fn identifier(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}
fn hyprland(socket: &Path) -> Option<String> {
    let mut stream = connect(socket)?;
    stream.write_all(b"j/activewindow").ok()?;
    let mut bytes = Vec::new();
    stream.take(MAX_REPLY + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_REPLY {
        return None;
    }
    let response: Value = serde_json::from_slice(&bytes).ok()?;
    identifier(response.get("class")?.as_str())
}
fn focused_sway_node(node: &Value) -> Option<String> {
    if node.get("focused").and_then(Value::as_bool) == Some(true) {
        return identifier(node.get("app_id").and_then(Value::as_str))
            .or_else(|| identifier(node.get("window_properties")?.get("class")?.as_str()));
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(Value::as_array) {
            for child in children {
                if let Some(app) = focused_sway_node(child) {
                    return Some(app);
                }
            }
        }
    }
    None
}
fn sway(socket: &Path) -> Option<String> {
    let mut stream = connect(socket)?;
    let mut request = Vec::with_capacity(14);
    request.extend_from_slice(b"i3-ipc");
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.extend_from_slice(&4u32.to_ne_bytes()); // GET_TREE
    stream.write_all(&request).ok()?;
    let mut header = [0u8; 14];
    stream.read_exact(&mut header).ok()?;
    if &header[..6] != b"i3-ipc" || u32::from_ne_bytes(header[10..14].try_into().ok()?) != 4 {
        return None;
    }
    let size = u32::from_ne_bytes(header[6..10].try_into().ok()?) as usize;
    if size as u64 > MAX_REPLY {
        return None;
    }
    let mut body = vec![0; size];
    stream.read_exact(&mut body).ok()?;
    focused_sway_node(&serde_json::from_slice::<Value>(&body).ok()?)
}
fn niri(socket: &Path) -> Option<String> {
    let mut stream = connect(socket)?;
    stream.write_all(b"\"FocusedWindow\"\n").ok()?;
    let mut bytes = Vec::new();
    BufReader::new(stream)
        .take(MAX_REPLY + 1)
        .read_until(b'\n', &mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_REPLY {
        return None;
    }
    let response: Value = serde_json::from_slice(&bytes).ok()?;
    identifier(
        response
            .get("Ok")?
            .get("FocusedWindow")?
            .get("app_id")?
            .as_str(),
    )
}
fn hyprland_socket() -> Option<PathBuf> {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    // The signature is a single component supplied by Hyprland.
    if signature.is_empty() || signature.contains('/') || signature == "." || signature == ".." {
        return None;
    }
    Some(runtime.join("hypr").join(signature).join(".socket.sock"))
}
pub fn foreground_app() -> Option<String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").ok().as_deref() == Some("wayland")
    {
        // Pick the active compositor only; never query XWayland's stale focus.
        if let Some(path) = hyprland_socket() {
            return hyprland(&path);
        }
        if let Some(path) = std::env::var_os("SWAYSOCK") {
            return sway(Path::new(&path));
        }
        if let Some(path) = std::env::var_os("NIRI_SOCKET") {
            return niri(Path::new(&path));
        }
        return None;
    }
    let root = std::process::Command::new("xprop")
        .args(["-root", "_NET_ACTIVE_WINDOW"])
        .output()
        .ok()?;
    if !root.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&root.stdout);
    let id = root.split_whitespace().last()?;
    if id == "0x0" {
        return None;
    }
    let output = std::process::Command::new("xprop")
        .args(["-id", id, "WM_CLASS"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    identifier(text.split('"').nth(3).or_else(|| text.split('"').nth(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn serve(response: &[u8], request_len: usize, test: impl FnOnce(&Path)) {
        let response = response.to_vec();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; request_len];
            stream.read_exact(&mut request).unwrap();
            stream.write_all(&response).unwrap();
        });
        test(&path);
        server.join().unwrap();
    }
    #[test]
    fn hypr_returns_class_and_ignores_title() {
        serve(
            br#"{"class":"kitty","title":"private"}"#,
            b"j/activewindow".len(),
            |p| assert_eq!(hyprland(p).as_deref(), Some("kitty")),
        );
    }
    #[test]
    fn sway_reads_focused_wayland_and_xwayland_apps() {
        let value: Value = serde_json::from_str(r#"{"nodes":[{"focused":false,"app_id":"wrong"},{"floating_nodes":[{"focused":true,"app_id":"org.example.Editor"}]}]}"#).unwrap();
        assert_eq!(
            focused_sway_node(&value).as_deref(),
            Some("org.example.Editor")
        );
        let x: Value = serde_json::from_str(
            r#"{"focused":true,"app_id":null,"window_properties":{"class":"Code"}}"#,
        )
        .unwrap();
        assert_eq!(focused_sway_node(&x).as_deref(), Some("Code"));
        assert_eq!(
            focused_sway_node(&serde_json::json!({"focused": true, "app_id": ""})),
            None
        );
    }
    #[test]
    fn niri_returns_focused_app() {
        serve(b"{\"Ok\":{\"FocusedWindow\":{\"app_id\":\"org.example.Editor\",\"title\":\"private\"}}}\n", b"\"FocusedWindow\"\n".len(), |p| assert_eq!(niri(p).as_deref(), Some("org.example.Editor")));
    }
    #[test]
    fn sway_reads_ipc_response() {
        let body = br#"{"nodes":[{"focused":true,"app_id":"org.example.Editor"}]}"#;
        let mut response = b"i3-ipc".to_vec();
        response.extend_from_slice(&(body.len() as u32).to_ne_bytes());
        response.extend_from_slice(&4u32.to_ne_bytes());
        response.extend_from_slice(body);
        serve(&response, 14, |p| {
            assert_eq!(sway(p).as_deref(), Some("org.example.Editor"))
        });
    }
    #[test]
    fn sway_rejects_invalid_header() {
        serve(b"not-i3-ipc-xxx", 14, |p| assert_eq!(sway(p), None));
    }
}
